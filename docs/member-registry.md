# Member registry: last-known member names that survive disconnects

**Status: implemented (2026-07-10).** Verified end-to-end by
`crates/seed-core/tests/member_names.rs` (run with `-- --ignored`).

## The problem

A member's display name used to exist only in the in-memory `PeerRoster`, fed
exclusively by presence gossip. Three ways that degraded the member list to
bare endpoint ids (or dropped members entirely):

1. **Doc-sync discovery without presence.** A peer that syncs the replica but
   whose gossip hasn't reached us yet (the asymmetric-delivery case behind
   known-issues #9) sits in the roster nameless.
2. **Daemon restart.** The roster starts empty; members that don't come back
   online simply vanish from the list, and "2 of 5" reads "2 of 2".
3. **Fresh member.** A newly-joined device has never heard anyone; until each
   member broadcasts presence at it directly, it knows nobody's name.

## The design — two complementary layers

### 1. Doc member-records (`\x00m/` control keys) — replicated

Masters publish one tiny record per member into the share's iroh-docs replica:
their own identity every pass, plus any member they can hear **live right now**
via presence (online-only, so a republished name is at most one presence TTL
stale — a stale name re-stamped with a fresh LWW timestamp would beat a
better-informed master's record). Viewers hold a read-only capability and
can't write their own records; a master that hears them writes for them.

The record `{v, id, name, master}` is CBOR-encoded **into the doc key**
(`\x00m/<CBOR>`, content is a 1-byte marker like the `\x00e/`/`\x00t/`
markers). This is load-bearing: every replica disables iroh-docs' content
auto-downloader (`DownloadPolicy::NothingExcept`), so entry *values* never
arrive on their own — that's exactly the trap the replicated ignore list fell
into (known-issues #14). Keys ride doc-sync metadata, so a member's name
reaches every peer that syncs the doc, even one that has never heard a single
gossip message from it.

Per member, the freshest entry timestamp wins. Renames leave superseded keys
behind (~100 bytes each, never deleted — `del` is prefix deletion,
known-issues #13, and can't remove another author's entries anyway); they
simply lose the timestamp comparison forever.

Reading happens early in every reconcile pass (step 1.5); publishing happens
**last, and only once the replica has proven contact with the share's state**
(synced files, tombstones, an ignore entry, or existing member records). Both
are best-effort and timeout-bounded like the other doc reads; registry trouble
never fails a file reconcile.

The publish gate is load-bearing, found the hard way: with the publish inlined
at step 1.5, a co-master's very first pass wrote its own record while its
*initial* doc-sync with the existing members was still in flight. That write
churned the sync session enough that the first merge ran against a still-empty
replica — so the joiner republished its local copies as brand-new entries with
fresh LWW timestamps, resurrecting a concurrent delete exactly as in
known-issues #12 (`multi_master::delete_survives_unseen_master_copy` went from
~10s green on baseline to a reproducible 120s timeout; disabling the write
restored baseline; the gate keeps it green). Genesis costs one pass of delay:
the creator's own replicated ignore entry satisfies the gate from pass 2 on.
The same hazard class exists latently for the ignore-list publish itself
(known-issues #15).

**Trust:** unchanged from presence. Any master-key holder can write any
record (replica entries are signed by the shared namespace key, not
per-device), and presence names were already spoofable by members
(`usability-findings.md` #4). Masters are trusted with folder *content*, so
trusting them with display names adds no new exposure. File content stays
hash-verified.

### 2. The `peer_names` table — local cache

`(share_id, node_id) → (name, role, last_seen, updated)` in `state.db`,
flushed change-driven from the roster on the presence tick (~3s, both daemon
and mobile call `presence_broadcasts()`), with `last_seen` refreshes coalesced
to 5-minute granularity so steady-state presence doesn't rewrite sqlite every
beat. On share open the rows preload the fresh roster, so the member list
names everyone — rendered offline — immediately after a restart, before any
network activity. This also covers the one case the doc registry can't:
viewer-only pools (or a viewer heard only by viewers), where nobody holds a
write capability.

### Merge rules (in `PeerRoster`)

- Live presence is authoritative while a peer is heard: it refreshes the
  remembered identity every beat (`updated = now`).
- A doc record applies only if its timestamp is **newer** than the local
  `updated` — it fills members we've never heard (or heard before a rename we
  were offline for) and can never override fresher first-hand knowledge.
- Remembered identities are display-only: they never drive liveness, download
  providers, mesh rejoin targets, or health episodes. A long-gone member costs
  exactly one named offline row.
- `counts()` total now means "members known", not "members seen since the
  daemon started".

No IPC, GUI, or mobile-facade changes: `PeerInfo.name` simply stops being
`None`, and offline members keep their row. Old peers skip unknown `\x00`
control keys, so the wire format is backward-compatible.

## Aside: can the registry let us ping less often?

Explored while designing this; parked deliberately. Presence currently does
two jobs: **identity** (name/role) and **liveness/health** (online TTL,
percent, manifest fingerprint for divergence detection). The registry fully
takes over identity — a missed beat no longer loses names, and a fresh member
doesn't need to hear everyone before its list is useful.

What it cannot take over is liveness: "still alive" is inherently a heartbeat,
and doc entries are the wrong medium for it (viewers can't write, and stamping
the replica every few seconds per member would churn a permanent, replicated
structure for ephemeral data). So the 3s beat / 20s TTL cadence is now purely
a *liveness-latency* choice, no longer an identity-convergence requirement.
That makes relaxing it (e.g. 5s beat / 30s TTL, halving steady-state gossip)
a legitimate follow-up tuning knob — but cadence changes interact with the
mesh-repair dynamics that took a fleet soak to stabilize (known-issues #9:
rejoin sampling, TTL vs. reconcile-CPU beat delays), so any change should be
soak-verified at fleet scale, not reasoned about. Not changed in this pass.

## Files

- `crates/seed-core/src/engine.rs` — `MEMBER_PREFIX`, `MemberRecord`,
  `read_member_records`, `ReconcileJob::member_registry`, roster `remembered`
  map + merge rules, `Engine::flush_peer_names`, preload in `open_share`.
- `crates/seed-core/src/db.rs` — `peer_names` table (`PeerNameRow`).
- `crates/seed-core/src/presence.rs` — header note (division of labor).
- `crates/seed-core/tests/member_names.rs` — 3-node end-to-end proof.

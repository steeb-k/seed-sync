# Field note: Linux member saw nobody online (relays down)

**Status:** root cause identified — **the configured relays were down.** Restoring
them restored discovery. Kept as a note because the *diagnosis* was slower than it
should have been, and the app gave no signal that the relay set was unreachable.

## What happened

2026-07-24. A Linux member of a share whose other members are Windows boxes showed
no one online. Restarting the user service changed nothing. The same machine was
concurrently running Nullgate (also iroh 1.0) over the internet to one of those same
Windows boxes, and that worked — which is what made it look like a SEED Sync bug
rather than an infrastructure outage. It wasn't: Nullgate has its own relay
configuration, so it proved the host could reach *a* relay, not that SEED Sync's
configured relay set was up.

Relays back up → members visible again.

## Why this was hard to see, and what to do about it

This is the part worth keeping. The failure was in the infrastructure, but the app
made it indistinguishable from a bug in itself:

- **No user-visible "your relays are unreachable" state.** `RelayPolicy::Preferred`
  is documented as "use the custom relays; fall back to the public relays only while
  none of the custom ones is reachable" — so a down custom relay is *supposed* to
  degrade gracefully. It did not visibly do so here. Whether the fallback fired at
  all is worth confirming; `relays.rs` has a custom-relay fallback watchdog
  (`probe_relay`, and the fallback path around the `PreferredRelays` selector) that
  should be exercised against a black-holed relay, not just a refusing one. A relay
  that accepts TCP but never completes is a different failure from one that is down,
  and the watchdog needs to catch both.
- **`seed-cli relays` reports configuration, not reachability.** It prints the
  configured servers and the mode. It does not say whether any of them currently
  answers. A `seed-cli relay-test` exists (`TestRelay` IPC) but has to be invoked
  deliberately and per-URL; nothing surfaces "none of your relays is up" on its own.
- **"No peers online" is the same string for every cause.** Relay outage, partition,
  a dead presence overlay and a cold roster all present identically. Known-issues #17
  fixed the *worst* version of this (a partitioned node claiming `Healthy 100%`), but
  the member list still can't tell you *why* it's empty.

### Suggested follow-ups (not yet implemented)

1. Surface relay reachability in the GUI and in `seed-cli relays` — probe the
   configured relays periodically and show last-successful-contact per relay.
2. Log a WARN when every configured custom relay has been unreachable for some
   window, and say explicitly whether the public-relay fallback engaged.
3. Add a test that black-holes a custom relay (accepts, never responds) and asserts
   the `Preferred` fallback actually reaches the public relays — the graceful
   degradation is currently only asserted against a relay that refuses.

## Still open: two members on the same LAN not syncing

Reported immediately after the relays were restored. **This turned out to be a
different bug entirely, not a connectivity problem** — see known-issues #30. On the
Windows master (`bigDev`) all three members were online with `path=direct` and all
reported `Healthy 100%`, while the master's own `sync_index` base hash for
`WinRx_11_25H2.iso` (`52f91659…`) differed from the file actually on disk
(`63c6426c…`), and the blob store had never seen the on-disk content. The fleet was
consistent and wrong. Diagnosis and fix are in known-issues #30; the diagnostics used
are `crates/seed-core/examples/hashfile.rs` and `examples/showindex.rs`.

Two members being on the same LAN does mean relays are *not* required for them to
find each other (`iroh-mdns-address-lookup` handles local discovery), which is
consistent with what was observed: they were connected the whole time.

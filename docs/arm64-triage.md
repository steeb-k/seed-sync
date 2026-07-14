# SEED Sync on Windows ARM64 — triage brief

You are picking this up **on the ARM64 Windows machine itself**. A previous session built the ARM64
port from an x86_64 box and cannot reproduce this locally; you can. Read this whole file before
touching anything.

You are in the source tree (`main`, at or after tag `v0.6.1`), so every `file:line` reference below is
something you can open right now. The installed binaries under `C:\Program Files\SeedSync\bin` were
built from this exact commit.

---

## 1. The symptom

This machine runs **SEED Sync 0.6.1, native ARM64** (installed from
`seed-sync-0.6.1-windows-arm64.msi`, a signed pre-release).

It joins a share successfully, then:

- It sees exactly **one** other member. Never the other four.
- That member has **no name**, is labelled **"Viewer"**, and shows as **unhealthy**.
- The share reports **Healthy 100%**.

The pool has **5 members total**; the other 4 are **x86_64, running 0.6.0**, and are healthy with
each other. This ARM box is the only one that misbehaves.

### What the symptom actually means (don't take the UI at face value)

- **"Viewer" is not a role here, it's a fallback for *unknown*.** `crates/seed-core/src/engine.rs`
  (~line 311) builds `PeerInfo.role` as
  `e.role.or_else(|| m.map(|m| role_from_master(m.master))).unwrap_or(seed_ipc::Role::Viewer)`.
  A peer with no presence and no member-record therefore renders as a nameless "Viewer". So the one
  entry means: **"I have made contact with a peer and learned nothing whatsoever about it."**
- **No names anywhere ⇒ the iroh-docs replica is not syncing.** Per `docs/member-registry.md`,
  masters publish member records as CBOR **inside the doc key** (`\x00m/<CBOR>`), specifically so the
  data rides doc-sync *metadata* and reaches any peer that syncs the doc "even one that has never
  heard a single gossip message." Zero names ⇒ **zero doc sync**.
- **"Healthy 100%" is vacuous.** The health calc (`engine.rs` ~3064) filters on
  `p.online && p.is_master && p.manifest_fp != 0`. It sees **no online masters**, so there is nothing
  to be unhealthy against. It is 100% of an empty set, not a clean bill of health.

So the real statement of the bug is: **this node has essentially no working peer connectivity and no
document sync**, while every screen claims it's fine.

---

## 2. What has already been ruled out — do not re-litigate these

- **Not the Viewer role, and not cached state.** First join used a *viewer* key. It was then removed
  and re-added with a **master key** pointed at a **fresh, different folder**. Identical symptom.
- **Not a code change in 0.6.1.** The 0.6.0 → 0.6.1 bump changed only: workspace version, build
  scripts, `build.rs`, the WiX file, the updater script and docs. `cargo update --workspace` reported
  **"101 unchanged dependencies"** — no dependency moved. The ARM64 0.6.1 binary is functionally the
  **same code** the four healthy x86_64 0.6.0 peers are running.
- **Not "ARM64 can't do iroh/GTK".** **Nullgate 0.4.1 ARM64 works perfectly on this same machine** —
  it joins its network, sees all peers, and routes traffic (including its wintun kernel driver). It
  uses iroh, iroh-docs, iroh-gossip, QUIC, hole-punching, blake3 and ring on this box, all fine.

**Therefore the cause is arch/ABI-specific or environment-specific — not a logic bug in the app.**

---

## 3. The two things SEED Sync has that the working control (Nullgate) does not

This is the highest-value part of this brief. Nullgate is a near-identical app by the same author
(same crate shape, same GTK4 GUI, same iroh stack) and it works on this machine. Diffing the two dep
trees leaves exactly two suspects:

1. **SEED Sync vendors *patched* iroh.** `Cargo.toml` has:
   ```toml
   [patch.crates-io]
   iroh-blobs = { path = "vendor/iroh-blobs" }
   iroh       = { path = "vendor/iroh" }
   iroh-docs  = { path = "vendor/iroh-docs" }
   ```
   Nullgate uses the **published** crates. So Nullgate's success proves nothing about *these*
   forks. **Diff `vendor/` against the upstream crates** and look for anything arch-sensitive, and
   for anything touching relays, discovery, or the docs sync session (the vendored patch exists for
   `docs/known-issues.md` #11, an unbounded path-open retry queue).

2. **SEED Sync links SQLite; Nullgate does not.** `rusqlite` + `libsqlite3-sys` (the **bundled** C
   build) are in seed-sync's lockfile and absent from Nullgate's. That is a C library compiled for
   `aarch64-pc-windows-msvc` — a native code path that has *never* run on ARM in this project. If the
   store is silently failing, doc sync cannot complete, which is exactly what we observe. Note
   `CLAUDE.md` already flags this crate as fragile on Windows ("`libsqlite3-sys`'s bundled Windows
   build uses `cfg_select!`").

**Check the daemon log first — a store/DB failure will be screaming in it.**

---

## 4. Environment facts you need

| | |
|---|---|
| Install dir | `C:\Program Files\SeedSync\bin` |
| Data dir | `C:\ProgramData\SeedSync` |
| Daemon log | `C:\ProgramData\SeedSync\daemon.log` (the SCM-run service has no console, so it logs here) |
| IPC socket | `C:\ProgramData\SeedSync\seed.sock` |
| Service name | `SeedSyncDaemon` (LocalSystem, auto-start) |
| Binaries | `seed-daemon.exe` (ARM64/MSVC), `seed-gui.exe` (ARM64/**gnullvm**), `seed-cli.exe` (ARM64/MSVC) |

**The ARM64 build is deliberately two ABIs.** The GUI is `aarch64-pc-windows-gnullvm` (mingw ABI,
because the only prebuilt GTK4+libadwaita for Windows-on-ARM is MSYS2's CLANGARM64); the daemon and
CLI are `aarch64-pc-windows-msvc`. They are separate processes that only meet over the IPC socket, so
no ABI boundary is crossed inside a process. **This is not the bug — do not go "fix" it.** All the
networking lives in the MSVC daemon. See `docs/windows-packaging.md`.

**`seed-cli` quirk:** `--socket` is a **required** top-level arg with no default, and being a parent
arg it must come **before** the subcommand:

```powershell
cd "C:\Program Files\SeedSync\bin"
$sock = "C:\ProgramData\SeedSync\seed.sock"
.\seed-cli.exe --socket $sock list
.\seed-cli.exe --socket $sock peers --share <SHARE_ID>
.\seed-cli.exe --socket $sock relays
.\seed-cli.exe --socket $sock relay-test --url <URL> --token <TOKEN>
```

---

## 5. Diagnostic plan (in order — stop as soon as something looks wrong)

### Step 1 — read the log. Do this before anything else.
```powershell
Get-Content C:\ProgramData\SeedSync\daemon.log -Tail 200
Select-String -Path C:\ProgramData\SeedSync\daemon.log -Pattern "error|panic|failed|sqlite|store|denied|refused" -Context 2,2
```
A bundled-SQLite or store failure, or a vendored-iroh panic, shows up here and would end the
investigation immediately.

### Step 2 — get a verbose foreground daemon.
The service holds the default data dir and socket, so **stop it first**, then run a foreground daemon
with full tracing (it logs to stderr in `run` mode; `RUST_LOG` is honoured via `EnvFilter`, default
`seed_daemon=info,seed_core=info`):

```powershell
Stop-Service SeedSyncDaemon
$env:RUST_LOG = "seed_daemon=debug,seed_core=debug,iroh=debug,iroh_docs=debug,iroh_gossip=debug,iroh_relay=debug"
.\seed-daemon.exe run 2>&1 | Tee-Object C:\Users\Public\seed-verbose.log
```
Then, in a second shell, add the share and watch. Look specifically for:
- Does it **connect to any peers at all**, and how many?
- Does a **docs sync session** start, and does it complete or error?
- Does **gossip** subscribe to the topic and receive anything?
- Which **relay** does it home onto?

(You can also run a fully isolated instance with `--data-dir C:\Users\Public\seed-test`
`--socket C:\Users\Public\seed-test\seed.sock` without disturbing the installed service's state.)

### Step 3 — relays. This is per-device and per-app.
Relay settings are **per-device**, are **not** distributed through the share, and Nullgate's relay
config has nothing to do with SEED Sync's. The other 4 members may be homed on a custom (possibly
token-gated) relay this box cannot reach — which partitions this node while everything reports
healthy.

```powershell
.\seed-cli.exe --socket $sock relays        # is a custom relay set, and is mode `only` or `preferred`?
.\seed-cli.exe --socket $sock relay-test --url <URL> --token <TOKEN>
```
**Known trap:** a relay-map change **does not evict the home relay you are already on**. iroh
advertises exactly one home relay and only ever *moves* it; a live edit therefore looks applied while
the node stays put. **Always restart the daemon after changing relay settings**, and re-check. Also
beware `mode = only` pointed at an unreachable relay: it leaves the node stranded on whatever it had,
the opposite of what `only` promises.

Compare against a healthy x86_64 peer: run `seed-cli relays` there and make sure this box's config
matches (URL **and** token).

### Step 4 — Windows Firewall.
The daemon runs as a **LocalSystem service**, so it never raises the interactive "allow this app?"
prompt — inbound is just silently dropped. A fresh install of a *new binary at a new path* on a fresh
machine is exactly when this bites. Check for a rule, and note that Nullgate is not a valid control
here (different exe, possibly already allowed):
```powershell
Get-NetFirewallApplicationFilter -Program "C:\Program Files\SeedSync\bin\seed-daemon.exe" |
  Get-NetFirewallRule | Select-Object DisplayName, Direction, Action, Enabled
```
Inbound UDP being blocked would leave the node reliant on relays and could plausibly produce exactly
one half-formed peer contact.

### Step 5 — the decisive arch experiment (only if 1–4 come up clean).
The one variable never isolated: **is this ARM, or is it this machine/network?**

Install **x86_64 SEED Sync 0.6.0** on *this same ARM machine* (it runs fine under Windows'
x64 emulation — SEED Sync ships no kernel driver, unlike Nullgate) and join the same share.

- **Emulated x64 works, native ARM64 doesn't** ⇒ the bug is in the **ARM64 build** (go hard at the
  vendored iroh forks and bundled SQLite, §3).
- **Both fail** ⇒ it's this **machine/network** (firewall, relay, NAT), not the ARM port at all.

This single test cleanly separates the two worlds and is worth doing before any deep code work.

---

## 6. Building on this machine, if you need to

You probably don't — diagnose with the installed binaries first. If you do need a native build here,
note that MSYS2's **CLANGARM64** environment runs natively on this box, so unlike the x86_64 build
host you can just `pacman -S mingw-w64-clang-aarch64-gtk4 mingw-w64-clang-aarch64-libadwaita` and
build the GUI normally. The daemon needs no GTK at all — `cargo build -p seed-daemon` against the
MSVC toolchain is enough, and **the daemon is where the bug is**, so that is the cheap path to a
debug build.

---

## 7. Rules of engagement

- The other 4 members are a **live, working pool**. Do not run destructive experiments against the
  share. Prefer an isolated `--data-dir` instance.
- **Do not commit or push** from this machine without asking. If you change code, report the diff.
- Report back: the daemon log, `seed-cli peers`/`relays` output, which of §5's steps fired, and the
  §5.5 emulated-x64 result if you got that far.
- The answer we most need is binary: **is this the ARM64 build, or is it this machine's network?**

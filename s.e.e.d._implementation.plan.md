S.E.E.D. Implementation Plan

This plan breaks your app-plan.md into ordered phases. Each phase delivers something testable before moving on.



Tech stack







Layer



Choice



Notes





Runtime



.NET 8 (LTS)



WinUI 3 and MonoTorrent support it.





BitTorrent



MonoTorrent 2.0.x



Mature, NuGet, DHT/PEX; use stable 2.0.7.





Windows UI



WinUI 3 (Windows App SDK)



Packaged C# desktop app; acrylic/mica as you specified.





License



GPLv3



Per your requirement.

Solution layout: SeedSync.sln with projects:





SeedSync.Core — share model, key format, ignore rules, sync logic (MonoTorrent usage). No UI.



SeedSync.Daemon — long-running engine; exposes local API (e.g. HTTP or named pipes) for clients.



SeedSync.Cli — terminal app: create share, add share, list shares, status. Validates core + daemon.



SeedSync.App — WinUI 3 desktop app; talks to daemon, implements your GUI and toasts.

Windows behavior (Phase 5):





System tray: The app lives in the system tray (notification area). Main window opens from tray icon; closing the window does not quit the app — it minimizes to tray. Quit only via tray context menu (or similar).



Start with Windows: By default, S.E.E.D. starts when the user logs in (e.g. registry Run key or Startup folder for packaged app). User can turn this off in settings.



Architecture (high level)

flowchart LR
  subgraph clients [Clients]
    WinUI[SeedSync.App WinUI]
    CLI[SeedSync.Cli]
  end
  subgraph engine [Engine]
    Daemon[SeedSync.Daemon]
    Core[SeedSync.Core]
  end
  WinUI --> API
  CLI --> API
  API[Local API] --> Daemon
  Daemon --> Core
  Core --> MT[MonoTorrent]
  MT --> P2P[P2P Network]





Core: “What is a share?” (folder path, RW/RO key, default path, ignore list), key generation, and sync behavior (which files, who can write).



Daemon: Runs Core, manages multiple shares, listens for peers (DHT/tracker/PEX), handles file I/O and conflict policy (e.g. last-write-wins for “identical at all times”).



Cli / App: Same API against the daemon; only presentation differs.



Phase 1: Core and single-share sync

Goal: Two processes can keep one folder in sync using a shared secret, without any UI.





Solution and Core project





Create .NET 8 solution with SeedSync.Core class library.



Add MonoTorrent 2.0.7 to Core.



Define share model: ShareId, local path, RW key, RO key, optional default path and ignore list.



Key design: Two independent secrets. RW key — created once when creating a new share; cryptographically random; never derivable from the RO key so end-users cannot guess it. RO key — also random (or derived from share metadata that does not reveal RW); shareable with read-only users. Both keys are long, unguessable strings; the protocol identifies key type (RW vs RO) when joining so the engine knows whether to allow uploads. No prefix or pattern that would let someone infer the RW key from the RO key.



Minimal sync over BitTorrent





Use MonoTorrent to create a “torrent” for one folder (or a manifest of files) and join the same “swarm” with a shared info hash or magnet derived from the share key.



Implement: one folder → list of files (respecting ignore list) → create/load torrent → start engine, announce to DHT (and optionally a tracker).



Get two instances (e.g. two console apps or two runs of the same test host) to discover each other and sync the folder. No RW/RO enforcement yet; focus on “same content.”



Deliverable: A small test or CLI that creates a share for a folder and runs the engine; a second run with the same key syncs that folder. Proves Core + MonoTorrent wiring.



Phase 2: Keys, access control, and ignore list

Goal: RW vs RO enforced; creator can set default path and ignore list; keys are clearly distinguishable.





Key and access rules





Key generation: when creating a new share, generate two separate secrets: one RW key (full read/write; keep secret, only share with trusted users) and one RO key (read-only; safe to distribute). RW key must not be derivable from the RO key — use independent CSPRNG output for each so no one who has only the RO key can guess the RW key.



Engine: peers that join with the RO key never upload changes (only download); peers that join with the RW key can add/change/delete files. Protocol identifies which key type was used at join (e.g. in metadata or a custom extension) so the engine enforces role.



Default path and ignore list





In share metadata (or in a small “manifest” file in the swarm): default save path for new clients, and ignore rules (e.g. glob or .gitignore-style).



Core: when building the file list for the torrent, apply ignore list; when saving “add existing share” info, store default path and allow override.



Deliverable: Same two-node test, but one node RO: RO cannot alter files; RW can. Ignore list and default path stored and applied.



Phase 3: Daemon and local API

Goal: Sync runs as a background process; clients talk to it over a local API.





Daemon project





New project SeedSync.Daemon (e.g. console or worker host) that references Core.



Host the sync engine: load/save shares from disk (e.g. JSON or SQLite in AppData), start/stop MonoTorrent per share, handle file changes (inotify or polling) to re-sync.



Local API





Expose “list shares”, “create share”, “add share (by key)”, “remove share”, “get status” (syncing, up to date, errors). Use HTTP (e.g. Kestrel on localhost) or named pipes; keep it simple.



Authentication: optional for v1 (localhost-only); later add token or OS user check if needed.



Deliverable: Daemon runs and persists shares; API can create/add/list and report status. Still no UI beyond CLI.



Phase 4: Terminal client (CLI)

Goal: Validate full flow from a user’s perspective without WinUI.





CLI project





SeedSync.Cli that calls the daemon API: seed create <path>, seed add <key> [--path ...], seed list, seed status [share-id].



When adding a share: show default path from key/metadata, allow override; if key is RW, print a clear warning that changes affect all users.



Deliverable: User can create a share, get RW/RO keys, add that share on another machine (or same machine, different folder), and see sync via CLI. Confirms key UX and daemon behavior.



Phase 5: WinUI app (main UI)

Goal: Your described GUI on Windows, with acrylic/mica and toasts.





App project





WinUI 3 (Packaged) app, .NET 8; references daemon API client (or in-process engine if you prefer “single process” for first release).



Tray: App runs from system tray. Tray icon always visible when the daemon/engine is running; main window opens on tray icon click (or “Open S.E.E.D.” from context menu). Closing the main window minimizes to tray (does not exit). Explicit “Quit” in tray context menu exits the app.



Start at login: By default, S.E.E.D. is registered to start when the user logs in (e.g. HKCU\Software\Microsoft\Windows\CurrentVersion\Run or packaged app startup task). Provide a setting to disable “Start S.E.E.D. when I sign in.”



Main window: “+” top-left with two actions: Create new share | Add existing share. Below: list of active shares (name/path/status).



Create new share





Folder picker → select folder.



Set default directory for clients (pre-filled, editable).



Ignore list: simple text box or “add pattern” (e.g. .git, node_modules).



Generate and show RW and RO keys (both long random secrets; clearly labeled “Read/Write (secret)” and “Read-only”). Copy buttons. UI should emphasize that the RW key is secret and must not be shared with untrusted users.



Add existing share





Text box for key; on paste/Go, resolve key (e.g. fetch metadata from DHT or from key payload) and show default save location; user can change it.



If key is RW: show prominent warning that changes will affect all users.



On confirm, call daemon API to add share and start sync.



Polish





Acrylic/mica on window and relevant panels.



Toast notifications: successful first sync; errors (e.g. “Sync failed for Share X”).



Deliverable: Full Windows UX as in your plan; all flows go through the same Core/Daemon as the CLI.



Phase 6: Polish and stretch (later)





Resilience: You said “NOT backup” — keep conflict policy simple (e.g. last-write-wins), no heavy versioning.



Linux/macOS: Daemon and Core can stay .NET; add GTK (Linux) and native macOS UI as separate clients to the same API or protocol later.



Suggested first steps (Phase 1 in detail)





Create SeedSync.sln and SeedSync.Core class library (.NET 8).



Add MonoTorrent and implement Share model + key generation (two independent secrets: RW and RO, no derivation between them).



In Core, implement “create torrent for folder” and “start engine with key” so two processes with the same key form a swarm.



Add a minimal console or unit test that runs two engines and asserts one folder mirrors the other after a change.

Once Phase 1 works, Phase 2 (access control + ignore list) and Phase 3 (daemon + API) can proceed in order; then CLI and WinUI can be built in parallel if desired, both against the same API.
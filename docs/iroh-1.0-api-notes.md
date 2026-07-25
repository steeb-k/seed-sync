# iroh 1.0 stack — API notes (verified 2026-06-17, re-verified 2026-07-24)

Quick reference for the exact API we build against. Versions: **iroh 1.0.3,
iroh-blobs 0.103.0, iroh-docs 0.101.0, iroh-gossip 0.101.0, iroh-tickets 1.0.0**.
The protocol crates (blobs/docs/gossip) stay on 0.10x even though they target
iroh 1.0 — this skew is expected.

## 1.0.0 → 1.0.3 (bumped 2026-07-24)

Only `iroh` moved; every other crate above was already at its newest published
version. **Our API surface is unchanged** — the whole 1.0.0→1.0.3 diff was
re-verified file by file, and both semver-exempt features we depend on came
through clean:

- `unstable-custom-transports` — the public `PathSelector` / `PathSelection` /
  `PathSelectionContext` / `FourTuple` surface is byte-identical. `custom.rs`
  only gained a `Display` impl for `Box<dyn CustomEndpoint>`. `relays.rs` needed
  no changes.
- `unstable-net-report` — `net_report.rs` changed only `warn_span!` →
  `info_span!`. `ep.net_report()` unchanged.

Behaviour changes that matter to us:

- **Transport-lane fairness (1.0.2, iroh#4384).** 1.0.0's
  `inner_poll_recv` did `let counter = self.poll_recv_counter.wrapping_add(1)`
  and never stored the result, so the counter was pinned at 0 and the polling
  order never alternated. We register no custom transports, so our fixed order
  was **relay before IP, on every poll** — and the poll macro returns on the
  first lane with data, so sustained relay traffic could starve the direct-UDP
  lane. Now genuinely alternates.
- **Windows transient recv errors against a dead relay (1.0.2, iroh#4348 +
  net-tools#166).** On Windows a QAD probe to an unreachable relay draws ICMP
  port-unreachable, which the socket reports as a recv error on the *next*
  recv. Those count toward `MAX_CONSECUTIVE_RECV_ERRORS`, and hitting the cap
  tears the QUIC endpoint down with `NetworkDown`. Fixed in `noq-udp` 1.1.0 /
  `netwatch` 0.19.1, which this bump pulls in. Directly relevant to us: Windows
  is our primary platform and we run a custom relay that has gone down before
  (see `relay-outage-field-note.md`).
- **`PkarrResolver` added to the `N0` preset (1.0.3, iroh#4412).** 1.0.0
  published to pkarr but, outside browsers, resolved only via n0 DNS. 1.0.3
  resolves directly from the pkarr relay as well. `node.rs` uses `presets::N0`,
  so this lands on us for free and should tighten cold-join via the share-key
  pkarr rendezvous (known-issues #16) — no DNS propagation/TTL wait.
- **Empty ALPN now errors** with `ConnectWithOptsError::InvalidAlpn` (new enum
  variant). We never match on that type and always pass a real ALPN.
- **Keep-alive docs corrected (1.0.1, iroh#4352):** the default is **5 s**, not
  `None`/disabled as 1.0.0's docs claimed. Behaviour did not change — only the
  documentation was wrong.
- **`PortmapperConfig::Disabled`** is now documented as the way to skip UPnP
  SSDP multicast discovery, which raises firewall dialogs (notably on macOS).
  We do not set it; worth remembering if macOS users report a firewall prompt.

**Watch out — log levels dropped.** iroh#4378 moved the transports recv-error
from `warn!` to `debug!`, and net-report / reportgen / pkarr spans from
`warn_span!`/`error_span!` to `info_span!`. Our whole relay-outage follow-up was
about making an outage self-evident in logs, so re-check the daemon's tracing
filter still surfaces what `relays.rs` needs before trusting a quiet log.

**Still patched, still required:** `vendor/iroh` carries the
`pending_open_paths` dedup+cap (known-issues #9,
[iroh#4390](https://github.com/n0-computer/iroh/issues/4390) — open as of 1.0.3).
`remote_state.rs` was untouched between 1.0.0 and 1.0.3. See `vendor/README.md`.

**Bump gotcha:** `[patch.crates-io]` is silently ignored when the patched
version differs from what `Cargo.lock` pins — cargo emits only a *warning*
(`patch ... was not used in the crate graph`) and builds green against the
stock, unpatched crate. After re-vendoring at a new version you must run
`cargo update -p iroh@<OLD> --precise <NEW>` and then confirm with
`cargo tree -i iroh` that the path entry is in the graph.

## Big 1.0 renames
- `NodeId` → `EndpointId` (alias for `PublicKey`); `NodeAddr` → `EndpointAddr`.
- `endpoint.node_id()`/`node_addr()` → `endpoint.id()`/`endpoint.addr()`.
- Remote peer on a connection: `connection.remote_id()`.
- `Endpoint::builder().discovery_n0().bind()` (old) → `Endpoint::bind(presets::N0)`
  (preset is **mandatory**; discovery + relays come from the preset).
- `Router::...spawn()` is **synchronous** now (no `.await`).
- blobs/docs `net`/`engine` features are gone — networking is always compiled.
- Entry values are NOT in the doc replica — read bytes from the blobs store via
  `entry.content_hash()`.

## Endpoint
```rust
use iroh::{Endpoint, SecretKey, endpoint::presets};
let ep = Endpoint::builder(presets::N0).secret_key(sk).bind().await?; // stable id
// or: Endpoint::bind(presets::N0).await?   (random key)
ep.id();    // EndpointId  (== sk public key)
ep.addr();  // EndpointAddr
```
Presets: `N0` (n0 discovery + relays), `N0DisableRelay`, `Minimal`, `Empty`.
`SecretKey::from_bytes(&[u8;32])` / `to_bytes()` / `generate(rng)` (byte-method
names unverified — confirm on docs.rs/iroh-base).

### Local-network (LAN) discovery — mDNS
On top of `presets::N0` we add mDNS-based local discovery so peers on the same
LAN find each other with **no internet** (no n0 DNS, no relay). Discovery in 1.0
is the "Address Lookup" system; mDNS lives in a **separate crate**,
`iroh-mdns-address-lookup` (0.4, depends on iroh 1.0.0), and is attached via
`Builder::address_lookup(..)`:
```rust
let endpoint_id = secret_key.public();
let mut builder = Endpoint::builder(presets::N0).secret_key(secret_key);
// Build defensively: it errors if the host has no usable IPv4/IPv6 — degrade to
// "no LAN discovery" rather than failing endpoint bind.
match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint_id) {
    Ok(mdns) => builder = builder.address_lookup(mdns),
    Err(e) => tracing::warn!("local-network (mDNS) discovery unavailable: {e}"),
}
let ep = builder.bind().await?;
```
Lives in `crates/seed-core/src/node.rs`. n0 DNS + mDNS run side by side: n0 for
remote peers, mDNS for same-segment ones. **Android caveat:** inbound multicast
is dropped unless the app holds a `WifiManager.MulticastLock` (acquired in
`EngineService`, needs the `CHANGE_WIFI_MULTICAST_STATE` permission).

### Custom relays (verified 2026-07-10 against a live self-hosted relay)
The `N0` preset supplies the public n0 relay map. A user's own relay (with an
optional access token) replaces it via `RelayMode::Custom`; live map edits need
no rebind. Lives in `crates/seed-core/src/relays.rs` + `node.rs`.
```rust
use iroh::{RelayConfig, RelayMap, RelayMode, RelayUrl};
let url: RelayUrl = "https://relay.example.com:8443".parse()?;
let mut cfg = RelayConfig::from(url);          // cfg.quic: Some(port 7842) by default
cfg = cfg.with_auth_token(token);              // sent as `Authorization: Bearer <token>`
                                               // on the relay handshake (native targets)
let ep = Endpoint::builder(presets::N0)
    .relay_mode(RelayMode::Custom(RelayMap::from_iter(configs)))  // replaces the defaults
    .bind().await?;
// Live edits (no rebind); insert before remove so the map is never empty:
ep.insert_relay(url.clone(), std::sync::Arc::new(cfg)).await;
ep.remove_relay(&url).await;
ep.home_relay_status();                        // Watcher; .get() -> Vec<RelayStatus>
                                               //   (s.is_connected(), s.url())
iroh::endpoint::default_relay_mode().relay_map().relays()  // the public set
                                               // (semver-exempt; re-verify on bumps)
```
- Token constraint: header-safe, i.e. non-empty printable ASCII without spaces
  (we validate 0x21–0x7e up front).
- A token-protected relay rejects token-less clients with "relay denied our
  authentication"; the endpoint just never comes online through it.
- The token gates **only the relay connection** (the WS path carrying relayed
  traffic + hole-punch coordination). The relay's QUIC address-discovery
  service (UDP 7842) and HTTPS latency endpoint are unauthenticated by design
  (verified against iroh-relay 1.0.0 source + a live token-protected relay), so
  token-less clients still get STUN-like public-address discovery. Net-report
  `relay_latency` entries only appear for probes that got an answer — usable to
  distinguish "server up, relay connection refused" from "server unreachable".
- `builder.path_selector(Arc<dyn PathSelector>)` (feature
  **`unstable-custom-transports`**) customizes path choice — installed once at
  bind, can't be swapped later, so our `PreferMyRelaySelector` reads a shared
  live set instead. Types under `iroh::endpoint::transports`:
  `PathSelector`, `PathSelection(Context/Data)`, `FourTuple`.
- Dev-only feature **`unstable-net-report`**: `ep.net_report()` Watcher; `Qad*`
  entries in `report.relay_latency` prove the relay's QUIC address-discovery
  (UDP 7842) works. Used by `examples/relay_probe.rs`.

## Router
```rust
use iroh::protocol::Router;
let router = Router::builder(ep)
    .accept(iroh_blobs::ALPN, blobs_handler)
    .accept(iroh_gossip::ALPN, gossip)
    .accept(iroh_docs::ALPN, docs)
    .spawn();                       // sync
router.shutdown().await?;
```

## iroh-blobs 0.103
```rust
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, Hash};
let store = MemStore::new();                       // or FsStore::load(path).await? (feature fs-store)
let handler = BlobsProtocol::new(&store, None);
let tag  = store.blobs().add_bytes(b"hi".to_vec()).await?;  let h = tag.hash();
let tag  = store.blobs().add_path(abs_path).await?;          // import a file
store.blobs().export(h, abs_out_path).await?;                // export to disk
store.blobs().has(h).await? -> bool;
store.blobs().get_bytes(h).await? -> Bytes;
// ALPN = iroh_blobs::ALPN (= b"/iroh-bytes/4")
```

## iroh-docs 0.101 (multi-writer KV)
Built on blobs + gossip; all three register on the same Router.
```rust
use iroh_docs::{protocol::Docs, api::protocol::{ShareMode, AddrInfoOptions}, store::Query};
let docs = Docs::memory().spawn(ep.clone(), (*blobs).clone(), gossip.clone()).await?;
let api  = docs.api();                       // also Derefs to DocsApi
let author = api.author_create().await?;     // AuthorId  (author_default() also exists)
let doc    = api.create().await?;            // new namespace; doc.id() -> NamespaceId
doc.set_bytes(author, key_bytes, value_bytes).await? -> Hash;
let e = doc.get_one(Query::single_latest_per_key().key_exact(key)).await?; // Option<Entry>
let mut s = doc.get_many(Query::all()).await?;            // Stream<Result<Entry>>
doc.del(author, prefix).await? -> usize;
// value bytes: blobs.get_bytes(entry.content_hash()).await?
// entry: .key() .author() .timestamp() .content_hash() .content_len()
```
Share / join / sync:
```rust
let ticket = doc.share(ShareMode::Write, AddrInfoOptions::default()).await?; // DocTicket
let doc = api.import(ticket).await?;                          // join
let (doc, mut events) = api.import_and_subscribe(ticket).await?;
doc.start_sync(vec![endpoint_addr]).await?;                  // add peers to open doc
let mut events = doc.subscribe().await?;                     // Stream<Result<LiveEvent>>
```
`LiveEvent`: `InsertLocal{entry}`, `InsertRemote{from,entry,content_status}`,
`ContentReady{hash}` (gate reads on this), `NeighborUp/Down`, `SyncFinished`.
`ContentStatus`: `Complete|Incomplete|Missing`.

Router accept side is automatic; you must INITIATE via share→import or start_sync.

## iroh-gossip 0.101
```rust
use iroh_gossip::{ALPN as GOSSIP_ALPN, net::Gossip, proto::TopicId, api::Event};
let gossip = Gossip::builder().spawn(ep.clone());            // register under GOSSIP_ALPN
let topic = TopicId::from_bytes([0u8;32]);
let (tx, mut rx) = gossip.subscribe(topic, bootstrap_ids).await?.split(); // or subscribe_and_join
tx.broadcast(bytes).await?;
// rx: Stream<Result<Event>>; Event::Received(Message{content, delivered_from, scope})
```
Bootstrap peers are `Vec<EndpointId>`.

## Tickets
- `iroh_blobs::ticket::BlobTicket::new(addr, hash, format)` — `Display`/`FromStr`.
- `iroh_docs::DocTicket { capability, nodes }` — produced by `doc.share(..)`;
  `Display`/`FromStr`. Self-contained (content id + addrs); no central server.
- `iroh-tickets` provides the `Ticket` trait + `EndpointTicket`.

## Required features
```toml
iroh        = "1"      # no extra features
iroh-blobs  = "0.103"  # defaults (fs-store, rpc) give handler + local store API
iroh-docs   = "0.101"  # defaults (rpc, fs-store); rpc only gates cross-process DocsApi
iroh-gossip = "0.101"  # default "net"
```

## Unverified (confirm against docs.rs before relying)
- `SecretKey` exact byte-method names.
- `AddrInfoOptions` variants (`Default::default()` works).
- `QueryBuilder` finalizer names (sort/limit/offset).

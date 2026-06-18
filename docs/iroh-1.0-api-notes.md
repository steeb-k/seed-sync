# iroh 1.0 stack — API notes (verified 2026-06-17)

Quick reference for the exact API we build against. Versions: **iroh 1.0.0,
iroh-blobs 0.103.0, iroh-docs 0.101.0, iroh-gossip 0.101.0, iroh-tickets 1.0.0**.
The protocol crates (blobs/docs/gossip) stay on 0.10x even though they target
iroh 1.0 — this skew is expected.

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

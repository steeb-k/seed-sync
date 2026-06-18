//! S0b spike: prove the iroh-docs multi-writer flow works end to end on the
//! 1.0 stack, so the engine's folder<->doc<->blob mapping rests on solid API.
//!
//! Two in-process nodes, each with its own endpoint + blob store + gossip +
//! docs. Node A creates a document, writes a signed-manifest-style value plus a
//! file entry, and shares a write ticket. Node B imports the ticket, waits for
//! sync, reads the entries back, and confirms:
//!   * the key/value set reconciled (content-addressed bytes survived transport),
//!   * the author id on B's copy matches A's author (write attribution), and
//!   * a signature A embedded verifies on B (the manifest trust model travels).
//!
//! Marked `#[ignore]` because it opens real iroh endpoints (discovery/relay);
//! run explicitly with `cargo test -p seed-core --test docs_spike -- --ignored`.

use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey, Verifier};
use futures_lite::StreamExt;
use iroh::{protocol::Router, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol};
use iroh_docs::{
    api::protocol::{AddrInfoOptions, ShareMode},
    protocol::Docs,
    store::Query,
};
use iroh_gossip::net::Gossip;

struct Node {
    router: Router,
    blobs: MemStore,
    docs: Docs,
}

impl Node {
    async fn spawn() -> anyhow::Result<Self> {
        let endpoint = Endpoint::bind(iroh::endpoint::presets::N0).await?;
        let blobs = MemStore::new();
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory()
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
            .accept(iroh_gossip::ALPN, gossip)
            .accept(iroh_docs::ALPN, docs.clone())
            .spawn();
        Ok(Self {
            router,
            blobs,
            docs,
        })
    }

    fn addr(&self) -> iroh::EndpointAddr {
        self.router.endpoint().addr()
    }

    async fn shutdown(self) -> anyhow::Result<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn two_nodes_reconcile_a_document() -> anyhow::Result<()> {
    let a = Node::spawn().await?;
    let b = Node::spawn().await?;

    // --- Node A authors a document ---
    let a_api = a.docs.api();
    let author = a_api.author_create().await?;
    let doc = a_api.create().await?;

    // A "manifest" value signed by a master key, plus a file entry.
    let master = SigningKey::generate(&mut rand::rngs::OsRng);
    let manifest_payload = b"manifest-v1-bytes".to_vec();
    let sig = master.sign(&manifest_payload).to_bytes().to_vec();
    doc.set_bytes(author, b"\x00manifest".to_vec(), manifest_payload.clone())
        .await?;
    doc.set_bytes(author, b"\x00manifest.sig".to_vec(), sig.clone())
        .await?;
    doc.set_bytes(author, b"file/a.txt".to_vec(), b"hello world".to_vec())
        .await?;

    // --- A shares a write ticket; B joins ---
    // Embed A's direct addresses (not just the node id) so B can dial without
    // relying on DNS discovery / relays — deterministic for a local test.
    let ticket = doc
        .share(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let b_api = b.docs.api();
    let (b_doc, events) = b_api.import_and_subscribe(ticket).await?;
    let mut events = Box::pin(events);
    // Belt-and-suspenders: explicitly point B at A's address.
    b_doc.start_sync(vec![a.addr()]).await?;

    // Wait until B has the content for our three keys (or time out).
    let want_keys = 3;
    let got = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            // Drain a few events to drive progress; then poll the store.
            let _ = tokio::time::timeout(Duration::from_secs(2), events.next()).await;
            let mut stream = std::pin::pin!(b_doc.get_many(Query::all()).await?);
            let mut ready = 0usize;
            while let Some(entry) = stream.next().await {
                let entry = entry?;
                if b.blobs.blobs().has(entry.content_hash()).await? {
                    ready += 1;
                }
            }
            if ready >= want_keys {
                return anyhow::Ok(ready);
            }
        }
    })
    .await??;
    assert!(got >= want_keys, "expected {want_keys} keys, got {got}");

    // --- Read back and verify reconcile + attribution + signature ---
    let manifest_entry = b_doc
        .get_one(Query::single_latest_per_key().key_exact(b"\x00manifest"))
        .await?
        .expect("manifest entry present on B");
    // Attribution: the entry on B is authored by A's author.
    assert_eq!(manifest_entry.author(), author);

    let manifest_bytes = b
        .blobs
        .blobs()
        .get_bytes(manifest_entry.content_hash())
        .await?;
    assert_eq!(manifest_bytes.as_ref(), manifest_payload.as_slice());

    let sig_entry = b_doc
        .get_one(Query::single_latest_per_key().key_exact(b"\x00manifest.sig"))
        .await?
        .expect("sig entry present on B");
    let sig_bytes = b.blobs.blobs().get_bytes(sig_entry.content_hash()).await?;
    let sig = ed25519_dalek::Signature::from_slice(sig_bytes.as_ref())?;
    // The manifest trust model travels: B verifies A's signature.
    master.verifying_key().verify(&manifest_bytes, &sig)?;

    let file_entry = b_doc
        .get_one(Query::single_latest_per_key().key_exact(b"file/a.txt"))
        .await?
        .expect("file entry present on B");
    let file_bytes = b.blobs.blobs().get_bytes(file_entry.content_hash()).await?;
    assert_eq!(file_bytes.as_ref(), b"hello world");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

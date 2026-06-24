//! The shared iroh node: one endpoint + blob store + gossip + docs, all behind
//! a single [`Router`], reused across every share this daemon serves.
//!
//! The device identity (iroh [`SecretKey`]) is persisted to `node.key` in the
//! data dir so the endpoint id is stable across restarts. Blob and document
//! stores are filesystem-backed so synced content survives restarts.

use std::path::{Path, PathBuf};

use anyhow::Context;
use iroh::{protocol::Router, Endpoint, SecretKey};
use iroh_blobs::{store::fs::FsStore, BlobsProtocol};
use iroh_docs::{api::DocsApi, protocol::Docs};
use iroh_gossip::net::Gossip;

/// A running iroh node with the three protocols Seed Sync needs.
pub struct IrohNode {
    pub endpoint: Endpoint,
    pub blobs: FsStore,
    /// The blob store's root dir (`<data_dir>/blobs`). Used to reclaim a blob's
    /// owned `data/<hash>.data` file after a cross-volume reference export leaves
    /// it orphaned (see `engine::reclaim_owned_data`).
    pub blobs_dir: PathBuf,
    pub gossip: Gossip,
    pub docs: Docs,
    router: Router,
}

impl IrohNode {
    /// Bootstrap the node, creating the data dir layout if needed:
    /// `node.key`, `blobs/`, `docs.redb`. The blob store lives under `data_dir`.
    pub async fn spawn(data_dir: &Path) -> anyhow::Result<Self> {
        Self::spawn_with_blobs(data_dir, &data_dir.join("blobs")).await
    }

    /// Like [`spawn`](Self::spawn) but with the blob store rooted at an explicit
    /// `blobs_dir`. On Android we keep `node.key` + `docs/` on internal storage
    /// while placing `blobs/` on the same shared-storage volume as the synced
    /// folders, so the engine's zero-copy reference export (rename/hardlink)
    /// stays on one volume and never falls back to a full copy.
    pub async fn spawn_with_blobs(data_dir: &Path, blobs_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;

        let secret_key = load_or_create_secret_key(&data_dir.join("node.key"))?;

        let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .context("bind iroh endpoint")?;

        let blobs_dir = blobs_dir.to_path_buf();
        let docs_dir = data_dir.join("docs");
        std::fs::create_dir_all(&blobs_dir).context("create blobs dir")?;
        std::fs::create_dir_all(&docs_dir).context("create docs dir")?;

        let blobs = FsStore::load(&blobs_dir).await.context("open blob store")?;
        let gossip = Gossip::builder().spawn(endpoint.clone());
        // `Docs::persistent` treats its argument as a directory and creates
        // `docs.redb` inside it.
        let docs = Docs::persistent(docs_dir)
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await
            .context("spawn docs")?;

        let router = Router::builder(endpoint.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(iroh_docs::ALPN, docs.clone())
            .spawn();

        Ok(Self {
            endpoint,
            blobs,
            blobs_dir,
            gossip,
            docs,
            router,
        })
    }

    pub fn docs_api(&self) -> &DocsApi {
        self.docs.api()
    }

    /// This node's endpoint id (32 bytes), used as a discovery bootstrap hint
    /// when minting share keys.
    pub fn endpoint_id_bytes(&self) -> [u8; 32] {
        *self.endpoint.id().as_bytes()
    }

    /// This node's current dialable address.
    pub fn addr(&self) -> iroh::EndpointAddr {
        self.endpoint.addr()
    }

    /// Wait until the endpoint has contacted a relay (and thus has a complete,
    /// dialable [`addr`](Self::addr) with relay URL + direct addresses).
    pub async fn wait_online(&self) {
        self.endpoint.online().await;
    }

    /// Cumulative (bytes_sent, bytes_received) across all transports (IPv4/IPv6/
    /// relay). Sample over time and diff to get throughput. This is endpoint-wide
    /// (all shares combined), matching the GUI's global speed indicator.
    pub fn byte_totals(&self) -> (u64, u64) {
        let m = &self.endpoint.metrics().socket;
        let sent = m.send_ipv4.get() + m.send_ipv6.get() + m.send_relay.get();
        let recv = m.recv_data_ipv4.get()
            + m.recv_data_ipv6.get()
            + m.recv_data_relay.get()
            + m.recv_data_custom.get();
        (sent, recv)
    }

    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}

/// Load the persisted device secret key, or generate and persist a new one.
fn load_or_create_secret_key(path: &PathBuf) -> anyhow::Result<SecretKey> {
    if let Ok(bytes) = std::fs::read(path) {
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("node.key must be exactly 32 bytes")?;
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let key = SecretKey::generate();
        // Best-effort tighten permissions on unix (0600).
        std::fs::write(path, key.to_bytes()).context("write node.key")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(key)
    }
}

//! The shared iroh node: one endpoint + blob store + gossip + docs, all behind
//! a single [`Router`], reused across every share this daemon serves.
//!
//! The device identity (iroh [`SecretKey`]) is persisted to `node.key` in the
//! data dir so the endpoint id is stable across restarts. Blob and document
//! stores are filesystem-backed so synced content survives restarts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use iroh::{protocol::Router, Endpoint, SecretKey};
use iroh_blobs::{api::downloader::Downloader, store::fs::FsStore, BlobsProtocol};
use iroh_docs::{api::DocsApi, protocol::Docs};
use iroh_gossip::net::Gossip;

/// A running iroh node with the three protocols SEED Sync needs.
pub struct IrohNode {
    pub endpoint: Endpoint,
    pub blobs: FsStore,
    /// Long-lived content downloader (one actor for the node). Cloned into each
    /// reconcile job so the engine drives blob fetches itself with a
    /// load-balanced provider set, rather than leaving it to iroh-docs' built-in
    /// auto-downloader (which funnels every file through the doc-sync source —
    /// the master). Cheap to clone (an mpsc handle).
    pub downloader: Downloader,
    /// The blob store's root dir (`<data_dir>/blobs`). Used to reclaim a blob's
    /// owned `data/<hash>.data` file after a cross-volume reference export leaves
    /// it orphaned (see `engine::reclaim_owned_data`).
    pub blobs_dir: PathBuf,
    pub gossip: Gossip,
    pub docs: Docs,
    /// Live handle to the relay URLs the path selector prefers (the user's own
    /// relays). Updated by [`Engine::set_relay_settings`](crate::Engine::set_relay_settings)
    /// — the selector itself can't be swapped after bind.
    pub preferred_relays: crate::relays::PreferredRelays,
    router: Router,
}

impl IrohNode {
    /// Bootstrap the node, creating the data dir layout if needed:
    /// `node.key`, `blobs/`, `docs.redb`. The blob store lives under `data_dir`.
    pub async fn spawn(data_dir: &Path) -> anyhow::Result<Self> {
        Self::spawn_with_blobs(data_dir, &data_dir.join("blobs"), &Default::default()).await
    }

    /// Like [`spawn`](Self::spawn) but with the blob store rooted at an explicit
    /// `blobs_dir`. On Android we keep `node.key` + `docs/` on internal storage
    /// while placing `blobs/` on the same shared-storage volume as the synced
    /// folders, so the engine's zero-copy reference export (rename/hardlink)
    /// stays on one volume and never falls back to a full copy.
    pub async fn spawn_with_blobs(
        data_dir: &Path,
        blobs_dir: &Path,
        relay_settings: &crate::relays::RelaySettings,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;

        let secret_key = load_or_create_secret_key(&data_dir.join("node.key"))?;
        // The endpoint id is the public half of the device key; we need it to
        // build the mDNS service below, before the secret key is moved into the
        // endpoint builder.
        let endpoint_id = secret_key.public();

        // The N0 preset wires up n0 DNS discovery + relays (internet path). On
        // top of that we add mDNS-based local-network address lookup so two
        // members on the same LAN can find and reach each other with no
        // internet at all. Building the mDNS service can fail on a host with no
        // usable IPv4/IPv6 (or where multicast is unavailable) — degrade to "no
        // LAN discovery" with a warning rather than failing endpoint startup.
        let mut builder = Endpoint::builder(iroh::endpoint::presets::N0).secret_key(secret_key);

        // Custom relay servers (see `crate::relays`). The path selector is
        // installed unconditionally — with no preferred relays it behaves like
        // iroh's default — because it can't be swapped after bind, while the
        // preferred set and the relay map can both change at runtime.
        let preferred_relays = crate::relays::PreferredRelays::default();
        builder = builder.path_selector(Arc::new(crate::relays::PreferMyRelaySelector::new(
            preferred_relays.clone(),
        )));
        if let Some(mode) = crate::relays::relay_mode(relay_settings) {
            if let Ok(urls) = crate::relays::relay_urls(relay_settings) {
                preferred_relays.set(urls.into_iter().collect());
            }
            builder = builder.relay_mode(mode);
        }
        match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint_id) {
            Ok(mdns) => builder = builder.address_lookup(mdns),
            Err(e) => tracing::warn!("local-network (mDNS) discovery unavailable: {e}"),
        }
        let endpoint = builder.bind().await.context("bind iroh endpoint")?;

        let blobs_dir = blobs_dir.to_path_buf();
        let docs_dir = data_dir.join("docs");
        std::fs::create_dir_all(&blobs_dir).context("create blobs dir")?;
        std::fs::create_dir_all(&docs_dir).context("create docs dir")?;

        let blobs = FsStore::load(&blobs_dir).await.context("open blob store")?;
        let downloader = blobs.downloader(&endpoint);
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
            downloader,
            blobs_dir,
            gossip,
            docs,
            preferred_relays,
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
        // Persist the blob store's ephemeral state FIRST — specifically the
        // verified-range bitfields of large *partial* downloads. iroh-blobs keeps
        // those in memory and only flushes them on a clean `Store::shutdown`
        // (vendor/iroh-blobs/src/store/fs.rs runtime note). Its docs claim
        // `Router::shutdown` also closes the store, but in practice a big in-flight
        // download's progress was lost on every restart (reported 0% after reopen,
        // re-fetched from scratch) — fatal on a frequently-suspending laptop that
        // restarts mid-transfer (docs/sleep-resume-investigation.md). Shutting the
        // store down explicitly writes those bitfields so the next start resumes
        // from what's already on disk instead of re-downloading. Best-effort: a
        // store error here must not block tearing the endpoint down.
        if let Err(e) = self.blobs.shutdown().await {
            tracing::warn!("blob store shutdown (persist partials) failed: {e}");
        }
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

//! The shared iroh node: one endpoint + blob store + gossip + docs, all behind
//! a single [`Router`], reused across every share this daemon serves.
//!
//! The device identity (iroh [`SecretKey`]) is persisted to `node.key` in the
//! data dir so the endpoint id is stable across restarts. Blob and document
//! stores are filesystem-backed so synced content survives restarts.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Context;
use iroh::{protocol::Router, Endpoint, SecretKey};
use iroh_blobs::provider::events::{
    EventMask, EventSender, ProviderMessage, RequestMode, RequestUpdate,
};
use iroh_blobs::store::fs::options::Options as BlobStoreOptions;
use iroh_blobs::store::GcConfig;
use iroh_blobs::{api::downloader::Downloader, store::fs::FsStore, BlobsProtocol, Hash};

/// Hashes a peer asked this node for that the blob provider could not serve: the
/// store lists the hash, but exporting it failed (its data file is gone, or the
/// file it references has moved), so the provider reset the peer's stream with
/// `ERR_INTERNAL`. Filled by [`watch_provider_events`], drained by
/// `Engine::repair_refused_blobs`, which re-imports the file from the share folder
/// so the peer's next retry succeeds (known-issues #38).
pub type RefusedBlobs = Arc<StdMutex<HashSet<Hash>>>;
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
    /// Blobs this node was asked for and could not serve; see [`RefusedBlobs`].
    pub refused: RefusedBlobs,
    router: Router,
}

/// Consume the blob provider's event stream and record every transfer it had to
/// abort. This is how a member learns that a peer's fetch of a hash it *claims* to
/// hold was refused — the request itself is the signal, no protocol of our own.
///
/// Only the notify variants are enabled (nothing here can reject a request), and
/// each request's update stream is drained on its own task so a slow consumer can
/// never stall the provider. A transfer also aborts when the *peer* hangs up, so an
/// entry here means "probe this hash", not "this hash is broken"; the engine reads
/// the blob before it does anything.
async fn watch_provider_events(
    mut rx: tokio::sync::mpsc::Receiver<ProviderMessage>,
    refused: RefusedBlobs,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ProviderMessage::GetRequestReceivedNotify(msg) => {
                let requested = msg.inner.request.hash;
                let mut updates = msg.rx;
                let refused = refused.clone();
                tokio::spawn(async move {
                    // A get for a hash sequence names each child as its transfer
                    // starts; a plain get only ever transfers the requested hash.
                    let mut current = requested;
                    while let Ok(Some(update)) = updates.recv().await {
                        match update {
                            RequestUpdate::Started(s) => current = s.hash,
                            RequestUpdate::Aborted(_) => {
                                tracing::debug!(
                                    "provider aborted serving {current}; queued for a repair probe"
                                );
                                if let Ok(mut r) = refused.lock() {
                                    r.insert(current);
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
            ProviderMessage::GetManyRequestReceivedNotify(msg) => {
                let mut updates = msg.rx;
                let refused = refused.clone();
                tokio::spawn(async move {
                    let mut current: Option<Hash> = None;
                    while let Ok(Some(update)) = updates.recv().await {
                        match update {
                            RequestUpdate::Started(s) => current = Some(s.hash),
                            RequestUpdate::Aborted(_) => {
                                if let Some(h) = current {
                                    tracing::debug!(
                                        "provider aborted serving {h}; queued for a repair probe"
                                    );
                                    if let Ok(mut r) = refused.lock() {
                                        r.insert(h);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

impl IrohNode {
    /// Bootstrap the node, creating the data dir layout if needed:
    /// `node.key`, `blobs/`, `docs.redb`. The blob store lives under `data_dir`.
    pub async fn spawn(data_dir: &Path) -> anyhow::Result<Self> {
        Self::spawn_with_blobs(data_dir, &data_dir.join("blobs"), &Default::default(), None).await
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
        gc: Option<GcConfig>,
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

        // `FsStore::load` with our own options so a `GcConfig` can enable the
        // periodic GC sweep (known-issues #22); the db path mirrors `load`'s
        // (`<blobs_dir>/blobs.db`). With `gc = None` this is exactly the default
        // `load` behaviour.
        let mut blob_opts = BlobStoreOptions::new(&blobs_dir);
        blob_opts.gc = gc;
        let blobs = FsStore::load_with_opts(blobs_dir.join("blobs.db"), blob_opts)
            .await
            .context("open blob store")?;
        let downloader = blobs.downloader(&endpoint);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        // `Docs::persistent` treats its argument as a directory and creates
        // `docs.redb` inside it.
        let docs = Docs::persistent(docs_dir)
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await
            .context("spawn docs")?;

        // Watch our own blob provider so a fetch we had to refuse becomes a repair
        // instead of a peer retrying forever against a store that lists a hash it
        // cannot read (known-issues #38). See `watch_provider_events`.
        let refused: RefusedBlobs = Arc::new(StdMutex::new(HashSet::new()));
        let (events, events_rx) = EventSender::channel(
            64,
            EventMask {
                get: RequestMode::NotifyLog,
                get_many: RequestMode::NotifyLog,
                ..EventMask::DEFAULT
            },
        );
        tokio::spawn(watch_provider_events(events_rx, refused.clone()));

        let router = Router::builder(endpoint.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, Some(events)))
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
            refused,
            router,
        })
    }

    /// Tear the whole iroh stack down and bring it back up **in place**: same
    /// `node.key` (so the endpoint id is stable), same blob store and docs
    /// directory, fresh endpoint / gossip / docs actors / router.
    ///
    /// This is the in-process equivalent of a daemon restart at the transport
    /// layer, and exists because a restart is the only thing that has ever
    /// cleared known-issues #36: after days of uptime an endpoint can lose the
    /// ability to reach a member that a *fresh* endpoint on the same machine
    /// reaches in under a second (stale per-remote path / relay-actor state
    /// inside iroh). Every handle cloned out of the old node (docs, blob store,
    /// downloader, endpoint) is dead after this; the caller re-opens its shares.
    ///
    /// The old stores must be fully closed before the new ones open — both
    /// `blobs.db` and `docs.redb` are single-writer redb files — so the store
    /// shutdown is awaited first and the respawn retries briefly while a
    /// background actor is still releasing its file lock.
    pub async fn rebuild(
        &mut self,
        data_dir: &Path,
        blobs_dir: &Path,
        relay_settings: &crate::relays::RelaySettings,
        gc: Option<GcConfig>,
    ) -> anyhow::Result<()> {
        if let Err(e) = self.blobs.shutdown().await {
            tracing::warn!("transport rebuild: blob store shutdown: {e}");
        }
        if let Err(e) = self.router.shutdown().await {
            tracing::warn!("transport rebuild: router shutdown: {e}");
        }
        // Belt and braces: the router shuts the endpoint down, but make sure the
        // socket is gone before binding the replacement.
        self.endpoint.close().await;

        let mut last_err = None;
        for attempt in 1..=20u32 {
            match Self::spawn_with_blobs(data_dir, blobs_dir, relay_settings, gc.clone()).await {
                Ok(fresh) => {
                    *self = fresh;
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!("transport rebuild: respawn attempt {attempt} failed: {e:#}");
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("transport rebuild: respawn never attempted"))
            .context("respawn iroh node after shutdown"))
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
        // restarts mid-transfer (known-issues #21). Shutting the
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

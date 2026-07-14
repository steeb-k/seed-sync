//! SEED Sync mobile facade.
//!
//! This crate is the Android equivalent of `seed-daemon`: it owns a tokio
//! runtime and a single [`seed_core::Engine`], and exposes a flat, UniFFI-
//! friendly API the Kotlin/Compose app drives directly — no socket, no separate
//! process. The reconcile loop and throughput loop are lifted here verbatim from
//! `seed-daemon/src/main.rs`; instead of broadcasting [`seed_ipc::IpcEvent`]s
//! over a socket they invoke a registered [`EventListener`] callback.
//!
//! Threading model: the exported methods are *synchronous* and drive the engine
//! by `block_on`-ing the owned runtime. The Android side calls them off the main
//! thread (a background `Dispatchers.IO` coroutine inside the foreground
//! service), so blocking is fine and the lifecycle stays explicit. The
//! background loops run as tasks *on* the runtime and converge sync exactly as
//! the desktop daemon does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use seed_core::Engine;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

uniffi::setup_scaffolding!();

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Every fallible facade method surfaces engine/anyhow failures as this flat
/// error; UniFFI maps it to a thrown exception carrying the message on the
/// Kotlin side.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum SeedError {
    #[error("{0}")]
    Engine(String),
}

impl From<anyhow::Error> for SeedError {
    fn from(e: anyhow::Error) -> Self {
        // `{:#}` includes the full anyhow context chain (e.g. "scan folder:
        // permission denied"), matching the daemon's diagnostics.
        SeedError::Engine(format!("{e:#}"))
    }
}

type Result<T> = std::result::Result<T, SeedError>;

// ---------------------------------------------------------------------------
// Records / enums — mirror the seed-ipc data model 1:1 so the wire-compatible
// shapes carry over to Kotlin unchanged.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum Role {
    Master,
    Viewer,
}

impl From<seed_ipc::Role> for Role {
    fn from(r: seed_ipc::Role) -> Self {
        match r {
            seed_ipc::Role::Master => Role::Master,
            seed_ipc::Role::Viewer => Role::Viewer,
        }
    }
}

/// Keeps an `Error` variant the engine no longer produces (`seed_ipc` dropped
/// it as dead) so the Android ABI and existing Kotlin `when` arms stay valid.
///
/// **Variant order is the ABI.** UniFFI marshals this enum by *ordinal*, and the
/// generated Kotlin (`FfiConverterTypeShareStatus`) reads it back as
/// `values()[ordinal]` — so a variant inserted in the middle silently renumbers
/// every one after it, and a Kotlin binding that wasn't regenerated in lockstep
/// would then decode every status as its neighbour. Only ever **append**, and
/// regenerate `android/app/src/main/java/uniffi/seed_mobile/seed_mobile.kt`.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ShareStatus {
    Healthy,
    Syncing,
    Indexing,
    Paused,
    Error,
    OutOfSync,
    NoPeers,
}

impl From<seed_ipc::ShareStatus> for ShareStatus {
    fn from(s: seed_ipc::ShareStatus) -> Self {
        match s {
            seed_ipc::ShareStatus::Healthy => ShareStatus::Healthy,
            seed_ipc::ShareStatus::Syncing => ShareStatus::Syncing,
            seed_ipc::ShareStatus::Indexing => ShareStatus::Indexing,
            seed_ipc::ShareStatus::Paused => ShareStatus::Paused,
            seed_ipc::ShareStatus::OutOfSync => ShareStatus::OutOfSync,
            seed_ipc::ShareStatus::NoPeers => ShareStatus::NoPeers,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ShareSummary {
    pub share_id: String,
    pub name: String,
    pub folder: String,
    pub role: Role,
    pub status: ShareStatus,
    pub percent: u8,
    pub online: u32,
    pub total: u32,
    pub paused: bool,
    pub indexed_bytes: u64,
    pub index_total: u64,
    pub last_updated: i64,
}

impl From<seed_ipc::ShareSummary> for ShareSummary {
    fn from(s: seed_ipc::ShareSummary) -> Self {
        ShareSummary {
            share_id: s.share_id,
            name: s.name,
            folder: s.folder,
            role: s.role.into(),
            status: s.status.into(),
            percent: s.percent,
            online: s.online,
            total: s.total,
            paused: s.paused,
            indexed_bytes: s.indexed_bytes,
            index_total: s.index_total,
            last_updated: s.last_updated,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PeerInfo {
    pub node_id: String,
    pub name: Option<String>,
    pub role: Role,
    pub online: bool,
    pub last_seen: i64,
    pub have_seqno: u64,
    pub percent: u8,
}

impl From<seed_ipc::PeerInfo> for PeerInfo {
    fn from(p: seed_ipc::PeerInfo) -> Self {
        PeerInfo {
            node_id: p.node_id,
            name: p.name,
            role: p.role.into(),
            online: p.online,
            last_seen: p.last_seen,
            have_seqno: p.have_seqno,
            percent: p.percent,
        }
    }
}

/// Result of creating a new master share: the id plus both key strings to show
/// in the reveal-keys dialog.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CreatedShare {
    pub share_id: String,
    pub master_key: String,
    pub viewer_key: String,
}

/// Keys returned by [`MobileEngine::reveal_keys`]; `master_key` is present only
/// when the local role is master.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ShareKeys {
    pub master_key: Option<String>,
    pub viewer_key: String,
}

// ---------------------------------------------------------------------------
// Event callback — the Android-side observer, replacing the socket subscription.
// ---------------------------------------------------------------------------

/// Implemented in Kotlin and registered once at init. The background loops
/// invoke it to push live state changes (replacing the daemon's broadcast of
/// [`seed_ipc::IpcEvent`]). The app refreshes its UI from these signals.
#[uniffi::export(callback_interface)]
pub trait EventListener: Send + Sync + 'static {
    /// Something about the share list changed (membership, status, or content) —
    /// re-query [`MobileEngine::list_shares`]. Mirrors `IpcEvent::ShareListChanged`.
    fn on_share_list_changed(&self);
    /// Endpoint-wide throughput sample, bytes/sec. Mirrors `IpcEvent::Throughput`.
    fn on_throughput(&self, down_bps: u64, up_bps: u64);
    /// A share just finished a successful sync/publish at `ts` (unix seconds).
    /// Mirrors `IpcEvent::LastUpdated`.
    fn on_last_updated(&self, share_id: String, ts: i64);
}

// ---------------------------------------------------------------------------
// The engine object
// ---------------------------------------------------------------------------

struct Inner {
    engine: Mutex<Engine>,
    listener: Box<dyn EventListener>,
    running: AtomicBool,
}

/// The single long-lived object the Android foreground service holds. Construct
/// once with [`MobileEngine::init`]; it spins up the engine and starts the
/// reconcile + throughput loops immediately. Drop it (or call
/// [`MobileEngine::shutdown`]) to stop.
#[derive(uniffi::Object)]
pub struct MobileEngine {
    rt: Runtime,
    inner: Arc<Inner>,
}

#[uniffi::export]
impl MobileEngine {
    /// Boot the engine against `data_dir` (holding `state.db`, `node.key`,
    /// `docs/`) with the blob store rooted at `blobs_dir`. On Android pass an
    /// internal-storage path for `data_dir` and a shared-storage path on the
    /// same volume as the synced folders for `blobs_dir`, so reference exports
    /// stay zero-copy. If `blobs_dir` is `None` the store lives under
    /// `data_dir/blobs` (the desktop layout). Starts the background loops.
    #[uniffi::constructor]
    pub fn init(
        data_dir: String,
        blobs_dir: Option<String>,
        listener: Box<dyn EventListener>,
    ) -> Result<Arc<Self>> {
        init_android_logging();

        let rt = Runtime::new().map_err(|e| SeedError::Engine(format!("create runtime: {e}")))?;
        let data_dir = PathBuf::from(data_dir);
        let blobs_dir = blobs_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("blobs"));

        let engine = rt.block_on(async {
            std::fs::create_dir_all(&data_dir)
                .map_err(|e| anyhow::anyhow!("create data dir: {e}"))?;
            let engine = Engine::new_with_blobs(&data_dir, &blobs_dir).await?;
            // Briefly wait for a dialable address so the first NodeAddr is useful,
            // but don't wedge startup if relays are unreachable.
            let _ = tokio::time::timeout(Duration::from_secs(10), engine.wait_online()).await;
            Ok::<_, anyhow::Error>(engine)
        })?;

        let inner = Arc::new(Inner {
            engine: Mutex::new(engine),
            listener,
            running: AtomicBool::new(true),
        });

        rt.spawn(reconcile_loop(inner.clone()));
        rt.spawn(throughput_loop(inner.clone()));

        Ok(Arc::new(MobileEngine { rt, inner }))
    }

    /// Snapshot of every share, for the share list. Mirrors `ListShares`.
    pub fn list_shares(&self) -> Vec<ShareSummary> {
        self.rt.block_on(async {
            self.inner
                .engine
                .lock()
                .await
                .list_summaries()
                .into_iter()
                .map(Into::into)
                .collect()
        })
    }

    /// Create a new master share for `folder` with optional ignore globs, run the
    /// initial import, and return both keys. Mirrors `CreateShare`.
    pub fn create_share(&self, folder: String, ignore: Vec<String>) -> Result<CreatedShare> {
        let inner = self.inner.clone();
        self.rt.block_on(async move {
            // Open + persist under a brief lock, then import without the lock so a
            // large folder doesn't block list/status queries (as the daemon does).
            let (created, job) = {
                let mut engine = inner.engine.lock().await;
                engine.create_open(&PathBuf::from(folder), ignore).await?
            };
            inner.listener.on_share_list_changed();
            let outcome = job.run().await;
            if let Err(e) = &outcome {
                tracing::warn!("initial import for {} failed: {e:#}", created.share_id);
            }
            {
                let mut engine = inner.engine.lock().await;
                engine.finish_reconcile(&created.share_id, outcome.ok());
            }
            inner.listener.on_share_list_changed();
            Ok(CreatedShare {
                share_id: created.share_id,
                master_key: created.master_key,
                viewer_key: created.viewer_key,
            })
        })
    }

    /// Add an existing share from a key string, syncing into `folder`. Optional
    /// `bootstrap` is an iroh endpoint-ticket string. Mirrors `AddShare`.
    pub fn add_share(
        &self,
        key: String,
        folder: String,
        bootstrap: Option<String>,
    ) -> Result<String> {
        let inner = self.inner.clone();
        self.rt.block_on(async move {
            let boot = match bootstrap {
                Some(s) => vec![Engine::parse_bootstrap(&s)?],
                None => vec![],
            };
            let share_id = {
                let mut engine = inner.engine.lock().await;
                engine.add_share(&key, &PathBuf::from(folder), boot).await?
            };
            inner.listener.on_share_list_changed();
            Ok(share_id)
        })
    }

    /// Trigger a master republish (rescan + sign) for a share. Mirrors `Publish`.
    pub fn publish(&self, share_id: String) -> Result<()> {
        self.rt.block_on(async {
            self.inner.engine.lock().await.publish(&share_id).await?;
            Ok(())
        })
    }

    /// This node's dialable address as an endpoint-ticket string, to hand a peer
    /// as the `bootstrap` of `add_share`. Mirrors `NodeAddr`.
    pub fn node_addr(&self) -> String {
        self.rt
            .block_on(async { self.inner.engine.lock().await.endpoint_ticket() })
    }

    /// Reveal a share's keys (master key only when locally a master). Mirrors
    /// `RevealKeys`.
    pub fn reveal_keys(&self, share_id: String) -> Result<ShareKeys> {
        self.rt.block_on(async {
            let (master_key, viewer_key) = self.inner.engine.lock().await.reveal_keys(&share_id)?;
            Ok(ShareKeys {
                master_key,
                viewer_key,
            })
        })
    }

    pub fn pause(&self, share_id: String) -> Result<()> {
        self.rt.block_on(async {
            self.inner.engine.lock().await.set_paused(&share_id, true)?;
            self.inner.listener.on_share_list_changed();
            Ok(())
        })
    }

    pub fn resume(&self, share_id: String) -> Result<()> {
        self.rt.block_on(async {
            self.inner
                .engine
                .lock()
                .await
                .set_paused(&share_id, false)?;
            self.inner.listener.on_share_list_changed();
            Ok(())
        })
    }

    pub fn pause_all(&self) -> Result<()> {
        self.rt.block_on(async {
            self.inner.engine.lock().await.set_paused_all(true)?;
            self.inner.listener.on_share_list_changed();
            Ok(())
        })
    }

    pub fn resume_all(&self) -> Result<()> {
        self.rt.block_on(async {
            self.inner.engine.lock().await.resume_all()?;
            self.inner.listener.on_share_list_changed();
            Ok(())
        })
    }

    pub fn remove_share(&self, share_id: String, delete_files: bool) -> Result<()> {
        self.rt.block_on(async {
            self.inner
                .engine
                .lock()
                .await
                .remove_share(&share_id, delete_files)
                .await?;
            self.inner.listener.on_share_list_changed();
            Ok(())
        })
    }

    pub fn get_peers(&self, share_id: String) -> Result<Vec<PeerInfo>> {
        self.rt.block_on(async {
            let peers = self.inner.engine.lock().await.peers(&share_id)?;
            Ok(peers.into_iter().map(Into::into).collect())
        })
    }

    /// Whether the global user pause switch is set (every share suspended via
    /// "Pause all" / the notification toggle). Distinct from the Wi-Fi/charging
    /// gate and from per-share pauses.
    pub fn paused_all(&self) -> bool {
        self.rt
            .block_on(async { self.inner.engine.lock().await.paused_all() })
    }

    /// This device's display name (default: hostname). Mirrors `GetDeviceName`.
    pub fn device_name(&self) -> String {
        self.rt
            .block_on(async { self.inner.engine.lock().await.device_name() })
    }

    /// Set the device name and push an immediate presence broadcast so peers see
    /// the rename within a tick. Mirrors `SetDeviceName`.
    pub fn set_device_name(&self, name: String) -> Result<()> {
        self.rt.block_on(async {
            let jobs = {
                let engine = self.inner.engine.lock().await;
                engine.set_device_name(&name)?;
                engine.presence_broadcasts()
            };
            for job in jobs {
                job.send().await;
            }
            self.inner.listener.on_share_list_changed();
            Ok(())
        })
    }

    /// Suspend or resume sync via the host network/charging gate (Android's
    /// Wi-Fi-only / charging-only policy). Independent of the user's pause state:
    /// while suspended no share reconciles, and clearing it resumes whatever the
    /// user hasn't explicitly paused. Cheap and idempotent — call it whenever the
    /// network type, charging state, or the relevant setting changes.
    pub fn set_sync_suspended(&self, suspended: bool) {
        self.rt
            .block_on(async { self.inner.engine.lock().await.set_sync_suspended(suspended) });
    }

    /// Stop the background loops. The node is torn down when the object is
    /// dropped (the foreground service drops its reference on stop).
    pub fn shutdown(&self) {
        self.inner.running.store(false, Ordering::SeqCst);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Periodically reconcile every share against the merged replica. Ported from
/// `seed-daemon`'s `reconcile_loop`; the only change is that share/membership
/// changes invoke the [`EventListener`] instead of a broadcast channel.
async fn reconcile_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(Duration::from_millis(750));
    let mut tick_n: u64 = 0;
    let mut last_membership: u64 = 0;
    while inner.running.load(Ordering::SeqCst) {
        tick.tick().await;
        tick_n = tick_n.wrapping_add(1);
        let mut changed = Vec::new();

        let ids = { inner.engine.lock().await.share_ids() };
        for id in ids {
            let job = {
                inner
                    .engine
                    .lock()
                    .await
                    .make_reconcile_job(&id)
                    .ok()
                    .flatten()
            };
            let Some(job) = job else { continue };
            match job.run().await {
                Ok(outcome) => {
                    let did = outcome.changed();
                    inner
                        .engine
                        .lock()
                        .await
                        .finish_reconcile(&id, Some(outcome));
                    if did {
                        changed.push(id);
                    }
                }
                Err(e) => {
                    tracing::warn!("reconcile {id} failed: {e:#}");
                    inner.engine.lock().await.finish_reconcile(&id, None);
                }
            }
        }

        inner.engine.lock().await.retry_reclaims();

        if tick_n.is_multiple_of(4) || !changed.is_empty() {
            let jobs = { inner.engine.lock().await.presence_broadcasts() };
            for job in jobs {
                job.send().await;
            }
        }

        if tick_n.is_multiple_of(8) {
            let rejoins = { inner.engine.lock().await.presence_rejoins() };
            for rejoin in rejoins {
                rejoin.join().await;
            }

            // Rendezvous (known-issues #16). Both halves are internally throttled,
            // so asking on every 8th tick is cheap: a master publishes its address
            // every ~2min, and a lookup happens only while a share can reach no
            // member at all. Detached, because a resolve is a network round trip
            // and this loop also drives reconcile.
            let publishes = { inner.engine.lock().await.rendezvous_publishes() };
            for publish in publishes {
                tokio::spawn(publish.run());
            }
            let dials = { inner.engine.lock().await.rendezvous_dials() };
            for dial in dials {
                tokio::spawn(dial.run());
            }
        }

        let membership = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for s in inner.engine.lock().await.list_summaries() {
                s.share_id.hash(&mut h);
                s.online.hash(&mut h);
                s.total.hash(&mut h);
                (s.status as u8).hash(&mut h);
                s.percent.hash(&mut h);
                s.paused.hash(&mut h);
            }
            h.finish()
        };
        let membership_changed = membership != last_membership;
        last_membership = membership;

        if !changed.is_empty() || membership_changed {
            inner.listener.on_share_list_changed();
        }
        if !changed.is_empty() {
            let ts = now_unix();
            for share_id in &changed {
                inner.listener.on_last_updated(share_id.clone(), ts);
            }
            tracing::debug!("reconcile changed {} share(s)", changed.len());
        }
    }
}

/// Sample endpoint byte counters once a second and push the throughput. Ported
/// from `seed-daemon`'s `throughput_loop`.
async fn throughput_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last: Option<(u64, u64)> = None;
    while inner.running.load(Ordering::SeqCst) {
        tick.tick().await;
        let (sent, recv, indexing) = {
            let engine = inner.engine.lock().await;
            let (s, r) = engine.byte_totals();
            (s, r, engine.publishing_active())
        };
        if let Some((psent, precv)) = last {
            let up = sent.saturating_sub(psent);
            let down = recv.saturating_sub(precv);
            inner.listener.on_throughput(down, up);
        }
        last = Some((sent, recv));
        if indexing {
            inner.listener.on_share_list_changed();
        }
    }
}

/// On Android, route `tracing` to Logcat once. No-op elsewhere (and idempotent —
/// a second init is ignored).
#[cfg(target_os = "android")]
fn init_android_logging() {
    use std::sync::Once;
    static START: Once = Once::new();
    START.call_once(|| {
        use tracing_subscriber::prelude::*;
        let _ = tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "seed_mobile=info,seed_core=info".into()),
            )
            .with(paranoid_android::layer("seed-sync"))
            .try_init();
    });
}

#[cfg(not(target_os = "android"))]
fn init_android_logging() {}

/// JNI entry point: hand the app's JVM `Context` to the two Android-aware pieces
/// of the iroh stack so they can reach platform services:
///
/// * **`ndk-context`** — iroh's DNS discovery (hickory-resolver) reads Android's
///   DNS config through it. Unset, the first resolver call panics "android
///   context was not initialized".
/// * **`rustls-platform-verifier`** — iroh's relay TLS validates certificates
///   against Android's trust store through it.
///
/// The Kotlin `RustlsSetup` object declares this as an `external fun` and calls
/// it once in `Application.onCreate`, before the engine starts.
///
/// Symbol name maps to `io.github.steeb_k.seedsync.RustlsSetup` (the `_1`
/// escapes the underscore in `steeb_k`, per JNI name mangling).
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_steeb_1k_seedsync_RustlsSetup_initRustlsPlatformVerifier<
    'caller,
>(
    mut env: jni::EnvUnowned<'caller>,
    _this: jni::objects::JObject<'caller>,
    context: jni::objects::JObject<'caller>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        // ndk-context wants the raw JavaVM + a long-lived global ref to the
        // Context (it stores the pointers for the process lifetime). `into_raw`
        // leaks the global ref intentionally so the jobject stays valid.
        let vm = env.get_java_vm()?;
        let ctx_global = env.new_global_ref(&context)?;
        unsafe {
            ndk_context::initialize_android_context(
                vm.get_raw() as *mut std::ffi::c_void,
                ctx_global.into_raw() as *mut std::ffi::c_void,
            );
        }
        rustls_platform_verifier::android::init_with_env(env, context)?;
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

//! A member that lists a blob it cannot serve must repair itself — on the peer's
//! refused fetch, and on its own health pass — instead of refusing forever while
//! reading `Healthy 100%`.
//!
//! Observed in the field (2026-09-17, known-issues #38). One complete member of a
//! share (the only one left after the other master's drive died) reported 100%, yet
//! two newly-added members sat at `Syncing 89%` for hours. Every fetch of the same
//! 44 files failed on the requesters with
//!
//! ```text
//! self-heal phoenix-master.xcf: fetch 476f91de…: io: stream reset by peer: error 3
//! ```
//!
//! `error 3` is the blob provider's `ERR_INTERNAL`: its store *lists* the hash but
//! `export_bao` fails on it — the owned data file is gone, or the file the entry
//! references has moved. Meanwhile the serving member's health pass credited every
//! one of those files, because it only asked `Blobs::has`, a metadata lookup that
//! still answers yes. The files were on its disk the whole time.
//!
//! Contract: (1) a refused fetch makes the serving member re-import the file from
//! disk, so the peer's next retry succeeds; (2) the health pass proves servability
//! with a real read and repairs a listed-but-unreadable blob on its own, so a member
//! never reads 100% over content it cannot hand out.
//!
//! The broken state is built with public store APIs: import the same content in
//! `Copy` mode (which rewrites the entry to an owned data file) and delete that
//! file behind the store's back — exactly what a GC sweep or a lost data file leaves
//! behind. Both tests also fail without vendored iroh-blobs hunk 3
//! (`vendor/README.md`): the store's in-memory handle stayed poisoned and its
//! merge rule kept the dead owned location over the re-imported reference.
//!
//! `#[ignore]`: real iroh endpoints and the OS keystore. Run with:
//!   cargo test -p seed-core --test serve_repair -- --ignored

use std::path::Path;
use std::time::Duration;

use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
use iroh_blobs::{BlobFormat, Hash};
use seed_core::Engine;

const FILE: &str = "artwork.xcf";

fn content() -> Vec<u8> {
    // Larger than the store's inline threshold (16 KiB) so the data lives in a
    // file that can be taken away; deterministic so every engine agrees on it.
    (0..300 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("RUST_LOG").is_none() {
            return;
        }
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    });
}

fn percent(engine: &Engine, share_id: &str) -> u8 {
    engine
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .map(|s| s.percent)
        .unwrap_or(0)
}

/// Put `engine`'s store into the field state for `hash`: listed as complete,
/// unreadable. Import the same bytes in `Copy` mode (the entry now points at an
/// owned `data/<hash>.data`), then delete that file. The store keeps an idle handle
/// open for a few seconds after an import, so both the delete (Windows) and the
/// "export now fails" precondition are retried briefly.
async fn break_blob(
    engine: &Engine,
    data_dir: &Path,
    hash: Hash,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let scratch = data_dir.join("scratch-copy.bin");
    std::fs::write(&scratch, bytes)?;
    let store = engine.debug_blob_store();
    let tag = store
        .blobs()
        .add_path_with_opts(AddPathOptions {
            path: scratch.clone(),
            format: BlobFormat::Raw,
            mode: ImportMode::Copy,
        })
        .temp_tag()
        .await?;
    assert_eq!(tag.hash(), hash, "sanity: same content, same hash");
    drop(tag);
    let data_file = data_dir
        .join("blobs")
        .join("data")
        .join(format!("{}.data", hash.to_hex()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let deleted = match std::fs::remove_file(&data_file) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false, // handle still open (Windows); retry
        };
        if deleted && store.blobs().export_chunk(hash, 0).await.is_err() {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!(
                "could not put the store into the listed-but-unreadable state (data file \
                 {} still readable after 20s)",
                data_file.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        store.blobs().has(hash).await?,
        "precondition: the store must still LIST the hash — that is the whole bug"
    );
    Ok(())
}

/// A newly-added member fetching from the only complete member must get the file
/// even though that member's store had lost the blob's bytes: the refused fetch
/// itself triggers the repair.
#[tokio::test]
#[ignore = "opens real iroh endpoints and touches the OS keystore; run with --ignored"]
async fn a_refused_fetch_makes_the_serving_member_repair_the_blob() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let c_data = tempfile::tempdir()?;
    let c_folder = tempfile::tempdir()?;
    let bytes = content();
    let hash = Hash::new(&bytes);
    std::fs::write(a_folder.path().join(FILE), &bytes)?;

    // A: the one complete member, holding the file on disk.
    let mut a = Engine::new(a_data.path()).await?;
    let created = a.create_share(a_folder.path(), vec![]).await?;
    let share_id = created.share_id.clone();
    let a_addr = a.endpoint_addr();
    assert_eq!(percent(&a, &share_id), 100, "sanity: A is complete");

    // A's store loses the blob's bytes but keeps the entry — the field state.
    break_blob(&a, a_data.path(), hash, &bytes).await?;

    // C: a newly-added master, empty folder, A is its only source.
    let mut c = Engine::new(c_data.path()).await?;
    c.add_share(&created.master_key, c_folder.path(), vec![a_addr])
        .await?;
    let c_file = c_folder.path().join(FILE);

    // Drive C the way its daemon would, and give A only the refused-fetch repair
    // (NOT its own reconcile/health pass — that path has its own test below).
    // Before the fix this loop times out: every fetch is reset with error 3.
    let got = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let _ = c.reconcile(&share_id).await;
            let _ = a.repair_refused_blobs().await;
            if std::fs::read(&c_file).ok().as_deref() == Some(bytes.as_slice()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    assert!(
        got.is_ok(),
        "C never received {FILE}: A lists the blob but cannot read it, and refusing the \
         fetch must make A re-import the file from its disk so C's retry succeeds — \
         otherwise a share with one complete member is stuck for every newcomer, forever"
    );
    assert!(
        a.debug_blob_store()
            .blobs()
            .export_chunk(hash, 0)
            .await
            .is_ok(),
        "A's store must be able to read the blob again after the repair"
    );

    let _ = c.remove_share(&share_id, false).await;
    let _ = a.remove_share(&share_id, false).await;
    c.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// Nobody has to ask: the member's own health pass must notice a listed blob it
/// cannot read, re-import it, and only then keep claiming 100%.
#[tokio::test]
#[ignore = "opens a real iroh endpoint and touches the OS keystore; run with --ignored"]
async fn the_health_pass_repairs_a_listed_but_unreadable_blob() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let folder = tempfile::tempdir()?;
    let bytes = content();
    let hash = Hash::new(&bytes);
    std::fs::write(folder.path().join(FILE), &bytes)?;

    init_tracing();
    let mut engine = Engine::new(data.path()).await?;
    let share_id = engine.create_share(folder.path(), vec![]).await?.share_id;
    assert_eq!(percent(&engine, &share_id), 100, "sanity");

    break_blob(&engine, data.path(), hash, &bytes).await?;

    // One ordinary pass. Before the fix: 100% (has() says yes) and the blob stays
    // unreadable. After: the read probe fails, the file is re-imported from disk,
    // and 100% is true again.
    let _ = engine.reconcile(&share_id).await;
    assert!(
        engine
            .debug_blob_store()
            .blobs()
            .export_chunk(hash, 0)
            .await
            .is_ok(),
        "the health pass must repair a blob the store lists but cannot read — crediting \
         it on `has()` alone reports Healthy 100% over content no peer can fetch from us"
    );
    assert_eq!(percent(&engine, &share_id), 100);

    let _ = engine.remove_share(&share_id, false).await;
    engine.shutdown().await?;
    Ok(())
}

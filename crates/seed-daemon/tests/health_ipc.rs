//! The long-term health notification pipeline end-to-end across REAL daemon
//! processes and the IPC protocol — the automatable half of the notification
//! feature (the GUI toast/OS-notification rendering on top of these events is
//! verified manually). Three daemons (masters A+B, viewer C) with the health
//! thresholds shrunk to seconds via `SEED_HEALTH_*` env:
//!
//! 1. a frozen (paused) viewer raises `PeerHealth` events on BOTH masters'
//!    subscribe streams, and `GetPeerHealth` reports the open episode;
//! 2. resuming it produces `recovered` events where the alerts fired;
//! 3. a 1-vs-1 master split makes the live master alert about ITSELF
//!    (`is_self`) without blaming its peer.
//!
//! `#[ignore]` because it spawns processes + opens real iroh endpoints; run:
//!   cargo test -p seed-daemon --test health_ipc -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use seed_harness::proc::{request, wait_for_socket, DaemonSpawn, Daemons};
use seed_ipc::transport::{self, read_frame, write_frame};
use seed_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message};

fn daemon_bin() -> &'static str {
    env!("CARGO_BIN_EXE_seed-daemon")
}

/// Subscribe to a daemon's event stream, collecting every `PeerHealth` event
/// into a shared vec (the reader task lives until the connection drops).
async fn subscribe_health(socket: &Path) -> anyhow::Result<Arc<Mutex<Vec<IpcEvent>>>> {
    let stream = transport::connect(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Frame {
            id: 7,
            body: Message::Request(IpcRequest::Subscribe),
        },
    )
    .await?;
    let sink = Arc::new(Mutex::new(Vec::new()));
    let out = sink.clone();
    tokio::spawn(async move {
        // Keep the writer half alive for the connection's lifetime.
        let _writer = writer;
        while let Ok(Some(frame)) = read_frame(&mut reader).await {
            if let Message::Event(ev @ IpcEvent::PeerHealth { .. }) = frame.body {
                sink.lock().unwrap().push(ev);
            }
        }
    });
    Ok(out)
}

fn spawn(base: &Path, name: &str) -> (PathBuf, std::process::Child) {
    let sock = base.join(format!("{name}.sock"));
    let child = DaemonSpawn::new(daemon_bin(), base.join(format!("{name}-data")), &sock)
        .env("SEED_HEALTH_UNHEALTHY_SECS", "6")
        .env("SEED_HEALTH_RENOTIFY_SECS", "5")
        .rust_log("seed_core=info,seed_daemon=info")
        .spawn()
        .expect("spawn daemon");
    (sock, child)
}

/// Poll until `pred` over the collected events holds, or fail after `secs`.
async fn wait_events<F>(
    sink: &Arc<Mutex<Vec<IpcEvent>>>,
    secs: u64,
    label: &str,
    mut pred: F,
) -> anyhow::Result<()>
where
    F: FnMut(&[IpcEvent]) -> bool,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if pred(&sink.lock().unwrap()) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let seen = sink.lock().unwrap().len();
    anyhow::bail!("timed out waiting for: {label} ({seen} health events seen)")
}

fn is_remote_alert(ev: &IpcEvent) -> bool {
    matches!(
        ev,
        IpcEvent::PeerHealth {
            is_self: false,
            recovered: false,
            ..
        }
    )
}

fn is_remote_recovery(ev: &IpcEvent) -> bool {
    matches!(
        ev,
        IpcEvent::PeerHealth {
            is_self: false,
            recovered: true,
            ..
        }
    )
}

#[tokio::test]
#[ignore = "spawns processes + opens real iroh endpoints; slow (waits out the 45s divergence settle); run with --ignored"]
async fn unhealthy_member_notifies_masters_and_self_over_ipc() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path();
    for d in ["a-folder", "b-folder", "c-folder"] {
        std::fs::create_dir_all(base.join(d))?;
    }

    let (a_sock, a) = spawn(base, "a");
    let (b_sock, b) = spawn(base, "b");
    let (c_sock, c) = spawn(base, "c");
    let _daemons = Daemons(vec![a, b, c]);
    for s in [&a_sock, &b_sock, &c_sock] {
        wait_for_socket(s, Duration::from_secs(60)).await?;
    }

    // A creates; B joins as co-master; C joins as viewer.
    let IpcResponse::NodeAddr(a_addr) = request(&a_sock, IpcRequest::NodeAddr).await? else {
        anyhow::bail!("expected NodeAddr");
    };
    let IpcResponse::ShareCreated {
        share_id,
        master_key,
        viewer_key,
    } = request(
        &a_sock,
        IpcRequest::CreateShare {
            folder: base.join("a-folder").to_string_lossy().into_owned(),
            generate_ignore: false,
            ignore: vec![],
        },
    )
    .await?
    else {
        anyhow::bail!("expected ShareCreated");
    };
    let IpcResponse::ShareAdded { .. } = request(
        &b_sock,
        IpcRequest::AddShare {
            key: master_key,
            folder: base.join("b-folder").to_string_lossy().into_owned(),
            bootstrap: Some(a_addr.clone()),
        },
    )
    .await?
    else {
        anyhow::bail!("expected ShareAdded (B)");
    };
    let IpcResponse::ShareAdded { .. } = request(
        &c_sock,
        IpcRequest::AddShare {
            key: viewer_key,
            folder: base.join("c-folder").to_string_lossy().into_owned(),
            bootstrap: Some(a_addr),
        },
    )
    .await?
    else {
        anyhow::bail!("expected ShareAdded (C)");
    };

    let a_events = subscribe_health(&a_sock).await?;
    let b_events = subscribe_health(&b_sock).await?;

    // Let the empty share settle (presence all-to-all, equal fingerprints),
    // then freeze the viewer and move the share out from under it.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let IpcResponse::Ok = request(
        &c_sock,
        IpcRequest::Pause {
            share_id: share_id.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("expected Ok pausing C");
    };
    std::fs::write(base.join("a-folder/advance.txt"), b"c misses this")?;

    // 1. Both masters alert about the frozen viewer.
    wait_events(&a_events, 60, "master A alerts about the viewer", |evs| {
        evs.iter().any(is_remote_alert)
    })
    .await?;
    wait_events(&b_events, 60, "master B alerts about the viewer", |evs| {
        evs.iter().any(is_remote_alert)
    })
    .await?;
    println!("both masters received PeerHealth alerts over IPC OK");

    // The poll path reports the same episode.
    let IpcResponse::PeerHealth(rows) = request(
        &a_sock,
        IpcRequest::GetPeerHealth {
            share_id: share_id.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("expected PeerHealth response");
    };
    assert!(
        rows.iter().any(|r| !r.node_id.is_empty() && r.alerted),
        "GetPeerHealth must show the alerted episode (got {rows:?})"
    );
    println!("GetPeerHealth poll path OK");

    // 2. Resume → recovery events on both masters.
    let IpcResponse::Ok = request(
        &c_sock,
        IpcRequest::Resume {
            share_id: share_id.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("expected Ok resuming C");
    };
    wait_events(&a_events, 90, "master A sees recovery", |evs| {
        evs.iter().any(is_remote_recovery)
    })
    .await?;
    wait_events(&b_events, 90, "master B sees recovery", |evs| {
        evs.iter().any(is_remote_recovery)
    })
    .await?;
    println!("recovery events on both masters OK");

    // 3. Self-alert: freeze master B and advance A → a 1-vs-1 fingerprint split.
    //    A must alert about ITSELF (OutOfSync past the settle window) and must
    //    NOT blame B (no strict majority between two masters).
    let a_before = a_events.lock().unwrap().len();
    let IpcResponse::Ok = request(
        &b_sock,
        IpcRequest::Pause {
            share_id: share_id.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("expected Ok pausing B");
    };
    std::fs::write(base.join("a-folder/split.txt"), b"fork")?;
    wait_events(&a_events, 120, "master A self-alerts on the split", |evs| {
        evs.iter().any(|e| {
            matches!(
                e,
                IpcEvent::PeerHealth {
                    is_self: true,
                    recovered: false,
                    ..
                }
            )
        })
    })
    .await?;
    let new_remote_blames = a_events.lock().unwrap()[a_before..]
        .iter()
        .filter(|e| is_remote_alert(e))
        .count();
    assert_eq!(
        new_remote_blames, 0,
        "a 1-vs-1 master split must not blame the other master"
    );
    println!("self-alert without misattribution OK");
    Ok(())
}

//! Checkpoint #1: two *separate daemon processes* sync a share, driven entirely
//! over the IPC protocol (the same path the GUI/CLI use). This is the real
//! cross-process loopback — distinct from the in-process engine test in
//! `seed-core`.
//!
//! `#[ignore]` because it spawns processes and opens real iroh endpoints; run:
//!   cargo test -p seed-daemon --test loopback_ipc -- --ignored --nocapture

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use seed_ipc::transport::{self, read_frame, write_frame};
use seed_ipc::{Frame, IpcRequest, IpcResponse, Message};

/// Kills spawned daemons when dropped (even on panic).
struct Daemons(Vec<Child>);
impl Drop for Daemons {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn daemon_bin() -> &'static str {
    env!("CARGO_BIN_EXE_seed-daemon")
}

fn spawn_daemon(data_dir: &Path, socket: &Path) -> Child {
    Command::new(daemon_bin())
        .args(["run", "--data-dir"])
        .arg(data_dir)
        .arg("--socket")
        .arg(socket)
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn seed-daemon")
}

/// File-set snapshot (files only; directories aren't part of the mirror model).
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    for dent in walkdir_min(root) {
        if dent.is_file() {
            let rel = dent
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            map.insert(rel, std::fs::read(&dent).unwrap());
        }
    }
    map
}

/// Minimal recursive file walk (avoids a walkdir dep in this crate's tests).
fn walkdir_min(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

async fn request(socket: &Path, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let stream = transport::connect(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Frame {
            id: 1,
            body: Message::Request(req),
        },
    )
    .await?;
    loop {
        let Some(frame) = read_frame(&mut reader).await? else {
            anyhow::bail!("daemon closed without responding");
        };
        if frame.id == 1 {
            if let Message::Response(r) = frame.body {
                return Ok(r);
            }
        }
    }
}

async fn wait_for_socket(socket: &Path) -> anyhow::Result<()> {
    for _ in 0..120 {
        if socket.exists() {
            // Confirm it actually answers.
            if request(socket, IpcRequest::ListShares).await.is_ok() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("daemon socket {} never became ready", socket.display())
}

async fn wait_until_match(b_folder: &Path, want: &BTreeMap<String, Vec<u8>>) -> anyhow::Result<()> {
    for _ in 0..120 {
        if &snapshot(b_folder) == want {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!(
        "timed out; have {:?}, want {:?}",
        snapshot(b_folder).keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    )
}

#[tokio::test]
#[ignore = "spawns processes + opens real iroh endpoints; run with --ignored"]
async fn two_daemons_sync_over_ipc() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path();
    let a_data = base.join("a-data");
    let b_data = base.join("b-data");
    let a_folder = base.join("a-folder");
    let b_folder = base.join("b-folder");
    for d in [&a_data, &b_data, &a_folder, &b_folder] {
        std::fs::create_dir_all(d)?;
    }
    let a_sock = base.join("a.sock");
    let b_sock = base.join("b.sock");

    // Seed master content.
    std::fs::write(a_folder.join("readme.txt"), b"hello")?;
    std::fs::create_dir_all(a_folder.join("bin"))?;
    std::fs::write(a_folder.join("bin/tool.sh"), b"#!/bin/sh\necho hi\n")?;

    let _daemons = Daemons(vec![
        spawn_daemon(&a_data, &a_sock),
        spawn_daemon(&b_data, &b_sock),
    ]);

    wait_for_socket(&a_sock).await?;
    wait_for_socket(&b_sock).await?;

    // Master address + create share.
    let IpcResponse::NodeAddr(a_addr) = request(&a_sock, IpcRequest::NodeAddr).await? else {
        anyhow::bail!("expected NodeAddr");
    };
    let IpcResponse::ShareCreated { viewer_key, .. } = request(
        &a_sock,
        IpcRequest::CreateShare {
            folder: a_folder.to_string_lossy().into_owned(),
            generate_ignore: false,
            ignore: vec![],
        },
    )
    .await?
    else {
        anyhow::bail!("expected ShareCreated");
    };

    // Viewer joins.
    let resp = request(
        &b_sock,
        IpcRequest::AddShare {
            key: viewer_key,
            folder: b_folder.to_string_lossy().into_owned(),
            bootstrap: Some(a_addr),
        },
    )
    .await?;
    assert!(matches!(resp, IpcResponse::ShareAdded { .. }), "{resp:?}");

    // 1. Initial sync.
    wait_until_match(&b_folder, &snapshot(&a_folder)).await?;

    // 2. Update + add + delete on master; reconcile loop auto-republishes.
    std::fs::write(a_folder.join("readme.txt"), b"hello v2")?;
    std::fs::write(a_folder.join("notes.md"), b"# notes")?;
    std::fs::remove_file(a_folder.join("bin/tool.sh"))?;
    wait_until_match(&b_folder, &snapshot(&a_folder)).await?;
    assert!(!b_folder.join("bin/tool.sh").exists());

    // 3. Viewer-edit revert: a rogue local file must be discarded.
    std::fs::write(b_folder.join("rogue.txt"), b"nope")?;
    wait_until_match(&b_folder, &snapshot(&a_folder)).await?;
    assert!(!b_folder.join("rogue.txt").exists());

    Ok(())
}

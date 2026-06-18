//! Async transport for the IPC channel: length-prefixed CBOR [`Frame`]s over an
//! `interprocess` local socket (Unix domain socket on Linux, named pipe on
//! Windows). Enabled by the `transport` feature.
//!
//! Framing is a `u32` big-endian length followed by the CBOR payload. Read and
//! write halves can be split (`tokio::io::split`) so a connection can stream
//! server-pushed events while still reading requests.

use std::io;
use std::path::Path;

use interprocess::local_socket::tokio::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{ListenerOptions, Name};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{decode, encode, Frame};

pub use interprocess::local_socket::tokio::{Listener, Stream};

fn io_other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Write a single frame (length-prefixed CBOR).
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let bytes = encode(frame).map_err(io_other)?;
    w.write_u32(bytes.len() as u32).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Read a single frame. Returns `Ok(None)` on a clean EOF (peer closed).
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let len = match r.read_u32().await {
        Ok(len) => len,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let frame = decode(&buf).map_err(io_other)?;
    Ok(Some(frame))
}

/// Derive the platform socket [`Name`] from the `--socket` path.
///
/// On Unix the daemon listens on a Unix domain socket, so the name *is* the
/// filesystem path. On Windows a named pipe lives in a flat namespace
/// (`\\.\pipe\<name>`), not the filesystem, so a raw path is not a valid pipe
/// name; we hash the requested path into a stable, legal name. `bind` and
/// `connect` both call this, so the daemon and its clients always agree on the
/// pipe name as long as they were launched with the same `--socket` argument.
#[cfg(unix)]
fn socket_name(path: &Path) -> io::Result<Name<'_>> {
    path.to_fs_name::<GenericFilePath>()
}

#[cfg(windows)]
fn socket_name(path: &Path) -> io::Result<Name<'static>> {
    // FNV-1a over the path's bytes: deterministic across binaries and toolchains
    // (unlike `DefaultHasher`, whose keys aren't a stability guarantee), so a
    // separately-built GUI and daemon resolve the same pipe from the same path.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("seed-sync-{hash:016x}.sock").to_ns_name::<GenericNamespaced>()
}

/// Connect to a daemon listening on the socket at `path`.
pub async fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(socket_name(path)?).await
}

/// Named-pipe DACL applied on Windows: SYSTEM and Administrators get full
/// control; Authenticated Users get read/write (connect + duplex IO, but *not*
/// `FILE_CREATE_PIPE_INSTANCE`, so they cannot stand up a rogue server). This is
/// what lets the logged-in user's unprivileged GUI/CLI open a pipe created by a
/// daemon running as LocalSystem.
#[cfg(windows)]
const PIPE_SDDL: &str = "D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;AU)";

/// Bind a listener at the socket `path`. On Unix a stale socket file from a
/// previous run is removed first so rebinding succeeds. On Windows the pipe is
/// created with a permissive DACL (see [`PIPE_SDDL`]) so cross-account
/// (service ↔ user) IPC works.
pub fn bind(path: &Path) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    let opts = ListenerOptions::new().name(socket_name(path)?);
    #[cfg(windows)]
    let opts = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let sddl = widestring::U16CString::from_str(PIPE_SDDL).map_err(io_other)?;
        opts.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
    };
    opts.create_tokio()
}

/// Accept the next incoming connection (wraps the prelude trait method so
/// callers don't need it in scope).
pub async fn accept(listener: &Listener) -> io::Result<Stream> {
    listener.accept().await
}

/// Open a one-shot connection, send a single request, and return its correlated
/// response (skipping any pushed events). Used by the CLI and GUI command paths.
pub async fn oneshot_request(
    path: &Path,
    req: crate::IpcRequest,
) -> io::Result<crate::IpcResponse> {
    let stream = connect(path).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Frame {
            id: 1,
            body: crate::Message::Request(req),
        },
    )
    .await?;
    loop {
        let Some(frame) = read_frame(&mut reader).await? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed without responding",
            ));
        };
        if frame.id == 1 {
            if let crate::Message::Response(resp) = frame.body {
                return Ok(resp);
            }
        }
    }
}

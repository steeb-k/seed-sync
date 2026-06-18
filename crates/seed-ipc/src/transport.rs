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
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
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

/// Connect to a daemon listening on the socket at `path`.
pub async fn connect(path: &Path) -> io::Result<Stream> {
    let name = path.to_fs_name::<GenericFilePath>()?;
    Stream::connect(name).await
}

/// Bind a listener at the socket `path`. On Unix a stale socket file from a
/// previous run is removed first so rebinding succeeds.
pub fn bind(path: &Path) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    let name = path.to_fs_name::<GenericFilePath>()?;
    ListenerOptions::new().name(name).create_tokio()
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

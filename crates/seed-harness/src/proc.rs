//! Drive real `seed-daemon` processes over IPC — the shared driver behind the
//! daemon integration tests and the `seed-soak` bin. Extracted from the
//! `loopback_ipc` test so process spawning, socket readiness, and one-shot
//! requests live in one place.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use seed_ipc::transport::{self, read_frame, write_frame};
use seed_ipc::{Frame, IpcRequest, IpcResponse, Message};

/// Kills its daemons when dropped (even on panic), so a failed test or an
/// interrupted soak never leaves stray processes behind.
pub struct Daemons(pub Vec<Child>);

impl Drop for Daemons {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Options for spawning one daemon instance.
pub struct DaemonSpawn {
    /// Path to the `seed-daemon` binary. Tests use `env!("CARGO_BIN_EXE_…")`;
    /// the soak passes an explicit release-build path.
    pub bin: PathBuf,
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    /// Extra environment (e.g. `SEED_HEALTH_UNHEALTHY_SECS` overrides).
    pub envs: Vec<(String, String)>,
    /// `RUST_LOG` filter; also where stdout/stderr go when `log_file` is set.
    pub rust_log: String,
    /// Redirect the daemon's stdout+stderr into this file (soaks); `None`
    /// inherits the parent's (tests, where `--nocapture` shows it).
    pub log_file: Option<PathBuf>,
}

impl DaemonSpawn {
    pub fn new(
        bin: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        socket: impl Into<PathBuf>,
    ) -> Self {
        DaemonSpawn {
            bin: bin.into(),
            data_dir: data_dir.into(),
            socket: socket.into(),
            envs: Vec::new(),
            rust_log: "warn".into(),
            log_file: None,
        }
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.envs.push((k.into(), v.into()));
        self
    }

    pub fn rust_log(mut self, filter: impl Into<String>) -> Self {
        self.rust_log = filter.into();
        self
    }

    pub fn log_to(mut self, file: impl Into<PathBuf>) -> Self {
        self.log_file = Some(file.into());
        self
    }

    pub fn spawn(self) -> anyhow::Result<Child> {
        std::fs::create_dir_all(&self.data_dir)?;
        let mut cmd = Command::new(&self.bin);
        cmd.args(["run", "--data-dir"])
            .arg(&self.data_dir)
            .arg("--socket")
            .arg(&self.socket)
            .env("RUST_LOG", &self.rust_log);
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        if let Some(log) = &self.log_file {
            let out = std::fs::File::create(log)?;
            let err = out.try_clone()?;
            cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        Ok(cmd.spawn()?)
    }
}

/// One-shot request/response over a fresh connection (the CLI's model).
pub async fn request(socket: &Path, req: IpcRequest) -> anyhow::Result<IpcResponse> {
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

/// Wait until the daemon behind `socket` answers IPC, by actually talking to it:
/// a Windows named pipe has no filesystem entry, so an existence check would
/// never turn true. Fails after `timeout`.
pub async fn wait_for_socket(socket: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if request(socket, IpcRequest::ListShares).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("daemon socket {} never became ready", socket.display())
}

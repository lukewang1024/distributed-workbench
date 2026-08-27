use anyhow::{Context, Result, anyhow};
use interprocess::local_socket::{
    GenericFilePath, ListenerNonblockingMode, ListenerOptions, Stream, prelude::*,
};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;
use workbench_protocol::{Request, Response, RpcError};

pub struct RpcServer {
    socket: PathBuf,
}

impl RpcServer {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn serve<F>(&self, handler: F) -> Result<()>
    where
        F: Fn(Request) -> Response + Send + Sync + 'static,
    {
        self.serve_until(handler, Arc::new(AtomicBool::new(false)))
    }

    pub fn serve_until<F>(&self, handler: F, shutdown: Arc<AtomicBool>) -> Result<()>
    where
        F: Fn(Request) -> Response + Send + Sync + 'static,
    {
        if let Some(parent) = self.socket.parent() {
            fs::create_dir_all(parent)?;
        }
        let ipc_path = ipc_path(&self.socket);
        let name = ipc_path
            .as_os_str()
            .to_fs_name::<GenericFilePath>()
            .with_context(|| format!("map local IPC name {}", self.socket.display()))?;
        let options = ListenerOptions::new().name(name).try_overwrite(true);
        #[cfg(windows)]
        let options = windows_pipe_permissions(options)?;
        let listener = options
            .create_sync()
            .with_context(|| format!("bind local IPC {}", self.socket.display()))?;
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
        set_owner_only_permissions(&self.socket)?;
        let handler = Arc::new(handler);
        while !shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok(stream) => {
                    // Some BSD-family kernels inherit O_NONBLOCK from the
                    // listener even when only accept polling was requested.
                    // RPC streams themselves use blocking line-oriented I/O.
                    stream.set_nonblocking(false)?;
                    let handler = Arc::clone(&handler);
                    thread::spawn(move || {
                        if let Err(error) = handle_stream(stream, handler) {
                            eprintln!("workbench RPC connection failed: {error:#}");
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => eprintln!("workbench RPC accept failed: {error}"),
            }
        }
        Ok(())
    }
}

fn handle_stream<F>(mut stream: Stream, handler: Arc<F>) -> Result<()>
where
    F: Fn(Request) -> Response,
{
    let mut line = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut line)
        .context("read request")?;
    let response = match serde_json::from_str::<Request>(&line) {
        Ok(request) => handler(request),
        Err(error) => Response::failure(
            "unknown",
            RpcError::new("INVALID_REQUEST", format!("invalid JSON request: {error}")),
        ),
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

pub fn call_unix(socket: impl AsRef<Path>, request: &Request) -> Result<Response> {
    let ipc_path = ipc_path(socket.as_ref());
    let name = ipc_path
        .as_os_str()
        .to_fs_name::<GenericFilePath>()
        .with_context(|| format!("map local IPC name {}", socket.as_ref().display()))?;
    let mut stream = Stream::connect(name)
        .with_context(|| format!("connect local IPC {}", socket.as_ref().display()))?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err(anyhow!("server closed connection without a response"));
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(unix)]
fn ipc_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(windows)]
fn ipc_path(path: &Path) -> PathBuf {
    use workbench_core::sha256_bytes;

    let digest = sha256_bytes(path.to_string_lossy().to_ascii_lowercase().as_bytes());
    PathBuf::from(format!(r"\\.\pipe\distributed-workbench-{digest}"))
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(windows)]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn windows_pipe_permissions(options: ListenerOptions<'_>) -> Result<ListenerOptions<'_>> {
    use interprocess::os::windows::{
        local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
    };
    use widestring::U16CString;

    // LocalSystem, administrators, and authenticated local users may exchange
    // RPC frames. SSH still authenticates cross-node access; the pipe is never
    // exposed on the network.
    let sddl = U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)")?;
    let descriptor = SecurityDescriptor::deserialize(&sddl)?;
    Ok(options.security_descriptor(descriptor))
}

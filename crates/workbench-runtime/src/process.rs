use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use workbench_core::{atomic_replace, now_ms};
use workbench_protocol::RpcError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRecord {
    pub id: String,
    pub pid: u32,
    /// OS process birth identity; old records deliberately remain unverified.
    #[serde(default)]
    pub birth_identity: Option<String>,
    #[serde(default)]
    pub identity_verified: bool,
    #[serde(default)]
    pub observed_at: Option<u64>,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub restartable: bool,
    #[serde(default)]
    pub metadata: Value,
    pub log_path: PathBuf,
    #[serde(default)]
    pub log_start_offset: u64,
    pub state: ProcessState,
    pub readiness: ReadinessStatus,
    #[serde(default = "starting_phase")]
    pub phase: String,
    #[serde(default)]
    pub last_successful_probe_at: Option<u64>,
    #[serde(default)]
    pub last_log_progress: Option<u64>,
    #[serde(default)]
    readiness_spec: Option<Value>,
    #[serde(default)]
    readiness_deadline_at: Option<u64>,
    pub started_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessState {
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReadinessState {
    Pending,
    Ready,
    Failed,
    NotConfigured,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessStatus {
    pub state: ReadinessState,
    pub attempts: u64,
}

#[derive(Debug, Default)]
pub struct ProcessTable {
    records: Mutex<BTreeMap<String, ProcessRecord>>,
    state_path: Option<PathBuf>,
}

impl ProcessTable {
    pub fn open(state_path: PathBuf) -> Result<Self, RpcError> {
        let records = if state_path.exists() {
            serde_json::from_slice(
                &fs::read(&state_path)
                    .map_err(|error| RpcError::new("PROCESS_STATE_FAILED", error.to_string()))?,
            )
            .map_err(|error| RpcError::new("PROCESS_STATE_FAILED", error.to_string()))?
        } else {
            BTreeMap::new()
        };
        let table = Self {
            records: Mutex::new(records),
            state_path: Some(state_path),
        };
        table.refresh_all()?;
        Ok(table)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        id: String,
        cwd: PathBuf,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        log_path: PathBuf,
        readiness: Option<Value>,
        restartable: bool,
        metadata: Value,
    ) -> Result<ProcessRecord, RpcError> {
        if argv.is_empty() {
            return Err(RpcError::new("INVALID_PARAMS", "argv must not be empty"));
        }
        if let Ok(record) = self.get(&id)
            && record.state == ProcessState::Running
        {
            return Err(RpcError::new(
                if record.identity_verified {
                    "PROCESS_ALREADY_RUNNING"
                } else {
                    "PROCESS_IDENTITY_UNKNOWN"
                },
                format!("process {id} requires reconciliation before replacement"),
            ));
        }
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| RpcError::new("LOG_CREATE_FAILED", error.to_string()))?;
        }
        let log_start_offset = fs::metadata(&log_path)
            .map(|value| value.len())
            .unwrap_or(0);
        let stdout = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| RpcError::new("LOG_CREATE_FAILED", error.to_string()))?;
        let stderr = stdout
            .try_clone()
            .map_err(|error| RpcError::new("LOG_CREATE_FAILED", error.to_string()))?;
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(&cwd)
            .envs(&env)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| RpcError::new("PROCESS_START_FAILED", error.to_string()))?;
        let now = now_ms();
        let readiness_deadline_at = readiness.as_ref().map(|spec| {
            now.saturating_add(
                spec.get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(180_000),
            )
        });
        let record = ProcessRecord {
            id: id.clone(),
            pid: child.id(),
            birth_identity: process_birth_identity(child.id()),
            identity_verified: false,
            observed_at: None,
            cwd,
            argv,
            env,
            restartable,
            metadata,
            log_path,
            log_start_offset,
            state: ProcessState::Running,
            readiness: ReadinessStatus {
                state: if readiness.is_some() {
                    ReadinessState::Pending
                } else {
                    ReadinessState::NotConfigured
                },
                attempts: 0,
            },
            phase: "STARTING".to_owned(),
            last_successful_probe_at: None,
            last_log_progress: Some(log_start_offset),
            readiness_spec: readiness,
            readiness_deadline_at,
            started_at: now,
            updated_at: now,
        };
        // Dropping Child does not reap an exited process on Unix. Retain the
        // wait handle independently of the durable record so failed tunnels
        // cannot accumulate zombies in a long-running Executor.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let mut records = self.records.lock().expect("process lock");
        records.insert(id, record.clone());
        self.persist(&records)?;
        Ok(record)
    }

    pub fn get(&self, id: &str) -> Result<ProcessRecord, RpcError> {
        let mut records = self.records.lock().expect("process lock");
        let record = records
            .get_mut(id)
            .ok_or_else(|| RpcError::new("PROCESS_NOT_FOUND", format!("unknown process: {id}")))?;
        refresh(record);
        let result = record.clone();
        self.persist(&records)?;
        Ok(result)
    }

    pub fn list(&self) -> Vec<ProcessRecord> {
        let mut records = self.records.lock().expect("process lock");
        for record in records.values_mut() {
            refresh(record);
        }
        let _ = self.persist(&records);
        records.values().cloned().collect()
    }

    pub fn wait_ready(&self, id: &str, timeout_ms: u64) -> Result<ProcessRecord, RpcError> {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let record = self.get(id)?;
            if record.readiness.state == ReadinessState::Ready {
                return Ok(record);
            }
            if record.state != ProcessState::Running {
                return Err(process_error("PROCESS_EXITED", &record, false));
            }
            if record.readiness.state == ReadinessState::Failed {
                return Err(process_error("READINESS_MARKER_MISSING", &record, true));
            }
            if std::time::Instant::now() >= deadline {
                let code = if record.last_log_progress.unwrap_or(record.log_start_offset)
                    <= record.log_start_offset
                {
                    "BUILD_STALLED"
                } else {
                    "READINESS_MARKER_MISSING"
                };
                return Err(process_error(code, &record, true));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn stop(&self, id: &str) -> Result<ProcessRecord, RpcError> {
        let mut records = self.records.lock().expect("process lock");
        let record = records
            .get_mut(id)
            .ok_or_else(|| RpcError::new("PROCESS_NOT_FOUND", format!("unknown process: {id}")))?;
        refresh(record);
        if record.state == ProcessState::Running {
            if !record.identity_verified {
                return Err(RpcError::new(
                    "PROCESS_IDENTITY_UNKNOWN",
                    "cannot signal an unverified recorded PID",
                ));
            }
            stop_process(record.pid)?;
            record.state = ProcessState::Stopped;
            record.updated_at = now_ms();
        }
        let result = record.clone();
        self.persist(&records)?;
        Ok(result)
    }

    pub fn update_metadata(&self, id: &str, metadata: Value) -> Result<ProcessRecord, RpcError> {
        let mut records = self.records.lock().expect("process lock");
        let record = records
            .get_mut(id)
            .ok_or_else(|| RpcError::new("PROCESS_NOT_FOUND", format!("unknown process: {id}")))?;
        record.metadata = metadata;
        record.updated_at = now_ms();
        let result = record.clone();
        self.persist(&records)?;
        Ok(result)
    }

    pub fn restart(&self, id: &str) -> Result<ProcessRecord, RpcError> {
        let record = self.get(id)?;
        if !record.restartable {
            return Err(RpcError::new(
                "PROCESS_NOT_RESTARTABLE",
                "process was not started from a declared restart template",
            ));
        }
        self.stop(id)?;
        self.start(
            record.id,
            record.cwd,
            record.argv,
            record.env,
            record.log_path,
            record.readiness_spec,
            true,
            record.metadata,
        )
    }

    pub fn logs(&self, id: &str, tail: usize) -> Result<Value, RpcError> {
        let record = self.get(id)?;
        let content = fs::read_to_string(&record.log_path)
            .map_err(|error| RpcError::new("LOG_READ_FAILED", error.to_string()))?;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail);
        Ok(json!({
            "processId": id,
            "path": record.log_path,
            "lines": &lines[start..],
        }))
    }

    fn refresh_all(&self) -> Result<(), RpcError> {
        let mut records = self.records.lock().expect("process lock");
        for record in records.values_mut() {
            refresh(record);
        }
        self.persist(&records)
    }

    fn persist(&self, records: &BTreeMap<String, ProcessRecord>) -> Result<(), RpcError> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec_pretty(records)
            .map_err(|error| RpcError::new("PROCESS_STATE_FAILED", error.to_string()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| RpcError::new("PROCESS_STATE_FAILED", error.to_string()))?;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let temporary =
            path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| RpcError::new("PROCESS_STATE_FAILED", error.to_string()))?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| RpcError::new("PROCESS_STATE_FAILED", error.to_string()))?;
        atomic_replace(&temporary, path)
            .map_err(|error| RpcError::new("PROCESS_STATE_FAILED", error.to_string()))
    }
}

pub const OBSERVATION_TTL_MS: u64 = 5_000;

fn refresh(record: &mut ProcessRecord) {
    record.observed_at = Some(now_ms());
    record.identity_verified = false;
    if record.state != ProcessState::Running {
        return;
    }
    let current_identity = process_birth_identity(record.pid);
    let identity_mismatch = matches!((&record.birth_identity, &current_identity), (Some(expected), Some(actual)) if expected != actual);
    if !process_is_alive(record.pid) || identity_mismatch {
        record.state = ProcessState::Failed;
        record.phase = "FAILED".to_owned();
        record.updated_at = now_ms();
        return;
    }
    record.identity_verified = matches!((&record.birth_identity, &current_identity), (Some(expected), Some(actual)) if expected == actual);
    if !record.identity_verified {
        return;
    }
    // Tunnel readiness is live evidence, unlike a one-time build log marker.
    if record.metadata.get("kind").and_then(Value::as_str) == Some("tunnel")
        && record.readiness_spec.is_some()
        && record.readiness.state != ReadinessState::Pending
    {
        record.readiness.state = ReadinessState::Pending;
    }
    if let Ok(length) = fs::metadata(&record.log_path).map(|value| value.len())
        && length > record.last_log_progress.unwrap_or(record.log_start_offset)
    {
        record.last_log_progress = Some(length);
        record.phase = "BUILDING".to_owned();
        record.updated_at = now_ms();
    }
    if record.readiness.state == ReadinessState::Pending {
        record.readiness.attempts = record.readiness.attempts.saturating_add(1);
        let ready = record
            .readiness_spec
            .as_ref()
            .map(|spec| readiness_once(spec, &record.log_path, record.log_start_offset))
            .transpose();
        match ready {
            Ok(Some(true)) => {
                record.readiness.state = ReadinessState::Ready;
                record.phase = "READY".to_owned();
                record.last_successful_probe_at = Some(now_ms());
            }
            Ok(Some(false))
                if record
                    .readiness_deadline_at
                    .is_some_and(|deadline| now_ms() >= deadline) =>
            {
                record.readiness.state = ReadinessState::Failed;
            }
            Err(_) => record.readiness.state = ReadinessState::Failed,
            _ => {}
        }
        record.updated_at = now_ms();
    }
}

fn starting_phase() -> String {
    "STARTING".to_owned()
}

fn process_error(code: &str, record: &ProcessRecord, retryable: bool) -> RpcError {
    let mut error = RpcError::new(code, format!("{} for process {}", code, record.id));
    error.retryable = retryable;
    error.details = json!({
        "processId": record.id,
        "lastSuccessfulProbeAt": record.last_successful_probe_at,
        "lastLogProgress": record.last_log_progress,
        "phase": record.phase,
    });
    error
}

#[cfg(unix)]
pub(crate) fn stop_process(pid: u32) -> Result<(), RpcError> {
    // Processes started by this table lead their own process group. Signal the
    // group so package managers and shells cannot leave product servers or
    // download helpers behind after a restart.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(RpcError::new("PROCESS_STOP_FAILED", error.to_string()))
    }
}

#[cfg(windows)]
pub(crate) fn stop_process(pid: u32) -> Result<(), RpcError> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .map_err(|error| RpcError::new("PROCESS_STOP_FAILED", error.to_string()))?;
    if status.success() || !process_is_alive(pid) {
        Ok(())
    } else {
        Err(RpcError::new(
            "PROCESS_STOP_FAILED",
            format!("taskkill exited with {status}"),
        ))
    }
}

#[cfg(target_os = "linux")]
fn process_birth_identity(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parentheses. Field 22 follows the final ')'.
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{}:{}", boot.trim(), fields.get(19)?))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_birth_identity(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "stat="])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let mut fields: Vec<_> = text.split_whitespace().collect();
    let state = fields.pop()?;
    if !output.status.success() || state.starts_with('Z') || fields.is_empty() {
        return None;
    }
    Some(fields.join(" "))
}

#[cfg(windows)]
fn process_birth_identity(pid: u32) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut birth: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut birth, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        (ok != 0).then(|| format!("{}:{}", birth.dwHighDateTime, birth.dwLowDateTime))
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
        && let Some((_, fields)) = stat.rsplit_once(')')
        && matches!(fields.split_whitespace().next(), Some("Z" | "X"))
    {
        return false;
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    if let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .env("LC_ALL", "C")
        .output()
        && output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with(['Z', 'X'])
    {
        // kill(pid, 0) succeeds for zombies on macOS too. Their missing birth
        // identity means exited, not an unverifiable live process.
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        })
        .unwrap_or(true)
}

fn readiness_once(spec: &Value, log_path: &Path, log_start_offset: u64) -> Result<bool, RpcError> {
    match spec.get("type").and_then(Value::as_str).unwrap_or("") {
        "all" => {
            let probes = spec
                .get("probes")
                .and_then(Value::as_array)
                .ok_or_else(|| RpcError::new("INVALID_READINESS", "all probes are required"))?;
            if probes.is_empty() {
                return Err(RpcError::new(
                    "INVALID_READINESS",
                    "all requires at least one probe",
                ));
            }
            for probe in probes {
                if !readiness_once(probe, log_path, log_start_offset)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        "tcp" => {
            let host = spec
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or("127.0.0.1");
            let port = spec
                .get("port")
                .and_then(Value::as_u64)
                .ok_or_else(|| RpcError::new("INVALID_READINESS", "tcp port is required"))?;
            let address = format!("{host}:{port}");
            Ok(address
                .to_socket_addrs()
                .map_err(|error| RpcError::new("INVALID_READINESS", error.to_string()))?
                .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()))
        }
        "file" => Ok(spec
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| Path::new(path).exists())),
        "log" => {
            let pattern = spec
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::new("INVALID_READINESS", "log pattern is required"))?;
            Ok(fs::read(log_path)
                .map(|content| {
                    let start = usize::try_from(log_start_offset)
                        .unwrap_or(usize::MAX)
                        .min(content.len());
                    let content = String::from_utf8_lossy(&content[start..]);
                    if let Some(exact) = pattern
                        .strip_prefix('^')
                        .and_then(|value| value.strip_suffix('$'))
                    {
                        content.lines().any(|line| line == exact)
                    } else {
                        content.contains(pattern)
                    }
                })
                .unwrap_or(false))
        }
        other => Err(RpcError::new(
            "INVALID_READINESS",
            format!("unsupported readiness type: {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::readiness_once;
    #[cfg(unix)]
    use super::{ProcessTable, ReadinessState};
    use serde_json::json;
    use std::fs;
    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn exited_managed_children_are_reaped_without_process_queries() {
        let root = tempfile::tempdir().unwrap();
        let table = ProcessTable::default();
        let record = table
            .start(
                "reap-test".into(),
                root.path().into(),
                vec!["sh".into(), "-c".into(), "exit 0".into()],
                Default::default(),
                root.path().join("log"),
                None,
                true,
                json!({"kind": "tunnel"}),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // Observe PID removal directly: process_is_alive deliberately hides
        // zombies and would not prove the Executor actually reaped its child.
        loop {
            if unsafe { libc::kill(record.pid as i32, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "exited child was not reaped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            table.get(&record.id).unwrap().state,
            super::ProcessState::Failed
        );
    }

    #[cfg(unix)]
    #[test]
    fn persisted_zombie_is_failed_and_restartable_without_signaling_it() {
        let root = tempfile::tempdir().unwrap();
        let state_path = root.path().join("processes.json");
        let table = ProcessTable::open(state_path.clone()).unwrap();
        let record = table
            .start(
                "zombie-test".into(),
                root.path().into(),
                vec!["sleep".into(), "30".into()],
                Default::default(),
                root.path().join("log"),
                None,
                true,
                json!({"kind": "tunnel"}),
            )
            .unwrap();
        table.stop(&record.id).unwrap();
        // Simulate an unreaped child left by an older Executor. Keep Child
        // outside ProcessTable so the new background waiter cannot reap it.
        let mut zombie = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while super::process_birth_identity(zombie.id()).is_some() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(unsafe { libc::kill(zombie.id() as i32, 0) }, 0);
        for identity in [record.birth_identity.clone(), None] {
            let mut records = table.records.lock().unwrap();
            let stored = records.get_mut(&record.id).unwrap();
            stored.pid = zombie.id();
            stored.birth_identity = identity;
            stored.state = super::ProcessState::Running;
            table.persist(&records).unwrap();
            drop(records);
            let reopened = ProcessTable::open(state_path.clone()).unwrap();
            assert_eq!(
                reopened.get(&record.id).unwrap().state,
                super::ProcessState::Failed
            );
            let replacement = reopened.restart(&record.id).unwrap();
            assert_ne!(replacement.pid, zombie.id());
            assert!(reopened.get(&record.id).unwrap().identity_verified);
            reopened.stop(&record.id).unwrap();
            // Reconciliation did not signal or reap the historical PID.
            assert_eq!(unsafe { libc::kill(zombie.id() as i32, 0) }, 0);
        }
        zombie.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn persisted_pid_identity_is_revalidated_and_never_signals_a_reused_pid() {
        let root = tempfile::tempdir().unwrap();
        let table = ProcessTable::default();
        let record = table
            .start(
                "identity-test".into(),
                root.path().into(),
                vec!["sleep".into(), "30".into()],
                Default::default(),
                root.path().join("log"),
                None,
                false,
                json!({"kind": "tunnel"}),
            )
            .unwrap();
        assert!(table.get(&record.id).unwrap().identity_verified);
        {
            let mut records = table.records.lock().unwrap();
            let stored = records.get_mut(&record.id).unwrap();
            stored.birth_identity = None;
            stored.observed_at = Some(0);
        }
        assert_eq!(
            table.stop(&record.id).unwrap_err().code,
            "PROCESS_IDENTITY_UNKNOWN"
        );
        assert!(super::process_is_alive(record.pid));
        assert!(!table.get(&record.id).unwrap().identity_verified);
        {
            let mut records = table.records.lock().unwrap();
            records.get_mut(&record.id).unwrap().birth_identity = Some("previous-process".into());
        }
        let stale = table.stop(&record.id).unwrap();
        assert_eq!(stale.state, super::ProcessState::Failed);
        assert!(super::process_is_alive(record.pid));
        super::stop_process(record.pid).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_readiness_is_reprobed_after_a_previous_success() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("ready");
        fs::write(&marker, "ready").unwrap();
        let table = ProcessTable::default();
        table
            .start(
                "live-probe".into(),
                root.path().into(),
                vec!["sleep".into(), "30".into()],
                Default::default(),
                root.path().join("log"),
                Some(json!({"type": "file", "path": marker, "timeoutMs": 0})),
                false,
                json!({"kind": "tunnel"}),
            )
            .unwrap();
        assert_eq!(
            table.get("live-probe").unwrap().readiness.state,
            ReadinessState::Ready
        );
        fs::remove_file(marker).unwrap();
        assert_eq!(
            table.get("live-probe").unwrap().readiness.state,
            ReadinessState::Failed
        );
        table.stop("live-probe").unwrap();
    }

    #[test]
    fn all_readiness_requires_every_probe() {
        let log_path = std::env::temp_dir().join(format!(
            "workbench-readiness-{}-{}.log",
            std::process::id(),
            workbench_core::now_ms()
        ));
        fs::write(&log_path, "compiler done\n").expect("write log");
        let ready = readiness_once(
            &json!({
                "type": "all",
                "probes": [
                    {"type": "file", "path": log_path},
                    {"type": "log", "pattern": "compiler done"}
                ]
            }),
            &log_path,
            0,
        )
        .expect("valid readiness");
        assert!(ready);
        let _ = fs::remove_file(log_path);
    }

    #[cfg(unix)]
    #[test]
    fn process_start_returns_pending_and_get_advances_readiness() {
        let root = std::env::temp_dir().join(format!(
            "workbench-process-{}-{}",
            std::process::id(),
            workbench_core::now_ms()
        ));
        fs::create_dir_all(&root).expect("create root");
        let log_path = root.join("process.log");
        let table = ProcessTable::default();
        let started = std::time::Instant::now();
        let record = table
            .start(
                "readiness-test".to_owned(),
                root.clone(),
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "sleep 0.2; echo ready; sleep 1".to_owned(),
                ],
                Default::default(),
                log_path,
                Some(json!({"type": "log", "pattern": "^ready$", "timeoutMs": 2_000})),
                false,
                serde_json::Value::Null,
            )
            .expect("start process");
        assert!(started.elapsed() < Duration::from_millis(150));
        assert_eq!(record.readiness.state, ReadinessState::Pending);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let record = table.get("readiness-test").expect("get process");
            if record.readiness.state == ReadinessState::Ready {
                assert!(record.readiness.attempts > 0);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process did not become ready"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        table.stop("readiness-test").expect("stop process");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn process_readiness_ignores_log_output_from_a_previous_run() {
        let root = std::env::temp_dir().join(format!(
            "workbench-process-stale-log-{}-{}",
            std::process::id(),
            workbench_core::now_ms()
        ));
        fs::create_dir_all(&root).expect("create root");
        let log_path = root.join("process.log");
        fs::write(&log_path, "compiler done\n").expect("write stale log");
        let table = ProcessTable::default();
        table
            .start(
                "stale-log-test".to_owned(),
                root.clone(),
                vec!["sh".to_owned(), "-c".to_owned(), "sleep 2".to_owned()],
                Default::default(),
                log_path,
                Some(json!({"type": "log", "pattern": "^compiler done$", "timeoutMs": 1_000})),
                false,
                serde_json::Value::Null,
            )
            .expect("start process");
        std::thread::sleep(Duration::from_millis(50));
        let record = table.get("stale-log-test").expect("get process");
        assert_eq!(record.readiness.state, ReadinessState::Pending);
        table.stop("stale-log-test").expect("stop process");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn process_records_survive_table_restart() {
        let root = std::env::temp_dir().join(format!(
            "workbench-process-state-{}-{}",
            std::process::id(),
            workbench_core::now_ms()
        ));
        fs::create_dir_all(&root).expect("create root");
        let state_path = root.join("processes.json");
        let table = ProcessTable::open(state_path.clone()).expect("open table");
        table
            .start(
                "persistent-process".to_owned(),
                root.clone(),
                vec!["sleep".to_owned(), "30".to_owned()],
                Default::default(),
                root.join("process.log"),
                None,
                false,
                json!({"kind": "tunnel", "desiredState": "running"}),
            )
            .expect("start process");
        drop(table);
        let restored = ProcessTable::open(state_path).expect("restore table");
        let record = restored.get("persistent-process").expect("restored record");
        assert_eq!(record.state, super::ProcessState::Running);
        assert_eq!(record.metadata["kind"], "tunnel");
        restored
            .update_metadata(
                "persistent-process",
                json!({"kind": "tunnel", "desiredState": "stopped"}),
            )
            .expect("update metadata");
        assert_eq!(
            restored
                .get("persistent-process")
                .expect("updated record")
                .metadata["desiredState"],
            "stopped"
        );
        restored.stop("persistent-process").expect("stop process");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn process_stop_terminates_descendants() {
        let root = std::env::temp_dir().join(format!(
            "workbench-process-group-{}-{}",
            std::process::id(),
            workbench_core::now_ms()
        ));
        fs::create_dir_all(&root).expect("create root");
        let child_pid_path = root.join("child.pid");
        let table = ProcessTable::default();
        table
            .start(
                "process-group-test".to_owned(),
                root.clone(),
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!("sleep 30 & echo $! > {}; wait", child_pid_path.display()),
                ],
                Default::default(),
                root.join("process.log"),
                None,
                false,
                serde_json::Value::Null,
            )
            .expect("start process");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !child_pid_path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "child pid was not recorded"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let child_pid: u32 = fs::read_to_string(&child_pid_path)
            .expect("read child pid")
            .trim()
            .parse()
            .expect("parse child pid");
        table.stop("process-group-test").expect("stop process");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while super::process_is_alive(child_pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "descendant survived process stop"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_dir_all(root);
    }
}

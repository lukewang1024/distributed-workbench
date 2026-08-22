use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::env;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use workbench_core::JsonStore;
use workbench_protocol::Request;
use workbench_runtime::{
    Controller, ExecutorRuntime, PeerAcceptConfig, PeerConnectConfig, RpcServer, accept_peer,
    call_unix, connect_peer, init_logging, read_peer_status,
};

#[derive(Debug, Parser)]
#[command(
    name = "workbench",
    version,
    about = "Distributed workbench control plane"
)]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Status,
    Dashboard {
        #[arg(long)]
        open: bool,
        #[arg(long, default_value = "127.0.0.1:19898")]
        listen: String,
    },
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    Observe {
        #[command(subcommand)]
        command: ObserveCommand,
    },
    Logs {
        #[arg(long)]
        component: Option<String>,
        #[arg(long)]
        correlation_id: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        connection_id: Option<String>,
        #[arg(long)]
        since_ms: Option<u64>,
        #[arg(long, default_value_t = 200)]
        tail: usize,
    },
    Call {
        action: String,
        #[arg(default_value = "{}")]
        params: String,
    },
    CallStdin,
    Controller {
        #[command(subcommand)]
        command: ControllerCommand,
    },
    Executor {
        #[command(subcommand)]
        command: ExecutorCommand,
    },
    Peer {
        #[command(subcommand)]
        command: Box<PeerCommand>,
    },
    Fabric {
        #[command(subcommand)]
        command: FabricCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RunCommand {
    Start {
        #[arg(long)]
        target_summary: String,
        #[arg(long)]
        workspace_session_id: Option<String>,
        #[arg(long)]
        agent_session_id: Option<String>,
        #[arg(long, default_value = "agent")]
        created_by: String,
    },
    Finish {
        run_id: String,
        #[arg(long, default_value = "completed")]
        status: String,
        #[arg(long)]
        business_outcome: Option<String>,
    },
    Status {
        run_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum ObserveCommand {
    CodexHook,
}

#[derive(Debug, Subcommand)]
enum ControllerCommand {
    Serve {
        #[arg(long)]
        state: Option<PathBuf>,
        #[arg(long)]
        id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ExecutorCommand {
    Serve {
        #[arg(long)]
        id: String,
        #[arg(long = "allow-root", required = true)]
        allow_roots: Vec<PathBuf>,
        #[arg(long)]
        state: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    Connect {
        #[arg(long)]
        id: String,
        #[arg(long)]
        local_id: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        expose_controller_socket: PathBuf,
        #[arg(long)]
        expose_executor_socket: PathBuf,
        #[arg(long)]
        remote_executable: Option<String>,
        #[arg(long)]
        remote_state_root: String,
        #[arg(long, value_enum, default_value_t = RemotePlatform::Posix)]
        remote_platform: RemotePlatform,
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        local_controller_socket: Option<PathBuf>,
        #[arg(long)]
        local_executor_socket: Option<PathBuf>,
    },
    Accept {
        #[arg(long)]
        id: String,
        #[arg(long)]
        local_id: String,
        #[arg(long)]
        expose_controller_socket: PathBuf,
        #[arg(long)]
        expose_executor_socket: PathBuf,
        #[arg(long)]
        local_controller_socket: Option<PathBuf>,
        #[arg(long)]
        local_executor_socket: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        state: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum FabricCommand {
    Validate {
        #[arg(long)]
        file: PathBuf,
    },
    Plan {
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FabricManifest {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    initiator_node: String,
    nodes: Vec<FabricNode>,
    topology: FabricTopology,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FabricNode {
    id: String,
    platform: FabricPlatform,
    architecture: FabricArchitecture,
    #[serde(default)]
    connection: Option<FabricConnection>,
    allow_roots: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FabricConnection {
    ssh_alias: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum FabricPlatform {
    Macos,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
enum FabricArchitecture {
    #[serde(rename = "aarch64")]
    Aarch64,
    #[serde(rename = "x86_64")]
    X86_64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FabricTopology {
    mode: FabricTopologyMode,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum FabricTopologyMode {
    FullMesh,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RemotePlatform {
    Posix,
    Windows,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    #[cfg(windows)]
    if let Some(service) = windows_service_kind(&cli.command)
        && run_windows_service(service)?
    {
        return Ok(());
    }
    run_cli(cli)
}

fn run_cli(cli: Cli) -> Result<()> {
    match &cli.command {
        Command::Controller { .. } => {
            init_logging("controller", default_log_dir().join("controller.jsonl"))
        }
        Command::Executor { .. } => {
            init_logging("executor", default_log_dir().join("executor.jsonl"))
        }
        Command::Peer { .. } => init_logging("peer", default_log_dir().join("peer.jsonl")),
        _ => {}
    }
    match cli.command {
        Command::Status => print_response(call_unix(
            cli.socket.unwrap_or_else(default_controller_socket),
            &Request::new("status", Value::Null),
        )?),
        Command::Dashboard { open, listen } => serve_dashboard(
            cli.socket.unwrap_or_else(default_controller_socket),
            &listen,
            open,
        ),
        Command::Run { command } => {
            let socket = cli.socket.unwrap_or_else(default_controller_socket);
            let request = match command {
                RunCommand::Start {
                    target_summary,
                    workspace_session_id,
                    agent_session_id,
                    created_by,
                } => Request::new(
                    "run.start",
                    serde_json::json!({"targetSummary":target_summary,"workspaceSessionId":workspace_session_id,"agentSessionId":agent_session_id,"createdBy":created_by}),
                ),
                RunCommand::Finish {
                    run_id,
                    status,
                    business_outcome,
                } => Request::new(
                    "run.finish",
                    serde_json::json!({"runId":run_id,"status":status,"businessOutcome":business_outcome}),
                ),
                RunCommand::Status {
                    run_id: Some(run_id),
                    ..
                } => Request::new("run.get", serde_json::json!({"runId":run_id})),
                RunCommand::Status {
                    run_id: None,
                    limit,
                } => Request::new("run.list", serde_json::json!({"limit":limit})),
            };
            print_response(call_unix(socket, &request)?)
        }
        Command::Observe {
            command: ObserveCommand::CodexHook,
        } => observe_codex_hook(cli.socket.unwrap_or_else(default_controller_socket)),
        Command::Logs {
            component,
            correlation_id,
            request_id,
            task_id,
            connection_id,
            since_ms,
            tail,
        } => print_logs(
            component.as_deref(),
            correlation_id.as_deref(),
            request_id.as_deref(),
            task_id.as_deref(),
            connection_id.as_deref(),
            since_ms,
            tail,
        ),
        Command::Call { action, params } => print_response(call_unix(
            cli.socket.unwrap_or_else(default_controller_socket),
            &Request::new(action, serde_json::from_str(&params)?),
        )?),
        Command::CallStdin => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let request: Request = serde_json::from_str(&line)?;
            print_response(call_unix(
                cli.socket.unwrap_or_else(default_controller_socket),
                &request,
            )?)
        }
        Command::Controller {
            command: ControllerCommand::Serve { state, id },
        } => {
            let controller = Arc::new(Controller::open_with_id(
                JsonStore::new(state.unwrap_or_else(default_controller_state)),
                id,
            )?);
            let handler = Arc::clone(&controller);
            RpcServer::new(cli.socket.unwrap_or_else(default_controller_socket))
                .serve(move |request| handler.handle(request))
        }
        Command::Executor {
            command:
                ExecutorCommand::Serve {
                    id,
                    allow_roots,
                    state,
                },
        } => {
            let executor = Arc::new(
                ExecutorRuntime::open(
                    id,
                    allow_roots,
                    state.unwrap_or_else(default_executor_state),
                )
                .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?,
            );
            let handler = Arc::clone(&executor);
            RpcServer::new(cli.socket.unwrap_or_else(default_executor_socket))
                .serve(move |request| handler.handle(request))
        }
        Command::Peer { command } => match *command {
            PeerCommand::Connect {
                id,
                local_id,
                host,
                expose_controller_socket,
                expose_executor_socket,
                remote_executable,
                remote_state_root,
                remote_platform,
                state,
                local_controller_socket,
                local_executor_socket,
            } => connect_peer(PeerConnectConfig {
                local_id,
                peer_id: id,
                host,
                local_controller_socket: local_controller_socket
                    .unwrap_or_else(default_controller_socket),
                local_executor_socket: local_executor_socket
                    .unwrap_or_else(default_executor_socket),
                expose_controller_socket,
                expose_executor_socket,
                remote_executable: remote_executable
                    .unwrap_or_else(|| ".local/bin/workbench".to_owned()),
                remote_state_root,
                remote_windows: matches!(remote_platform, RemotePlatform::Windows),
                state_path: state,
            }),
            PeerCommand::Accept {
                id,
                local_id,
                expose_controller_socket,
                expose_executor_socket,
                local_controller_socket,
                local_executor_socket,
            } => accept_peer(PeerAcceptConfig {
                peer_id: id,
                local_id,
                local_controller_socket: local_controller_socket
                    .unwrap_or_else(default_controller_socket),
                local_executor_socket: local_executor_socket
                    .unwrap_or_else(default_executor_socket),
                expose_controller_socket,
                expose_executor_socket,
            }),
            PeerCommand::Status { state } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&read_peer_status(state)?)?
                );
                Ok(())
            }
        },
        Command::Fabric {
            command: FabricCommand::Validate { file },
        } => validate_fabric_manifest(&file),
        Command::Fabric {
            command: FabricCommand::Plan { file },
        } => plan_fabric_manifest(&file),
    }
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsServiceKind {
    Controller,
    Executor,
}

#[cfg(windows)]
fn windows_service_kind(command: &Command) -> Option<WindowsServiceKind> {
    match command {
        Command::Controller {
            command: ControllerCommand::Serve { .. },
        } => Some(WindowsServiceKind::Controller),
        Command::Executor {
            command: ExecutorCommand::Serve { .. },
        } => Some(WindowsServiceKind::Executor),
        _ => None,
    }
}

#[cfg(windows)]
fn run_windows_service(kind: WindowsServiceKind) -> Result<bool> {
    use windows_service::Error as WindowsServiceError;

    let result = windows_service_entry::start(kind);
    match result {
        Ok(()) => Ok(true),
        Err(WindowsServiceError::Winapi(error))
            if error.raw_os_error() == Some(1063) /* ERROR_FAILED_SERVICE_CONTROLLER_CONNECT */ =>
        {
            Ok(false)
        }
        Err(error) => Err(anyhow::anyhow!("start Windows service dispatcher: {error}")),
    }
}

#[cfg(windows)]
mod windows_service_entry {
    use super::{Cli, WindowsServiceKind, run_cli};
    use clap::Parser;
    use std::{ffi::OsString, sync::mpsc, thread, time::Duration};
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    define_windows_service!(ffi_controller_service_main, controller_service_main);
    define_windows_service!(ffi_executor_service_main, executor_service_main);

    pub(super) fn start(kind: WindowsServiceKind) -> windows_service::Result<()> {
        match kind {
            WindowsServiceKind::Controller => service_dispatcher::start(
                "DistributedWorkbenchController",
                ffi_controller_service_main,
            ),
            WindowsServiceKind::Executor => {
                service_dispatcher::start("DistributedWorkbenchExecutor", ffi_executor_service_main)
            }
        }
    }

    pub(super) fn controller_service_main(_arguments: Vec<OsString>) {
        run_service(
            "DistributedWorkbenchController",
            WindowsServiceKind::Controller,
        );
    }

    pub(super) fn executor_service_main(_arguments: Vec<OsString>) {
        run_service("DistributedWorkbenchExecutor", WindowsServiceKind::Executor);
    }

    fn run_service(service_name: &'static str, _kind: WindowsServiceKind) {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let Ok(status_handle) = service_control_handler::register(service_name, event_handler)
        else {
            return;
        };
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });

        thread::spawn(|| {
            let cli = Cli::parse();
            run_cli(cli)
        });
        let _ = shutdown_rx.recv();
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StopPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 1,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        });
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });
    }
}

fn observe_codex_hook(socket: PathBuf) -> Result<()> {
    let input: Value = serde_json::from_reader(std::io::stdin())?;
    let event = input
        .get("hook_event_name")
        .or_else(|| input.get("event"))
        .and_then(Value::as_str)
        .unwrap_or("ToolEvent");
    let explicit_run_id = input
        .get("runId")
        .or_else(|| input.get("run_id"))
        .and_then(Value::as_str);
    let agent_session_id = input
        .get("session_id")
        .or_else(|| input.get("agentSessionId"))
        .and_then(Value::as_str);
    let state_path = agent_session_id.map(codex_hook_state_path);
    let stored = state_path
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let run_id = explicit_run_id.or_else(|| {
        stored
            .as_ref()
            .and_then(|value| value.get("runId"))
            .and_then(Value::as_str)
    });
    let request = match event {
        "UserPromptSubmit" => Request::new(
            "run.start",
            serde_json::json!({"runId":explicit_run_id,"agentSessionId":agent_session_id,"targetSummary":"Codex agent task","createdBy":"codex-hook"}),
        ),
        "Stop" | "SessionEnd" if run_id.is_some() => Request::new(
            "run.finish",
            serde_json::json!({"runId":run_id,"status":if event=="Stop"{"completed"}else{"interrupted"}}),
        ),
        _ => Request::new(
            "observation.append",
            serde_json::json!({"eventId":0,"runId":run_id,"timestamp":now_millis(),"nodeId":env::var("WORKBENCH_NODE_ID").unwrap_or_else(|_|"local".into()),"role":"agent","kind":if event.contains("ToolUse"){"agent-tool"}else{"agent-hook"},"name":event,"status":if event=="PreToolUse"{"running"}else{"completed"},"durationMs":hook_duration_ms(&input,&stored,event),"spanId":input.get("tool_use_id"),"parentSpanId":input.get("turn_id"),"attributes":{"action":input.get("tool_name").and_then(Value::as_str).unwrap_or("lifecycle"),"targetRepo":input.get("cwd").and_then(Value::as_str).and_then(|cwd|Path::new(cwd).file_name()).and_then(|name|name.to_str())}}),
        ),
    };
    let response = call_unix(socket, &request)?;
    if response.ok {
        if event == "UserPromptSubmit"
            && let (Some(path), Some(result)) = (state_path.as_ref(), response.result.as_ref())
            && let Some(run_id) = result.get("runId").and_then(Value::as_str)
        {
            write_codex_hook_state(
                path,
                &serde_json::json!({"runId":run_id,"startedAt":now_millis()}),
            )?;
        }
        if event == "PreToolUse"
            && let Some(path) = state_path.as_ref()
        {
            let mut value = stored
                .clone()
                .unwrap_or_else(|| serde_json::json!({"runId":run_id}));
            if let Some(id) = input.get("tool_use_id").and_then(Value::as_str) {
                value["tools"][id] = serde_json::json!(now_millis());
            }
            write_codex_hook_state(path, &value)?;
        }
        if matches!(event, "Stop" | "SessionEnd")
            && let Some(path) = state_path
        {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    } else {
        print_response(response)
    }
}

fn codex_hook_state_path(session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(128)
        .collect();
    state_home().join("codex-runs").join(format!("{safe}.json"))
}
fn write_codex_hook_state(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}
fn hook_duration_ms(input: &Value, stored: &Option<Value>, event: &str) -> Option<u64> {
    if event != "PostToolUse" {
        return None;
    }
    let id = input.get("tool_use_id").and_then(Value::as_str)?;
    let started = stored
        .as_ref()?
        .pointer(&format!("/tools/{id}"))
        .and_then(Value::as_u64)?;
    Some(now_millis().saturating_sub(started))
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

const DASHBOARD_HTML: &str = include_str!("../../../dashboard/dist/index.html");
const DASHBOARD_JS: &str = include_str!("../../../dashboard/dist/app.js");
const DASHBOARD_CSS: &str = include_str!("../../../dashboard/dist/app.css");

struct DashboardAuth {
    session: String,
    csrf: String,
    bootstrap_expires_at: u64,
    bootstrap_consumed: AtomicBool,
}
#[derive(Default)]
struct DashboardEvents {
    sequence: u64,
    cursors: Value,
    replay: VecDeque<(u64, String)>,
}

fn serve_dashboard(socket: PathBuf, listen: &str, open: bool) -> Result<()> {
    use std::net::TcpListener;
    let address: std::net::SocketAddr = listen.parse()?;
    if !address.ip().is_loopback() {
        bail!("dashboard listen address must be loopback");
    }
    let listener = TcpListener::bind(address)?;
    let url = format!("http://{}/", listener.local_addr()?);
    println!("{url}");
    if open {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(opener).arg(&url).spawn();
    }
    let auth = Arc::new(DashboardAuth {
        session: format!("op_{}", uuid::Uuid::new_v4().simple()),
        csrf: format!("csrf_{}", uuid::Uuid::new_v4().simple()),
        bootstrap_expires_at: now_millis().saturating_add(60_000),
        bootstrap_consumed: AtomicBool::new(false),
    });
    let dashboard_events = Arc::new(std::sync::Mutex::new(DashboardEvents::default()));
    {
        let socket = socket.clone();
        let dashboard_events = Arc::clone(&dashboard_events);
        thread::spawn(move || {
            loop {
                let cursors = dashboard_events
                    .lock()
                    .expect("dashboard events")
                    .cursors
                    .clone();
                if let Ok(response) = call_unix(
                    &socket,
                    &Request::new(
                        "observability.query",
                        serde_json::json!({"cursors":cursors,"limit":500}),
                    ),
                ) && response.ok
                    && let Some(result) = response.result
                {
                    let has_events = result
                        .get("events")
                        .and_then(Value::as_array)
                        .is_some_and(|events| !events.is_empty());
                    let mut state = dashboard_events.lock().expect("dashboard events");
                    state.cursors = result
                        .get("cursors")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    if has_events {
                        state.sequence = state.sequence.saturating_add(1);
                        let sequence = state.sequence;
                        let data = serde_json::to_string(&result).unwrap_or_else(|_| "{}".into());
                        state.replay.push_back((sequence, data));
                        while state.replay.len() > 1000 {
                            state.replay.pop_front();
                        }
                    }
                }
                thread::sleep(std::time::Duration::from_secs(1));
            }
        });
    }
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let socket = socket.clone();
        let auth = Arc::clone(&auth);
        let dashboard_events = Arc::clone(&dashboard_events);
        thread::spawn(move || {
            if let Err(error) =
                handle_dashboard_connection(stream, &socket, &auth, &dashboard_events)
            {
                eprintln!("dashboard connection failed: {error}");
            }
        });
    }
    Ok(())
}

fn handle_dashboard_connection(
    mut stream: std::net::TcpStream,
    socket: &Path,
    auth: &DashboardAuth,
    dashboard_events: &std::sync::Mutex<DashboardEvents>,
) -> Result<()> {
    use std::io::{Read, Write};
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..size]);
        if bytes.len() > 65_536 {
            return write_http(
                &mut stream,
                "413 Payload Too Large",
                "application/json",
                "{\"error\":\"request too large\"}",
                None,
            );
        }
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|line| line.split_once(':'))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
    }
    let request = String::from_utf8_lossy(&bytes);
    let first = request.lines().next().unwrap_or("");
    let method = first.split_whitespace().next().unwrap_or("");
    let target = first.split_whitespace().nth(1).unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");
    let query = parse_query(target);
    let header = |name: &str| {
        request
            .lines()
            .find(|line| {
                line.split_once(':')
                    .is_some_and(|(key, _)| key.eq_ignore_ascii_case(name))
            })
            .and_then(|line| line.split_once(':').map(|(_, value)| value.trim()))
    };
    let host_ok = header("Host").is_some_and(|host| {
        host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]")
    });
    let authenticated = header("Cookie").is_some_and(|cookie| {
        cookie
            .split(';')
            .any(|item| item.trim() == format!("workbench_operator={}", auth.session))
    });
    if !host_ok {
        return write_http(
            &mut stream,
            "403 Forbidden",
            "application/json",
            "{\"error\":\"loopback Host required\"}",
            None,
        );
    }
    if path == "/" && method == "GET" {
        let browser_navigation = header("Sec-Fetch-Dest") == Some("document");
        if !authenticated
            && browser_navigation
            && (now_millis() > auth.bootstrap_expires_at
                || auth.bootstrap_consumed.swap(true, Ordering::AcqRel))
        {
            return write_http(
                &mut stream,
                "401 Unauthorized",
                "application/json",
                "{\"error\":\"dashboard bootstrap expired or already consumed\"}",
                None,
            );
        }
        let cookies = (!authenticated && browser_navigation).then(|| format!(
                "Set-Cookie: workbench_operator={}; HttpOnly; SameSite=Strict; Path=/\r\nSet-Cookie: workbench_csrf={}; SameSite=Strict; Path=/\r\n",
                auth.session, auth.csrf
            ));
        return write_http(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            DASHBOARD_HTML,
            cookies.as_deref(),
        );
    }
    if path == "/assets/app.js" && method == "GET" {
        return write_http(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            DASHBOARD_JS,
            None,
        );
    }
    if path == "/assets/app.css" && method == "GET" {
        return write_http(
            &mut stream,
            "200 OK",
            "text/css; charset=utf-8",
            DASHBOARD_CSS,
            None,
        );
    }
    if !authenticated {
        return write_http(
            &mut stream,
            "401 Unauthorized",
            "application/json",
            "{\"error\":\"operator session required\"}",
            None,
        );
    }
    if method == "POST" {
        let origin_ok = header("Origin")
            .is_some_and(|origin| origin == format!("http://{}", header("Host").unwrap_or("")));
        let csrf_ok = header("X-CSRF-Token") == Some(auth.csrf.as_str());
        if !origin_ok || !csrf_ok {
            return write_http(
                &mut stream,
                "403 Forbidden",
                "application/json",
                "{\"error\":\"Origin or CSRF validation failed\"}",
                None,
            );
        }
        let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
        let mut params: Value =
            serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({}));
        let needs_grant = path == "/api/operator/nonce"
            || (path == "/api/operator/action" && params.get("actionNonce").is_none());
        if needs_grant {
            let grant = call_unix(
                socket,
                &Request::new(
                    "operator.grant",
                    serde_json::json!({"operatorId":"dashboard","ttlMs":60_000}),
                ),
            )?;
            let grant_token = grant
                .result
                .and_then(|value| {
                    value
                        .get("grantToken")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .ok_or_else(|| anyhow::anyhow!("controller rejected operator grant"))?;
            params["grantToken"] = serde_json::json!(grant_token);
        }
        let action = if path == "/api/operator/nonce" {
            "operator.nonce"
        } else if path == "/api/operator/action" {
            "operator.action"
        } else {
            return write_http(
                &mut stream,
                "404 Not Found",
                "application/json",
                "{\"error\":\"not found\"}",
                None,
            );
        };
        let (status, content_type, body) = rpc_response_json(socket, action, params);
        return write_http(&mut stream, status, content_type, &body, None);
    }
    if path == "/api/events" && method == "GET" {
        let mut cursor = header("Last-Event-ID")
            .and_then(|value| value.parse::<u64>().ok())
            .or_else(|| {
                target
                    .split('?')
                    .nth(1)
                    .and_then(|query| {
                        query
                            .split('&')
                            .find_map(|item| item.strip_prefix("cursor="))
                    })
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(0);
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n")?;
        loop {
            let frames = dashboard_events
                .lock()
                .expect("dashboard events")
                .replay
                .iter()
                .filter(|(sequence, _)| *sequence > cursor)
                .cloned()
                .collect::<Vec<_>>();
            for (sequence, data) in frames {
                let frame = format!("id: {sequence}\nevent: observations\ndata: {data}\n\n");
                if stream.write_all(frame.as_bytes()).is_err() {
                    return Ok(());
                }
                cursor = sequence;
            }
            if stream.write_all(b": keepalive\n\n").is_err() {
                break;
            }
            thread::sleep(std::time::Duration::from_secs(1));
        }
        return Ok(());
    }
    let (status, content_type, body) = if path == "/api/snapshot" && method == "GET" {
        rpc_result_json(socket, "dashboard.snapshot", Value::Null)
    } else if path == "/api/runs" && method == "GET" {
        rpc_response_json(
            socket,
            "run.list",
            serde_json::json!({
                "limit": query_u64(&query,"limit",50).clamp(1,200),
                "offset": query_u64(&query,"offset",0).min(10_000),
                "status": query.get("status")
            }),
        )
    } else if path == "/api/run" && method == "GET" {
        match safe_query_id(&query, "runId") {
            Ok(run_id) => rpc_response_json(socket, "run.get", serde_json::json!({"runId":run_id})),
            Err(body) => ("400 Bad Request", "application/json", body),
        }
    } else if path == "/api/observations" && method == "GET" {
        match safe_query_id(&query, "runId") {
            Ok(run_id) => rpc_response_json(
                socket,
                "observability.query",
                serde_json::json!({
                    "runId":run_id,
                    "limit":query_u64(&query,"limit",500).clamp(1,1000),
                    "cursors":query.get("cursors").and_then(|value|serde_json::from_str::<Value>(value).ok()).unwrap_or_else(||serde_json::json!({}))
                }),
            ),
            Err(body) => ("400 Bad Request", "application/json", body),
        }
    } else if path == "/api/logs" && method == "GET" {
        match (
            safe_query_id(&query, "executorId"),
            safe_query_id(&query, "processId"),
        ) {
            (Ok(executor_id), Ok(process_id)) => {
                let response = call_unix(
                    socket,
                    &Request::new(
                        "executor.call",
                        serde_json::json!({"executorId":executor_id,"action":"logs.read","params":{"processId":process_id,"tail":query_u64(&query,"tail",200).clamp(1,2000)}}),
                    ),
                );
                match response {
                    Ok(value) if value.ok => {
                        let result = value.result.unwrap_or(Value::Null);
                        let safe = serde_json::json!({"processId":process_id,"lines":result.get("lines").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|line|line.as_str().map(|text|text.chars().take(4000).collect::<String>())).collect::<Vec<_>>()});
                        ("200 OK", "application/json", safe.to_string())
                    }
                    Ok(value) => (
                        "503 Service Unavailable",
                        "application/json",
                        serde_json::to_string(&value).unwrap(),
                    ),
                    Err(error) => (
                        "503 Service Unavailable",
                        "application/json",
                        serde_json::json!({"error":error.to_string()}).to_string(),
                    ),
                }
            }
            _ => (
                "400 Bad Request",
                "application/json",
                "{\"error\":\"valid executorId and processId are required\"}".to_owned(),
            ),
        }
    } else {
        (
            "404 Not Found",
            "application/json",
            "{\"error\":\"not found\"}".to_owned(),
        )
    };
    write_http(&mut stream, status, content_type, &body, None)
}

fn parse_query(target: &str) -> std::collections::BTreeMap<String, String> {
    target
        .split_once('?')
        .map(|(_, query)| {
            query
                .split('&')
                .filter_map(|item| item.split_once('='))
                .filter_map(|(key, value)| Some((percent_decode(key)?, percent_decode(value)?)))
                .collect()
        })
        .unwrap_or_default()
}
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            output.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            output.push(if bytes[index] == b'+' {
                b' '
            } else {
                bytes[index]
            });
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}
fn query_u64(query: &std::collections::BTreeMap<String, String>, key: &str, default: u64) -> u64 {
    query
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
fn safe_query_id(
    query: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> std::result::Result<String, String> {
    query
        .get(key)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 200
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
                })
        })
        .cloned()
        .ok_or_else(|| format!("{{\"error\":\"valid {key} is required\"}}"))
}

fn write_http(
    stream: &mut std::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
    extra: Option<&str>,
) -> Result<()> {
    use std::io::Write;
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        extra.unwrap_or(""),
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn rpc_result_json(
    socket: &Path,
    action: &str,
    params: Value,
) -> (&'static str, &'static str, String) {
    match call_unix(socket, &Request::new(action, params)) {
        Ok(response) if response.ok => (
            "200 OK",
            "application/json",
            serde_json::to_string(&response.result.unwrap_or(Value::Null)).unwrap(),
        ),
        Ok(response) => (
            "503 Service Unavailable",
            "application/json",
            serde_json::to_string(&response).unwrap(),
        ),
        Err(error) => (
            "503 Service Unavailable",
            "application/json",
            serde_json::json!({"error":error.to_string()}).to_string(),
        ),
    }
}
fn rpc_response_json(
    socket: &Path,
    action: &str,
    params: Value,
) -> (&'static str, &'static str, String) {
    match call_unix(socket, &Request::new(action, params)) {
        Ok(response) => (
            "200 OK",
            "application/json",
            serde_json::to_string(&response).unwrap(),
        ),
        Err(error) => (
            "503 Service Unavailable",
            "application/json",
            serde_json::json!({"error":error.to_string()}).to_string(),
        ),
    }
}

fn validate_fabric_manifest(path: &Path) -> Result<()> {
    let manifest = load_fabric_manifest(path)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

fn load_fabric_manifest(path: &Path) -> Result<FabricManifest> {
    let manifest: FabricManifest = serde_yaml::from_slice(&std::fs::read(path)?)?;
    if manifest.api_version != "distributed-workbench.dev/v1" {
        bail!("apiVersion must be distributed-workbench.dev/v1");
    }
    if manifest.kind != "Fabric" {
        bail!("kind must be Fabric");
    }
    if manifest.nodes.is_empty() {
        bail!("nodes must not be empty");
    }
    let mut identities = HashSet::new();
    for node in &manifest.nodes {
        validate_identity(&node.id)?;
        if !identities.insert(node.id.as_str()) {
            bail!("duplicate node id: {}", node.id);
        }
        if node.id != manifest.initiator_node {
            let connection = node.connection.as_ref().ok_or_else(|| {
                anyhow::anyhow!("remote node {} needs connection.sshAlias", node.id)
            })?;
            validate_identity(&connection.ssh_alias)?;
        }
        if !matches!(
            (node.platform, node.architecture),
            (FabricPlatform::Macos, FabricArchitecture::Aarch64)
                | (FabricPlatform::Linux, FabricArchitecture::X86_64)
                | (FabricPlatform::Windows, FabricArchitecture::X86_64)
        ) {
            bail!(
                "node {} uses an unsupported release platform/architecture combination",
                node.id
            );
        }
        if node.allow_roots.is_empty() {
            bail!("node {} needs at least one allowRoot", node.id);
        }
        for root in &node.allow_roots {
            validate_allow_root(root)
                .map_err(|error| anyhow::anyhow!("node {}: {error}", node.id))?;
        }
    }
    if !identities.contains(manifest.initiator_node.as_str()) {
        bail!("initiatorNode does not reference a node");
    }
    Ok(manifest)
}

fn plan_fabric_manifest(path: &Path) -> Result<()> {
    let manifest = load_fabric_manifest(path)?;
    let remote_nodes: Vec<&FabricNode> = manifest
        .nodes
        .iter()
        .filter(|node| node.id != manifest.initiator_node)
        .collect();
    let mut links = Vec::new();
    for node in &remote_nodes {
        links.push(serde_json::json!({
            "dialer": manifest.initiator_node,
            "peer": node.id,
            "sshAlias": node.connection.as_ref().map(|connection| &connection.ssh_alias),
        }));
    }
    for (index, left) in remote_nodes.iter().enumerate() {
        for right in remote_nodes.iter().skip(index + 1) {
            let (dialer, peer) = if !matches!(left.platform, FabricPlatform::Windows)
                && matches!(right.platform, FabricPlatform::Windows)
            {
                (*left, *right)
            } else if matches!(left.platform, FabricPlatform::Windows)
                && !matches!(right.platform, FabricPlatform::Windows)
            {
                (*right, *left)
            } else if left.id <= right.id {
                (*left, *right)
            } else {
                (*right, *left)
            };
            links.push(serde_json::json!({
                "dialer": dialer.id,
                "peer": peer.id,
                "sshAlias": peer.connection.as_ref().map(|connection| &connection.ssh_alias),
            }));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "apiVersion": manifest.api_version,
            "kind": "FabricPlan",
            "initiatorNode": manifest.initiator_node,
            "nodes": manifest.nodes,
            "links": links,
        }))?
    );
    Ok(())
}

fn validate_identity(value: &str) -> Result<()> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("invalid node identity or SSH alias: {value}");
    }
    Ok(())
}

fn validate_allow_root(value: &str) -> Result<()> {
    let trimmed = value.trim_end_matches(['/', '\\']);
    if value.is_empty()
        || matches!(value, "/" | "~" | "$HOME" | "${user.home}")
        || (trimmed.len() == 2
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':')
    {
        bail!("allowRoot must be a narrow non-root path: {value}");
    }
    if !(value.starts_with('/')
        || value.starts_with("${user.home}/")
        || (value.len() > 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'\\' | b'/')))
    {
        bail!("allowRoot must be absolute or start with ${{user.home}}/: {value}");
    }
    Ok(())
}

fn print_response(response: workbench_protocol::Response) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&response)?);
    if !response.ok {
        bail!("request failed")
    }
    Ok(())
}

fn state_home() -> PathBuf {
    platform_state_home().join("distributed-workbench")
}

fn default_log_dir() -> PathBuf {
    state_home().join("logs")
}

fn print_logs(
    component: Option<&str>,
    correlation_id: Option<&str>,
    request_id: Option<&str>,
    task_id: Option<&str>,
    connection_id: Option<&str>,
    since_ms: Option<u64>,
    tail: usize,
) -> Result<()> {
    let task_correlation = task_id.and_then(|task_id| {
        let state = std::fs::read_to_string(state_home().join("controller.json")).ok()?;
        let state: Value = serde_json::from_str(&state).ok()?;
        state
            .get("tasks")?
            .as_array()?
            .iter()
            .find(|task| task.get("id").and_then(Value::as_str) == Some(task_id))?
            .get("correlationId")?
            .as_str()
            .map(str::to_owned)
    });
    let components: Vec<&str> = component
        .map(|value| vec![value])
        .unwrap_or_else(|| vec!["controller", "executor", "peer"]);
    let mut records = Vec::new();
    for name in components {
        let path = default_log_dir().join(format!("{name}.jsonl"));
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if since_ms.is_some_and(|since| {
                value.get("timestamp").and_then(Value::as_u64).unwrap_or(0) < since
            }) {
                continue;
            }
            let text = value.to_string();
            if [correlation_id, request_id, connection_id]
                .into_iter()
                .flatten()
                .any(|needle| !text.contains(needle))
            {
                continue;
            }
            if task_id.is_some_and(|task_id| {
                !text.contains(task_id)
                    && task_correlation
                        .as_deref()
                        .is_none_or(|correlation_id| !text.contains(correlation_id))
            }) {
                continue;
            }
            records.push(value);
        }
    }
    records.sort_by_key(|value| value.get("timestamp").and_then(Value::as_u64).unwrap_or(0));
    let start = records.len().saturating_sub(tail);
    for record in &records[start..] {
        println!("{}", serde_json::to_string(record)?);
    }
    Ok(())
}

#[cfg(unix)]
fn platform_state_home() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").expect("HOME is required")).join(".local/state")
        })
}

#[cfg(windows)]
fn platform_state_home() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .or_else(|| env::var_os("PROGRAMDATA"))
        .map(PathBuf::from)
        .expect("LOCALAPPDATA or PROGRAMDATA is required")
}

fn default_controller_socket() -> PathBuf {
    state_home().join("controller.sock")
}

fn default_executor_socket() -> PathBuf {
    state_home().join("executor.sock")
}

fn default_controller_state() -> PathBuf {
    state_home().join("controller.json")
}

fn default_executor_state() -> PathBuf {
    state_home().join("executor-fences.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabric_manifest_is_domain_neutral_and_strict() {
        let manifest: FabricManifest = serde_yaml::from_str(
            r#"
apiVersion: distributed-workbench.dev/v1
kind: Fabric
initiatorNode: laptop
nodes:
  - id: laptop
    platform: macos
    architecture: aarch64
    allowRoots: ["${user.home}/Code"]
  - id: devbox-a
    platform: linux
    architecture: x86_64
    connection: {sshAlias: devbox-a}
    allowRoots: ["/srv/workspace"]
topology: {mode: full-mesh}
"#,
        )
        .unwrap();
        assert_eq!(manifest.nodes.len(), 2);
        assert!(
            serde_yaml::from_str::<FabricManifest>(
                r#"
apiVersion: distributed-workbench.dev/v1
kind: Fabric
initiatorNode: laptop
nodes: []
topology: {mode: full-mesh}
domains: []
"#
            )
            .is_err()
        );
    }

    #[test]
    fn allow_roots_reject_broad_or_relative_paths() {
        for root in ["/", "~", "$HOME", "${user.home}", "C:\\", "relative/path"] {
            assert!(validate_allow_root(root).is_err(), "accepted {root}");
        }
        for root in ["/srv/workspace", "${user.home}/Code", "D:\\Workspace"] {
            validate_allow_root(root).unwrap();
        }
    }

    #[test]
    fn fabric_architecture_is_explicit() {
        assert!(
            serde_yaml::from_str::<FabricManifest>(
                r#"
apiVersion: distributed-workbench.dev/v1
kind: Fabric
initiatorNode: laptop
nodes:
  - id: laptop
    platform: macos
    allowRoots: ["${user.home}/Code"]
topology: {mode: full-mesh}
"#,
            )
            .is_err()
        );
    }

    #[test]
    fn dashboard_query_parser_decodes_and_rejects_unsafe_ids() {
        let query = parse_query("/api/logs?executorId=node%2Done&processId=process_1&tail=20");
        assert_eq!(safe_query_id(&query, "executorId").unwrap(), "node-one");
        assert_eq!(query_u64(&query, "tail", 1), 20);
        let unsafe_query = parse_query("/api/logs?executorId=..%2Fsecret");
        assert!(safe_query_id(&unsafe_query, "executorId").is_err());
    }
}

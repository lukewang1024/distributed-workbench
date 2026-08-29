use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, connect};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TokenPrimary},
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    System::{
        Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
        RemoteDesktop::{
            WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSDisconnected,
            WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken,
        },
        Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
        },
    },
};
use workbench_core::now_ms;
use workbench_protocol::RpcError;

pub fn inspect(application: &Path) -> Result<Value, RpcError> {
    let executable = find_executable(application).ok_or_else(|| {
        RpcError::new(
            "APPLICATION_INSPECT_FAILED",
            format!(
                "no Windows application executable found under {}",
                application.display()
            ),
        )
    })?;
    let package = find_package(application).and_then(|path| {
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    });
    let native = native_identity(&executable)?;
    let bundle_identifier = package.as_ref().and_then(package_identifier);
    let package_version = package
        .as_ref()
        .and_then(|value| value.get("version"))
        .cloned();
    let document_types = package
        .as_ref()
        .map(package_document_types)
        .unwrap_or_default();
    Ok(json!({
        "path": application,
        "executablePath": executable,
        "bundleIdentifier": bundle_identifier,
        "version": package_version.or_else(|| native.get("productVersion").cloned()),
        "build": native.get("fileVersion"),
        "documentTypes": document_types,
        "signature": {"valid": native.get("signatureValid").and_then(Value::as_bool).unwrap_or(false)},
        "localWebcontents": find_directory(application, "local_webcontents"),
    }))
}

pub struct LaunchOptions<'a> {
    pub user_data_dir: Option<&'a Path>,
    pub chromium_local_state_path: Option<&'a Path>,
    pub chromium_local_state_patch: Option<&'a Value>,
    pub chromium_local_state_settle_ms: u64,
    pub file: Option<&'a Path>,
    pub remote_debugging_port: Option<u16>,
    pub terminate_conflicting_instances: bool,
}

pub fn launch(
    application: &Path,
    args: &[String],
    options: LaunchOptions<'_>,
) -> Result<Value, RpcError> {
    let executable = find_executable(application).ok_or_else(|| {
        RpcError::new(
            "APPLICATION_LAUNCH_FAILED",
            "application executable is missing",
        )
    })?;
    if let Some(profile) = options.user_data_dir {
        fs::create_dir_all(profile)
            .map_err(|error| RpcError::new("PROFILE_WRITE_FAILED", error.to_string()))?;
    }
    let terminated = if options.terminate_conflicting_instances {
        terminate_process_trees(
            &executable,
            options.user_data_dir,
            options.remote_debugging_port,
        )?
    } else {
        0
    };
    // Chromium writes Local State while shutting down. Patch only after every
    // conflicting process has exited, otherwise its final flush can silently
    // replace the development resource flags before the new process starts.
    let local_state = options
        .chromium_local_state_path
        .map(Path::to_path_buf)
        .or_else(|| {
            options
                .user_data_dir
                .map(|profile| profile.join("Local State"))
        });
    let prelaunch_local_state_patch =
        match (local_state.as_deref(), options.chromium_local_state_patch) {
            (Some(local_state), Some(patch)) => {
                Some(patch_chromium_local_state(local_state, patch)?)
            }
            (None, Some(_)) => {
                return Err(RpcError::new(
                    "INVALID_PARAMS",
                    "chromiumLocalStatePatch requires chromiumLocalStatePath or userDataDir",
                ));
            }
            _ => None,
        };
    let mut launch_args = args.to_vec();
    if let Some(profile) = options.user_data_dir {
        launch_args.push(format!("--user-data-dir={}", profile.display()));
    }
    if let Some(port) = options.remote_debugging_port {
        launch_args.push(format!("--remote-debugging-port={port}"));
    }
    if let Some(file) = options.file {
        launch_args.push(file.to_string_lossy().into_owned());
    }
    let pid = spawn_in_active_session(&executable, &launch_args, application)?;
    if options.chromium_local_state_patch.is_some() && options.chromium_local_state_settle_ms > 0 {
        thread::sleep(Duration::from_millis(
            options.chromium_local_state_settle_ms,
        ));
    }
    // The native CCM bootstrap can flush its startup snapshot after process
    // creation, replacing values written before launch. Re-apply the exact
    // patch after that startup settle window and fail launch if the atomic
    // write/read path cannot be completed.
    let postlaunch_local_state_patch =
        match (local_state.as_deref(), options.chromium_local_state_patch) {
            (Some(local_state), Some(patch)) => {
                Some(patch_chromium_local_state(local_state, patch)?)
            }
            _ => None,
        };
    Ok(json!({
        "applicationPath": application,
        "executable": executable,
        "pid": pid,
        "launcherPid": pid,
        "terminatedConflictingInstances": terminated,
        "chromiumLocalState": {
            "prelaunch": prelaunch_local_state_patch,
            "postlaunch": postlaunch_local_state_patch,
            "settleMs": options.chromium_local_state_settle_ms,
        },
        "file": options.file,
        "args": args,
        "cdp": Value::Null,
        "readyAt": now_ms(),
    }))
}

fn merge_json(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                merge_json(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

fn patch_chromium_local_state(local_state: &Path, patch: &Value) -> Result<Value, RpcError> {
    if !patch.is_object() {
        return Err(RpcError::new(
            "INVALID_PARAMS",
            "chromiumLocalStatePatch must be an object",
        ));
    }
    let parent = local_state.parent().ok_or_else(|| {
        RpcError::new(
            "INVALID_PARAMS",
            "chromiumLocalStatePath must have a parent directory",
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| RpcError::new("PROFILE_WRITE_FAILED", error.to_string()))?;
    let mut state = if local_state.exists() {
        let bytes = fs::read(local_state)
            .map_err(|error| RpcError::new("PROFILE_WRITE_FAILED", error.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|error| {
            RpcError::new(
                "PROFILE_INVALID",
                format!("cannot parse {}: {error}", local_state.display()),
            )
        })?
    } else {
        json!({})
    };
    if !state.is_object() {
        return Err(RpcError::new(
            "PROFILE_INVALID",
            format!("{} must contain a JSON object", local_state.display()),
        ));
    }
    merge_json(&mut state, patch);
    let temporary = parent.join(format!(".Local State.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(&state)
        .map_err(|error| RpcError::new("PROFILE_WRITE_FAILED", error.to_string()))?;
    fs::write(&temporary, bytes)
        .map_err(|error| RpcError::new("PROFILE_WRITE_FAILED", error.to_string()))?;
    let temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let local_state_wide: Vec<u16> = local_state
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let replaced = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            local_state_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(&temporary);
        return Err(RpcError::new("PROFILE_WRITE_FAILED", error.to_string()));
    }
    Ok(json!({
        "path": local_state,
        "applied": true,
    }))
}

fn terminate_process_trees(
    executable: &Path,
    user_data_dir: Option<&Path>,
    remote_debugging_port: Option<u16>,
) -> Result<usize, RpcError> {
    let name = executable
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            RpcError::new(
                "APPLICATION_TERMINATE_FAILED",
                "executable has no UTF-8 file name",
            )
        })?;
    let escaped = name.replace('\'', "''");
    let (selector_prelude, selector) = if user_data_dir.is_some() || remote_debugging_port.is_some()
    {
        let profile = user_data_dir
            .map(|value| {
                value
                    .to_string_lossy()
                    .replace('\\', "/")
                    .replace('\'', "''")
            })
            .unwrap_or_default();
        let port = remote_debugging_port
            .map(|value| format!("--remote-debugging-port={value}"))
            .unwrap_or_default();
        (
            format!("$profile='{profile}'; $portNeedle='{port}'; "),
            "@($all | Where-Object { if($_.ProcessId -eq $PID) { $false } else { $command=[string]$_.CommandLine; if($command) { $command=$command.Replace('\\','/'); (($profile.Length -gt 0) -and ($command.IndexOf($profile,[System.StringComparison]::OrdinalIgnoreCase) -ge 0)) -or (($portNeedle.Length -gt 0) -and ($command.IndexOf($portNeedle,[System.StringComparison]::OrdinalIgnoreCase) -ge 0)) } else { $false } } })".to_owned(),
        )
    } else {
        (
            String::new(),
            format!("@($all | Where-Object {{ $_.Name -ieq '{escaped}' }})"),
        )
    };
    let script = format!(
        "{selector_prelude}$all=@(Get-CimInstance Win32_Process); $items={selector}; foreach($item in $items) {{ Start-Process -FilePath taskkill.exe -ArgumentList @('/PID',[string]$item.ProcessId,'/T','/F') -Wait -WindowStyle Hidden | Out-Null }}; $deadline=(Get-Date).AddSeconds(15); do {{ $all=@(Get-CimInstance Win32_Process); $remaining={selector}; if($remaining.Count -eq 0) {{ break }}; Start-Sleep -Milliseconds 200 }} while((Get-Date) -lt $deadline); if($remaining.Count -ne 0) {{ throw \"conflicting processes for {escaped} did not exit\" }}; Write-Output $items.Count; exit 0"
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|error| RpcError::new("APPLICATION_TERMINATE_FAILED", error.to_string()))?;
    if !output.status.success() {
        return Err(RpcError::new(
            "APPLICATION_TERMINATE_FAILED",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .map_err(|error| RpcError::new("APPLICATION_TERMINATE_FAILED", error.to_string()))
}

pub fn open_file(
    application: &Path,
    file: &Path,
    handler: Option<&Path>,
) -> Result<Value, RpcError> {
    if !file.is_file() {
        return Err(RpcError::new(
            "FILE_NOT_FOUND",
            format!("file not found: {}", file.display()),
        ));
    }
    let executable = handler
        .map(Path::to_path_buf)
        .or_else(|| find_executable(application))
        .ok_or_else(|| RpcError::new("OPEN_FILE_FAILED", "application executable is missing"))?;
    spawn_in_active_session(
        &executable,
        &[file.to_string_lossy().into_owned()],
        application,
    )?;
    Ok(
        json!({"applicationPath": application, "handlerPath": executable, "file": file, "openedAt": now_ms()}),
    )
}

fn spawn_in_active_session(
    executable: &Path,
    args: &[String],
    cwd: &Path,
) -> Result<u32, RpcError> {
    let mut sessions = std::ptr::null_mut::<WTS_SESSION_INFOW>();
    let mut count = 0_u32;
    let enumerated = unsafe {
        WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut sessions, &mut count)
    };
    if enumerated == 0 {
        return Err(RpcError::new(
            "INTERACTIVE_SESSION_UNAVAILABLE",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let mut user_token: HANDLE = std::ptr::null_mut();
    let enumerated_sessions = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
    let mut candidate_found = false;
    let mut last_token_error = None;
    for session in enumerated_sessions
        .iter()
        .filter(|session| session.State == WTSActive)
        .chain(
            enumerated_sessions
                .iter()
                .filter(|session| session.State == WTSDisconnected),
        )
    {
        candidate_found = true;
        if unsafe { WTSQueryUserToken(session.SessionId, &mut user_token) } != 0 {
            break;
        }
        last_token_error = Some(std::io::Error::last_os_error().to_string());
    }
    unsafe { WTSFreeMemory(sessions.cast()) };
    if user_token.is_null() {
        let (code, message) = if candidate_found {
            (
                "INTERACTIVE_SESSION_TOKEN_FAILED",
                last_token_error.unwrap_or_else(|| "no session exposes a user token".into()),
            )
        } else {
            (
                "INTERACTIVE_SESSION_UNAVAILABLE",
                "no active or disconnected Windows desktop session is available".into(),
            )
        };
        return Err(RpcError::new(code, message));
    }
    let mut primary_token: HANDLE = std::ptr::null_mut();
    let duplicated = unsafe {
        DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary_token,
        )
    };
    unsafe { CloseHandle(user_token) };
    if duplicated == 0 {
        return Err(RpcError::new(
            "INTERACTIVE_SESSION_TOKEN_FAILED",
            std::io::Error::last_os_error().to_string(),
        ));
    }

    let mut command_line = std::iter::once(executable.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut desktop = "winsta0\\default\0".encode_utf16().collect::<Vec<_>>();
    let cwd = cwd
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.lpDesktop = desktop.as_mut_ptr();
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut environment = std::ptr::null_mut();
    if unsafe { CreateEnvironmentBlock(&mut environment, primary_token, 0) } == 0 {
        unsafe { CloseHandle(primary_token) };
        return Err(RpcError::new(
            "INTERACTIVE_ENVIRONMENT_FAILED",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let created = unsafe {
        CreateProcessAsUserW(
            primary_token,
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT,
            environment,
            cwd.as_ptr(),
            &startup,
            &mut process,
        )
    };
    unsafe { DestroyEnvironmentBlock(environment) };
    unsafe { CloseHandle(primary_token) };
    if created == 0 {
        return Err(RpcError::new(
            "INTERACTIVE_PROCESS_CREATE_FAILED",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(process.dwProcessId)
}

pub fn cdp_evaluate(
    port: u16,
    target_url_prefix: Option<&str>,
    expression: &str,
) -> Result<Value, RpcError> {
    let pages = cdp_json(port, "/json")?;
    let page = pages
        .as_array()
        .and_then(|items| {
            items.iter().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("page")
                    && target_url_prefix.is_none_or(|prefix| {
                        item.get("url")
                            .and_then(Value::as_str)
                            .is_some_and(|url| url.starts_with(prefix))
                    })
            })
        })
        .ok_or_else(|| RpcError::new("CDP_TARGET_NOT_FOUND", "matching page target not found"))?;
    let websocket_url = page
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("CDP_TARGET_INVALID", "target has no debugger URL"))?;
    // The discovery endpoint is deliberately reached over IPv4 loopback. Some
    // Chromium builds advertise `localhost` even when the debugger only listens
    // on 127.0.0.1, and Windows may resolve localhost to ::1 first.
    let websocket_url = websocket_url
        .replacen("ws://localhost:", "ws://127.0.0.1:", 1)
        .replacen("ws://[::1]:", "ws://127.0.0.1:", 1);
    let (mut socket, _) = connect(websocket_url.as_str())
        .map_err(|error| RpcError::new("CDP_WEBSOCKET_FAILED", error.to_string()))?;
    socket
        .send(Message::Text(
            json!({"id": 1, "method": "Runtime.evaluate", "params": {
                "expression": expression, "returnByValue": true, "awaitPromise": true
            }})
            .to_string()
            .into(),
        ))
        .map_err(|error| RpcError::new("CDP_WEBSOCKET_FAILED", error.to_string()))?;
    loop {
        let Message::Text(text) = socket
            .read()
            .map_err(|error| RpcError::new("CDP_WEBSOCKET_FAILED", error.to_string()))?
        else {
            continue;
        };
        let response: Value = serde_json::from_str(&text)
            .map_err(|error| RpcError::new("CDP_INVALID_RESPONSE", error.to_string()))?;
        if response.get("id").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(RpcError::new("CDP_EVALUATION_FAILED", error.to_string()));
        }
        if let Some(exception) = response.pointer("/result/exceptionDetails") {
            return Err(RpcError::new(
                "CDP_EVALUATION_FAILED",
                exception.to_string(),
            ));
        }
        return Ok(
            json!({"target": page, "value": response.pointer("/result/result/value").cloned().unwrap_or(Value::Null), "evaluatedAt": now_ms()}),
        );
    }
}

pub fn native_inspect(
    application: &Path,
    expected_window_title: Option<&str>,
) -> Result<Value, RpcError> {
    let executable = find_executable(application).ok_or_else(|| {
        RpcError::new(
            "NATIVE_INSPECTION_FAILED",
            "application executable is missing",
        )
    })?;
    let output_path = application.join(format!(
        ".workbench-native-inspect-{}-{}.json",
        std::process::id(),
        now_ms()
    ));
    let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
    // MainWindowHandle is unreliable for Chromium multi-process applications:
    // the document HWND can belong to another same-executable process and its
    // Win32 title may remain empty while UI Automation exposes the document
    // name on a descendant. Enumerate every desktop top-level UIA element for
    // the exact executable's PIDs and retain bounded descendant names.
    let script = r#"
Add-Type -AssemblyName UIAutomationClient
$target=[IO.Path]::GetFullPath('@TARGET@')
$expected='@EXPECTED@'
$processes=@(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and ([IO.Path]::GetFullPath($_.ExecutablePath) -eq $target) })
$roots=[Windows.Automation.AutomationElement]::RootElement.FindAll([Windows.Automation.TreeScope]::Children,[Windows.Automation.Condition]::TrueCondition)
$items=@()
foreach($process in $processes) {
  $windows=@()
  foreach($root in $roots) {
    if($root.Current.ProcessId -ne $process.ProcessId) { continue }
    $names=@()
    try {
      $descendants=$root.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)
      $limit=[Math]::Min($descendants.Count,500)
      for($index=0;$index -lt $limit;$index++) {
        $name=$descendants.Item($index).Current.Name
        if(-not [string]::IsNullOrWhiteSpace($name) -and $names.Count -lt 80 -and $names -notcontains $name) { $names += $name }
      }
    } catch {}
    $windows += @{
      title=$root.Current.Name
      role=$root.Current.ControlType.ProgrammaticName
      className=$root.Current.ClassName
      handle=$root.Current.NativeWindowHandle
      enabled=$root.Current.IsEnabled
      offscreen=$root.Current.IsOffscreen
      accessibleNames=$names
      expectedTitleObserved=((-not [string]::IsNullOrWhiteSpace($expected)) -and (@($root.Current.Name)+$names | Where-Object { $_ -and $_.IndexOf($expected,[StringComparison]::OrdinalIgnoreCase) -ge 0 }).Count -gt 0)
    }
  }
  $items += @{pid=$process.ProcessId;windows=$windows}
}
$json=@{accessibilityTrusted=$true;processes=$items} | ConvertTo-Json -Depth 8 -Compress
[IO.File]::WriteAllText('@OUTPUT@',$json,(New-Object Text.UTF8Encoding($false)))
"#
    .replace("@TARGET@", &quote(&executable))
    .replace(
        "@EXPECTED@",
        &expected_window_title.unwrap_or_default().replace('\'', "''"),
    )
    .replace("@OUTPUT@", &quote(&output_path));
    let powershell = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    spawn_in_active_session(
        powershell,
        &[
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script,
        ],
        application,
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !output_path.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    let output = fs::read(&output_path)
        .map_err(|error| RpcError::new("NATIVE_INSPECTION_FAILED", error.to_string()))?;
    let _ = fs::remove_file(&output_path);
    let inspection: Value = serde_json::from_slice(&output)
        .map_err(|error| RpcError::new("NATIVE_INSPECTION_FAILED", error.to_string()))?;
    let pids = inspection
        .get("processes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("pid").cloned())
        .collect::<Vec<_>>();
    Ok(
        json!({"applicationPath": application, "executable": executable, "pids": pids, "inspection": inspection, "inspectedAt": now_ms()}),
    )
}

pub fn capture_window(
    application: &Path,
    expected_window_title: Option<&str>,
    output: &Path,
) -> Result<Value, RpcError> {
    let executable = find_executable(application).ok_or_else(|| {
        RpcError::new("WINDOW_CAPTURE_FAILED", "application executable is missing")
    })?;
    let parent = output.parent().ok_or_else(|| {
        RpcError::new(
            "INVALID_PARAMS",
            "capture output must have a parent directory",
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| RpcError::new("WINDOW_CAPTURE_FAILED", error.to_string()))?;
    let metadata_path = parent.join(format!(
        ".workbench-window-capture-{}-{}.json",
        std::process::id(),
        now_ms()
    ));
    let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
    let script = r#"
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class WorkbenchCapture {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern int GetWindowTextLengthW(IntPtr hWnd);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, System.Text.StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
}
'@
function Normalize-WorkbenchPath([string]$value) {
  $full=[IO.Path]::GetFullPath($value)
  if($full.StartsWith('\\?\')) { $full=$full.Substring(4) }
  $full.TrimEnd('\')
}
$target=Normalize-WorkbenchPath '@TARGET@'
$expected='@EXPECTED@'
$output=Normalize-WorkbenchPath '@OUTPUT@'
$metadata=Normalize-WorkbenchPath '@METADATA@'
$ErrorActionPreference='Stop'
try {
$pids=@(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and ((Normalize-WorkbenchPath $_.ExecutablePath) -eq $target) } | ForEach-Object { [uint32]$_.ProcessId })
$candidates=New-Object System.Collections.Generic.List[object]
$callback=[WorkbenchCapture+EnumWindowsProc]{ param($hwnd,$unused)
  if(-not [WorkbenchCapture]::IsWindowVisible($hwnd)) { return $true }
  $windowPid=[uint32]0
  [WorkbenchCapture]::GetWindowThreadProcessId($hwnd,[ref]$windowPid) | Out-Null
  if($pids -notcontains $windowPid) { return $true }
  $rect=New-Object WorkbenchCapture+RECT
  if(-not [WorkbenchCapture]::GetWindowRect($hwnd,[ref]$rect)) { return $true }
  $width=$rect.Right-$rect.Left; $height=$rect.Bottom-$rect.Top
  if($width -lt 2 -or $height -lt 2) { return $true }
  $length=[WorkbenchCapture]::GetWindowTextLengthW($hwnd)
  $builder=New-Object Text.StringBuilder ($length+1)
  [WorkbenchCapture]::GetWindowTextW($hwnd,$builder,$builder.Capacity) | Out-Null
  $title=$builder.ToString()
  $matches=[string]::IsNullOrWhiteSpace($expected) -or $title.IndexOf($expected,[StringComparison]::OrdinalIgnoreCase) -ge 0
  $candidates.Add([pscustomobject]@{Hwnd=$hwnd;Pid=$windowPid;Rect=$rect;Title=$title;Matches=$matches;Area=([int64]$width*[int64]$height)})
  return $true
}
[WorkbenchCapture]::EnumWindows($callback,[IntPtr]::Zero) | Out-Null
$selected=$candidates | Sort-Object @{Expression='Matches';Descending=$true},@{Expression='Area';Descending=$true} | Select-Object -First 1
if(-not $selected) { throw 'no visible application window found' }
$rect=$selected.Rect; $width=$rect.Right-$rect.Left; $height=$rect.Bottom-$rect.Top
function Measure-Bitmap([Drawing.Bitmap]$bitmap) {
  $samples=0; $nonBlack=0; $minimum=765; $maximum=0; $sum=0L
  $stepX=[Math]::Max(1,[int]($bitmap.Width/100)); $stepY=[Math]::Max(1,[int]($bitmap.Height/100))
  for($y=0;$y -lt $bitmap.Height;$y+=$stepY) { for($x=0;$x -lt $bitmap.Width;$x+=$stepX) {
    $pixel=$bitmap.GetPixel($x,$y); $value=$pixel.R+$pixel.G+$pixel.B
    $samples++; $sum+=$value; if($value -gt 12){$nonBlack++}; if($value -lt $minimum){$minimum=$value}; if($value -gt $maximum){$maximum=$value}
  }}
  [pscustomobject]@{samples=$samples;nonBlackRatio=($nonBlack/[double]$samples);meanRgbSum=($sum/[double]$samples);range=($maximum-$minimum)}
}
function Capture-PrintWindow {
  $bitmap=New-Object Drawing.Bitmap $width,$height,([Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $graphics=[Drawing.Graphics]::FromImage($bitmap); $hdc=$graphics.GetHdc(); $ok=$false
  try { $ok=[WorkbenchCapture]::PrintWindow($selected.Hwnd,$hdc,2) } finally { $graphics.ReleaseHdc($hdc); $graphics.Dispose() }
  [pscustomobject]@{Bitmap=$bitmap;Ok=$ok;Metrics=(Measure-Bitmap $bitmap)}
}
function Capture-ScreenDc {
  [WorkbenchCapture]::SetForegroundWindow($selected.Hwnd) | Out-Null; Start-Sleep -Milliseconds 400
  $bitmap=New-Object Drawing.Bitmap $width,$height,([Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $graphics=[Drawing.Graphics]::FromImage($bitmap)
  try { $graphics.CopyFromScreen($rect.Left,$rect.Top,0,0,$bitmap.Size,[Drawing.CopyPixelOperation]::SourceCopy) } finally { $graphics.Dispose() }
  [pscustomobject]@{Bitmap=$bitmap;Ok=$true;Metrics=(Measure-Bitmap $bitmap)}
}
$capture=Capture-PrintWindow; $backend='printwindow'
if((-not $capture.Ok) -or $capture.Metrics.nonBlackRatio -lt 0.01 -or $capture.Metrics.range -lt 8) {
  $capture.Bitmap.Dispose(); $capture=Capture-ScreenDc; $backend='screen-dc'
}
$capture.Bitmap.Save($output,[Drawing.Imaging.ImageFormat]::Png); $capture.Bitmap.Dispose()
$result=[pscustomobject]@{
  path=$output;backend=$backend;pid=$selected.Pid;handle=[int64]$selected.Hwnd;title=$selected.Title
  width=$width;height=$height;originX=$rect.Left;originY=$rect.Top;metrics=$capture.Metrics
}
[IO.File]::WriteAllText($metadata,($result|ConvertTo-Json -Depth 5 -Compress),(New-Object Text.UTF8Encoding($false)))
} catch {
  $failure=[pscustomobject]@{error=$_.Exception.ToString();scriptStack=$_.ScriptStackTrace}
  [IO.File]::WriteAllText($metadata,($failure|ConvertTo-Json -Depth 5 -Compress),(New-Object Text.UTF8Encoding($false)))
}
"#
    .replace("@TARGET@", &quote(&executable))
    .replace(
        "@EXPECTED@",
        &expected_window_title.unwrap_or_default().replace('\'', "''"),
    )
    .replace("@OUTPUT@", &quote(output))
    .replace("@METADATA@", &quote(&metadata_path));
    spawn_in_active_session(
        Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        &[
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script,
        ],
        application,
    )?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !metadata_path.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    let metadata = fs::read(&metadata_path)
        .map_err(|error| RpcError::new("WINDOW_CAPTURE_FAILED", error.to_string()))?;
    let _ = fs::remove_file(&metadata_path);
    let capture: Value = serde_json::from_slice(&metadata)
        .map_err(|error| RpcError::new("WINDOW_CAPTURE_FAILED", error.to_string()))?;
    if let Some(error) = capture.get("error").and_then(Value::as_str) {
        let mut failure = RpcError::new("WINDOW_CAPTURE_FAILED", error);
        failure.details = capture;
        return Err(failure);
    }
    let bytes = fs::metadata(output)
        .map_err(|error| RpcError::new("WINDOW_CAPTURE_FAILED", error.to_string()))?
        .len();
    Ok(json!({
        "applicationPath": application,
        "executable": executable,
        "path": output,
        "bytes": bytes,
        "capture": capture,
        "capturedAt": now_ms(),
    }))
}

pub fn input_window(
    application: &Path,
    expected_window_title: Option<&str>,
    actions: &[Value],
) -> Result<Value, RpcError> {
    let executable = find_executable(application)
        .ok_or_else(|| RpcError::new("WINDOW_INPUT_FAILED", "application executable is missing"))?;
    if actions.is_empty() || actions.len() > 100 {
        return Err(RpcError::new(
            "INVALID_PARAMS",
            "actions must contain between 1 and 100 items",
        ));
    }
    let normalized = actions
        .iter()
        .enumerate()
        .map(|(index, action)| normalize_input_action(index, action))
        .collect::<Result<Vec<_>, _>>()?;
    let parent = application.parent().unwrap_or(application);
    let metadata_path = parent.join(format!(
        ".workbench-window-input-{}-{}.json",
        std::process::id(),
        now_ms()
    ));
    let payload = BASE64.encode(
        serde_json::to_vec(&normalized)
            .map_err(|error| RpcError::new("WINDOW_INPUT_FAILED", error.to_string()))?,
    );
    let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
    let script = r###"
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
public static class WorkbenchInput {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public InputUnion U; }
  [StructLayout(LayoutKind.Explicit)] public struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx,dy; public uint mouseData,dwFlags,time; public UIntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk,wScan; public uint dwFlags,time; public UIntPtr dwExtraInfo; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr value);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out RECT rect);
  [DllImport("user32.dll")] public static extern int GetWindowTextLengthW(IntPtr window);
  [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr window,StringBuilder text,int count);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr window);
  [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr window,int command);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] static extern bool AttachThreadInput(uint source,uint target,bool attach);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x,int y);
  [DllImport("user32.dll",SetLastError=true)] static extern uint SendInput(uint count,INPUT[] inputs,int size);
  const uint Keyboard=1,Mouse=0,KeyUp=2,Unicode=4;
  static INPUT K(ushort vk,ushort scan,uint flags) { return new INPUT { type=Keyboard,U=new InputUnion { ki=new KEYBDINPUT { wVk=vk,wScan=scan,dwFlags=flags } } }; }
  static INPUT M(uint flags) { return new INPUT { type=Mouse,U=new InputUnion { mi=new MOUSEINPUT { dwFlags=flags } } }; }
  static void Send(INPUT[] values) { if(SendInput((uint)values.Length,values,Marshal.SizeOf(typeof(INPUT)))!=values.Length) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(),"SendInput failed"); }
  public static void Focus(IntPtr window) { uint ignored;uint foreground=GetWindowThreadProcessId(GetForegroundWindow(),out ignored);uint current=GetCurrentThreadId();bool attached=foreground!=0&&foreground!=current&&AttachThreadInput(current,foreground,true);try{ShowWindowAsync(window,9);BringWindowToTop(window);if(!SetForegroundWindow(window))throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(),"SetForegroundWindow failed");}finally{if(attached)AttachThreadInput(current,foreground,false);} }
  public static void Text(string value) { foreach(char ch in value) Send(new[]{K(0,ch,Unicode),K(0,ch,Unicode|KeyUp)}); }
  public static void Click(string button,int count) { uint down=2,up=4; if(button=="right"){down=8;up=16;} if(button=="middle"){down=32;up=64;} for(int i=0;i<count;i++){Send(new[]{M(down),M(up)});Thread.Sleep(80);} }
  static ushort Modifier(string value) { switch(value){case "ALT":return 18;case "CTRL":return 17;case "SHIFT":return 16;case "WIN":return 91;default:throw new ArgumentException("unsupported modifier: "+value);} }
  static ushort KeyCode(string value) {
    var keys=new Dictionary<string,ushort>{{"BACKSPACE",8},{"TAB",9},{"ENTER",13},{"ESC",27},{"SPACE",32},{"PAGEUP",33},{"PAGEDOWN",34},{"END",35},{"HOME",36},{"LEFT",37},{"UP",38},{"RIGHT",39},{"DOWN",40},{"INSERT",45},{"DELETE",46},{"F1",112},{"F2",113},{"F3",114},{"F4",115},{"F5",116},{"F6",117},{"F7",118},{"F8",119},{"F9",120},{"F10",121},{"F11",122},{"F12",123},{"SEMICOLON",186},{"EQUALS",187},{"COMMA",188},{"MINUS",189},{"PERIOD",190},{"SLASH",191},{"BACKTICK",192},{"LEFTBRACKET",219},{"BACKSLASH",220},{"RIGHTBRACKET",221},{"QUOTE",222}};
    ushort code;if(keys.TryGetValue(value,out code))return code;if(value.Length==1){char ch=Char.ToUpperInvariant(value[0]);if((ch>='A'&&ch<='Z')||(ch>='0'&&ch<='9'))return ch;}throw new ArgumentException("unsupported key: "+value);
  }
  public static void Key(string key,string[] modifiers) { var values=new List<INPUT>();foreach(string modifier in modifiers)values.Add(K(Modifier(modifier),0,0));ushort code=KeyCode(key);values.Add(K(code,0,0));values.Add(K(code,0,KeyUp));for(int i=modifiers.Length-1;i>=0;i--)values.Add(K(Modifier(modifiers[i]),0,KeyUp));Send(values.ToArray()); }
}
'@
function Normalize-WorkbenchPath([string]$value) { $full=[IO.Path]::GetFullPath($value);if($full.StartsWith('\\?\')){$full=$full.Substring(4)};$full.TrimEnd('\') }
$target=Normalize-WorkbenchPath '@TARGET@';$expected='@EXPECTED@';$metadata=Normalize-WorkbenchPath '@METADATA@'
$decodedActions=([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('@ACTIONS@'))) | ConvertFrom-Json
if($decodedActions -is [Array]){$actions=$decodedActions}else{$actions=@($decodedActions)}
$ErrorActionPreference='Stop'
try {
  $pids=@(Get-CimInstance Win32_Process|Where-Object{$_.ExecutablePath -and ((Normalize-WorkbenchPath $_.ExecutablePath)-eq $target)}|ForEach-Object{[uint32]$_.ProcessId})
  $windows=New-Object Collections.Generic.List[object]
  $callback=[WorkbenchInput+EnumWindowsProc]{param($hwnd,$unused)
    if(-not [WorkbenchInput]::IsWindowVisible($hwnd)){return $true};$pidValue=[uint32]0;[WorkbenchInput]::GetWindowThreadProcessId($hwnd,[ref]$pidValue)|Out-Null;if($pids -notcontains $pidValue){return $true}
    $rect=New-Object WorkbenchInput+RECT;if(-not [WorkbenchInput]::GetWindowRect($hwnd,[ref]$rect)){return $true};$length=[WorkbenchInput]::GetWindowTextLengthW($hwnd);$builder=New-Object Text.StringBuilder ($length+1);[WorkbenchInput]::GetWindowTextW($hwnd,$builder,$builder.Capacity)|Out-Null;$title=$builder.ToString()
    $matches=[string]::IsNullOrWhiteSpace($expected)-or $title.IndexOf($expected,[StringComparison]::OrdinalIgnoreCase)-ge 0;$area=[int64]($rect.Right-$rect.Left)*[int64]($rect.Bottom-$rect.Top);$windows.Add([pscustomobject]@{Hwnd=$hwnd;Pid=$pidValue;Rect=$rect;Title=$title;Matches=$matches;Area=$area});return $true
  }
  [WorkbenchInput]::EnumWindows($callback,[IntPtr]::Zero)|Out-Null;$selected=$windows|Sort-Object @{Expression='Matches';Descending=$true},@{Expression='Area';Descending=$true}|Select-Object -First 1
  if(-not $selected){throw 'no visible application window found'};if(-not $selected.Matches){throw "no application window matched expected title '$expected'"}
  [WorkbenchInput]::Focus($selected.Hwnd);Start-Sleep -Milliseconds 250
  $performed=@();foreach($action in $actions){switch($action.type){
    'focus' {[WorkbenchInput]::Focus($selected.Hwnd)}
    'click' {$x=$selected.Rect.Left+[int]$action.x;$y=$selected.Rect.Top+[int]$action.y;if($x -lt $selected.Rect.Left -or $x -ge $selected.Rect.Right -or $y -lt $selected.Rect.Top -or $y -ge $selected.Rect.Bottom){throw 'click coordinates are outside the target window'};if(-not [WorkbenchInput]::SetCursorPos($x,$y)){throw 'SetCursorPos failed'};[WorkbenchInput]::Click([string]$action.button,[int]$action.count)}
    'key' {[string[]]$modifiers=@();if($null -ne $action.modifiers){$modifiers=[string[]]@($action.modifiers|Where-Object{$_})};[WorkbenchInput]::Key([string]$action.key,$modifiers)}
    'text' {[WorkbenchInput]::Text([string]$action.text)}
    'paste' {$clipboardSet=$false;for($attempt=0;$attempt -lt 10 -and -not $clipboardSet;$attempt++){try{[Windows.Forms.Clipboard]::SetText([string]$action.text);$clipboardSet=$true}catch{if($attempt -eq 9){throw};Start-Sleep -Milliseconds 50}};[WorkbenchInput]::Key('V',[string[]]@('CTRL'))}
    'wait' {Start-Sleep -Milliseconds ([int]$action.durationMs)}
    default {throw "unsupported action type: $($action.type)"}
  };$performed+=$action.type;Start-Sleep -Milliseconds 60}
  $result=[pscustomobject]@{applicationPath='@APPLICATION@';executable=$target;target=[pscustomobject]@{pid=$selected.Pid;handle=[int64]$selected.Hwnd;title=$selected.Title;left=$selected.Rect.Left;top=$selected.Rect.Top;width=($selected.Rect.Right-$selected.Rect.Left);height=($selected.Rect.Bottom-$selected.Rect.Top)};actions=$performed;inputAt=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()}
  [IO.File]::WriteAllText($metadata,($result|ConvertTo-Json -Depth 6 -Compress),(New-Object Text.UTF8Encoding($false)))
} catch {$failure=[pscustomobject]@{error=$_.Exception.ToString();scriptStack=$_.ScriptStackTrace};[IO.File]::WriteAllText($metadata,($failure|ConvertTo-Json -Depth 5 -Compress),(New-Object Text.UTF8Encoding($false)))}
"###
    .replace("@TARGET@", &quote(&executable))
    .replace(
        "@EXPECTED@",
        &expected_window_title.unwrap_or_default().replace('\'', "''"),
    )
    .replace("@METADATA@", &quote(&metadata_path))
    .replace("@APPLICATION@", &quote(application))
    .replace("@ACTIONS@", &payload);
    spawn_in_active_session(
        Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        &[
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-STA".to_owned(),
            "-Command".to_owned(),
            script,
        ],
        application,
    )?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !metadata_path.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    let metadata = fs::read(&metadata_path)
        .map_err(|error| RpcError::new("WINDOW_INPUT_FAILED", error.to_string()))?;
    let _ = fs::remove_file(&metadata_path);
    let result: Value = serde_json::from_slice(&metadata)
        .map_err(|error| RpcError::new("WINDOW_INPUT_FAILED", error.to_string()))?;
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        let mut failure = RpcError::new("WINDOW_INPUT_FAILED", error);
        failure.details = result;
        return Err(failure);
    }
    Ok(result)
}

fn normalize_input_action(index: usize, action: &Value) -> Result<Value, RpcError> {
    let object = action
        .as_object()
        .ok_or_else(|| invalid_action(index, "must be an object"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_action(index, "type is required"))?;
    let mut result = Map::new();
    result.insert("type".to_owned(), json!(kind));
    match kind {
        "focus" => ensure_action_fields(index, object, &["type"]),
        "click" => {
            ensure_action_fields(index, object, &["type", "x", "y", "button", "count"])?;
            let x = action_i64(index, object, "x")?;
            let y = action_i64(index, object, "y")?;
            let button = object
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left");
            if !matches!(button, "left" | "right" | "middle") {
                return Err(invalid_action(index, "invalid button"));
            }
            let count = object.get("count").and_then(Value::as_u64).unwrap_or(1);
            if !(1..=3).contains(&count) {
                return Err(invalid_action(index, "count must be 1..3"));
            }
            result.extend([
                ("x".into(), json!(x)),
                ("y".into(), json!(y)),
                ("button".into(), json!(button)),
                ("count".into(), json!(count)),
            ]);
            Ok(())
        }
        "key" => {
            ensure_action_fields(index, object, &["type", "key", "modifiers"])?;
            let key = object
                .get("key")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid_action(index, "key is required"))?;
            let modifiers = object
                .get("modifiers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if modifiers.len() > 4
                || modifiers
                    .iter()
                    .any(|value| !matches!(value.as_str(), Some("ALT" | "CTRL" | "SHIFT" | "WIN")))
            {
                return Err(invalid_action(index, "invalid modifier"));
            }
            result.insert("key".into(), json!(key));
            result.insert("modifiers".into(), json!(modifiers));
            Ok(())
        }
        "text" | "paste" => {
            ensure_action_fields(index, object, &["type", "text"])?;
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_action(index, "text is required"))?;
            if text.encode_utf16().count() > 16_384 {
                return Err(invalid_action(index, "text is too long"));
            }
            result.insert("text".into(), json!(text));
            Ok(())
        }
        "wait" => {
            ensure_action_fields(index, object, &["type", "durationMs"])?;
            let duration = object
                .get("durationMs")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid_action(index, "durationMs is required"))?;
            if duration > 5_000 {
                return Err(invalid_action(index, "durationMs exceeds 5000"));
            }
            result.insert("durationMs".into(), json!(duration));
            Ok(())
        }
        _ => Err(invalid_action(index, "unsupported action type")),
    }?;
    Ok(Value::Object(result))
}

fn ensure_action_fields(
    index: usize,
    object: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), RpcError> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(invalid_action(
            index,
            &format!("unsupported field: {field}"),
        ));
    }
    Ok(())
}
fn action_i64(index: usize, object: &Map<String, Value>, field: &str) -> Result<i64, RpcError> {
    object
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid_action(index, &format!("{field} must be an integer")))
}
fn invalid_action(index: usize, message: &str) -> RpcError {
    RpcError::new("INVALID_PARAMS", format!("actions[{index}]: {message}"))
}

fn cdp_json(port: u16, path: &str) -> Result<Value, RpcError> {
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().expect("loopback"),
        Duration::from_millis(500),
    )
    .map_err(|error| RpcError::new("CDP_UNAVAILABLE", error.to_string()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| RpcError::new("CDP_UNAVAILABLE", error.to_string()))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| RpcError::new("CDP_UNAVAILABLE", error.to_string()))?;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && !response.is_empty() =>
            {
                break;
            }
            Err(error) => return Err(RpcError::new("CDP_UNAVAILABLE", error.to_string())),
        }
    }
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| &response[position + 4..])
        .ok_or_else(|| RpcError::new("CDP_INVALID_RESPONSE", "missing HTTP response body"))?;
    serde_json::from_slice(body)
        .map_err(|error| RpcError::new("CDP_INVALID_RESPONSE", error.to_string()))
}

fn find_executable(root: &Path) -> Option<PathBuf> {
    if root.is_file()
        && root
            .extension()
            .is_some_and(|value| value.eq_ignore_ascii_case("exe"))
    {
        return Some(root.to_path_buf());
    }
    let mut pending = vec![(root.to_path_buf(), 0_u8)];
    let mut candidates = Vec::new();
    while let Some((directory, depth)) = pending.pop() {
        if depth > 3 {
            continue;
        }
        let entries = fs::read_dir(directory).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push((path, depth + 1));
            } else if path
                .extension()
                .is_some_and(|value| value.eq_ignore_ascii_case("exe"))
            {
                candidates.push(path);
            }
        }
    }
    candidates.sort_by_key(|path| (path.components().count(), path.as_os_str().len()));
    candidates.into_iter().next()
}

fn find_package(root: &Path) -> Option<PathBuf> {
    let candidates = [
        root.join("resources/app/package.json"),
        root.join("resources/package.json"),
        root.join("package.json"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

fn package_identifier(value: &Value) -> Option<Value> {
    value
        .pointer("/build/appId")
        .or_else(|| value.get("appId"))
        .cloned()
}

fn package_document_types(value: &Value) -> Vec<Value> {
    let Some(associations) = value
        .pointer("/build/fileAssociations")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    associations
        .iter()
        .filter_map(|association| {
            let extensions = match association.get("ext")? {
                Value::String(value) => {
                    vec![Value::String(value.trim_start_matches('.').to_owned())]
                }
                Value::Array(values) => values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|value| Value::String(value.trim_start_matches('.').to_owned()))
                    .collect(),
                _ => Vec::new(),
            };
            Some(json!({"CFBundleTypeExtensions": extensions, "LSItemContentTypes": []}))
        })
        .collect()
}

fn native_identity(executable: &Path) -> Result<Value, RpcError> {
    let script = "$item=Get-Item -LiteralPath $env:WORKBENCH_APPLICATION_EXE; ".to_owned()
        + "$signature=Get-AuthenticodeSignature -LiteralPath $item.FullName; "
        + "@{fileVersion=$item.VersionInfo.FileVersion;productVersion=$item.VersionInfo.ProductVersion;signatureValid=($signature.Status -eq 'Valid')} | ConvertTo-Json -Compress";
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("WORKBENCH_APPLICATION_EXE", executable)
        .output()
        .map_err(|error| RpcError::new("APPLICATION_INSPECT_FAILED", error.to_string()))?;
    if !output.status.success() {
        return Err(RpcError::new(
            "APPLICATION_INSPECT_FAILED",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| RpcError::new("APPLICATION_INSPECT_FAILED", error.to_string()))
}

fn find_directory(root: &Path, name: &str) -> Option<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(directory).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().and_then(|value| value.to_str()) == Some(name) {
                    return Some(path);
                }
                pending.push(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn electron_package_identity_and_associations_are_normalized() {
        let package = json!({
            "version": "1.2.3",
            "build": {
                "appId": "com.example.desktop",
                "fileAssociations": [{"ext": [".docx", "txt"]}]
            }
        });
        assert_eq!(
            package_identifier(&package),
            Some(json!("com.example.desktop"))
        );
        assert_eq!(
            package_document_types(&package),
            vec![json!({
                "CFBundleTypeExtensions": ["docx", "txt"],
                "LSItemContentTypes": []
            })]
        );
    }

    #[test]
    fn chromium_local_state_patch_preserves_existing_preferences() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("Local State"),
            serde_json::to_vec(&json!({"existing": true, "saman": {"other": 1}})).unwrap(),
        )
        .unwrap();

        let result = patch_chromium_local_state(
            &directory.path().join("Local State"),
            &json!({"saman": {"use_file_resource": true, "hotfix": {"is_enabled": false}}}),
        )
        .unwrap();

        assert_eq!(result["applied"], true);
        let state: Value =
            serde_json::from_slice(&fs::read(directory.path().join("Local State")).unwrap())
                .unwrap();
        assert_eq!(state["existing"], true);
        assert_eq!(state["saman"]["other"], 1);
        assert_eq!(state["saman"]["use_file_resource"], true);
        assert_eq!(state["saman"]["hotfix"]["is_enabled"], false);
    }

    #[test]
    fn paste_input_action_is_normalized_as_bounded_text() {
        assert_eq!(
            normalize_input_action(0, &json!({"type": "paste", "text": "x^2"})).unwrap(),
            json!({"type": "paste", "text": "x^2"})
        );
        assert!(
            normalize_input_action(0, &json!({"type": "paste", "text": "x", "key": "V"})).is_err()
        );
    }
}

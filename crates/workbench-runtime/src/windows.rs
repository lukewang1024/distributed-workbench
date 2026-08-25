use serde_json::{Value, json};
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
            WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW,
            WTSFreeMemory, WTSQueryUserToken,
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

pub fn launch(
    application: &Path,
    args: &[String],
    user_data_dir: Option<&Path>,
    chromium_local_state_patch: Option<&Value>,
    file: Option<&Path>,
    remote_debugging_port: Option<u16>,
    terminate_conflicting_instances: bool,
) -> Result<Value, RpcError> {
    let executable = find_executable(application).ok_or_else(|| {
        RpcError::new(
            "APPLICATION_LAUNCH_FAILED",
            "application executable is missing",
        )
    })?;
    if let Some(profile) = user_data_dir {
        fs::create_dir_all(profile)
            .map_err(|error| RpcError::new("PROFILE_WRITE_FAILED", error.to_string()))?;
    }
    let terminated = if terminate_conflicting_instances {
        terminate_process_trees(&executable)?
    } else {
        0
    };
    // Chromium writes Local State while shutting down. Patch only after every
    // conflicting process has exited, otherwise its final flush can silently
    // replace the development resource flags before the new process starts.
    let local_state_patch = match (user_data_dir, chromium_local_state_patch) {
        (Some(profile), Some(patch)) => Some(patch_chromium_local_state(profile, patch)?),
        (None, Some(_)) => {
            return Err(RpcError::new(
                "INVALID_PARAMS",
                "chromiumLocalStatePatch requires userDataDir",
            ));
        }
        _ => None,
    };
    let mut launch_args = args.to_vec();
    if let Some(profile) = user_data_dir {
        launch_args.push(format!("--user-data-dir={}", profile.display()));
    }
    if let Some(port) = remote_debugging_port {
        launch_args.push(format!("--remote-debugging-port={port}"));
    }
    if let Some(file) = file {
        launch_args.push(file.to_string_lossy().into_owned());
    }
    let pid = spawn_in_active_session(&executable, &launch_args, application)?;
    Ok(json!({
        "applicationPath": application,
        "executable": executable,
        "pid": pid,
        "launcherPid": pid,
        "terminatedConflictingInstances": terminated,
        "chromiumLocalState": local_state_patch,
        "file": file,
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

fn patch_chromium_local_state(profile: &Path, patch: &Value) -> Result<Value, RpcError> {
    if !patch.is_object() {
        return Err(RpcError::new(
            "INVALID_PARAMS",
            "chromiumLocalStatePatch must be an object",
        ));
    }
    fs::create_dir_all(profile)
        .map_err(|error| RpcError::new("PROFILE_WRITE_FAILED", error.to_string()))?;
    let local_state = profile.join("Local State");
    let mut state = if local_state.exists() {
        let bytes = fs::read(&local_state)
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
    let temporary = profile.join(format!(".Local State.{}.tmp", std::process::id()));
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

fn terminate_process_trees(executable: &Path) -> Result<usize, RpcError> {
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
    let script = format!(
        "$name='{escaped}'; $items=@(Get-CimInstance Win32_Process | Where-Object {{ $_.Name -ieq $name }}); foreach($item in $items) {{ & taskkill.exe /PID $item.ProcessId /T /F | Out-Null }}; $deadline=(Get-Date).AddSeconds(15); do {{ $remaining=@(Get-CimInstance Win32_Process | Where-Object {{ $_.Name -ieq $name }}); if($remaining.Count -eq 0) {{ break }}; Start-Sleep -Milliseconds 200 }} while((Get-Date) -lt $deadline); if($remaining.Count -ne 0) {{ throw \"conflicting $name process tree did not exit\" }}; $items.Count"
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
    let active_session = unsafe { std::slice::from_raw_parts(sessions, count as usize) }
        .iter()
        .find(|session| session.State == WTSActive)
        .map(|session| session.SessionId);
    unsafe { WTSFreeMemory(sessions.cast()) };
    let session_id = active_session.ok_or_else(|| {
        RpcError::new(
            "INTERACTIVE_SESSION_UNAVAILABLE",
            "no active Windows desktop session is available",
        )
    })?;

    let mut user_token: HANDLE = std::ptr::null_mut();
    if unsafe { WTSQueryUserToken(session_id, &mut user_token) } == 0 {
        return Err(RpcError::new(
            "INTERACTIVE_SESSION_TOKEN_FAILED",
            std::io::Error::last_os_error().to_string(),
        ));
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
            directory.path(),
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
}

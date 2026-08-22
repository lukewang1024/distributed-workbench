use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use tungstenite::{Message, connect};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TokenPrimary},
    System::{
        RemoteDesktop::{
            WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW,
            WTSFreeMemory, WTSQueryUserToken,
        },
        Threading::{CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW},
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
    file: Option<&Path>,
    remote_debugging_port: Option<u16>,
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
        "file": file,
        "args": args,
        "cdp": Value::Null,
        "readyAt": now_ms(),
    }))
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
    let created = unsafe {
        CreateProcessAsUserW(
            primary_token,
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            std::ptr::null(),
            cwd.as_ptr(),
            &startup,
            &mut process,
        )
    };
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
    let (mut socket, _) = connect(websocket_url)
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

pub fn native_inspect(application: &Path) -> Result<Value, RpcError> {
    let executable = find_executable(application).ok_or_else(|| {
        RpcError::new(
            "NATIVE_INSPECTION_FAILED",
            "application executable is missing",
        )
    })?;
    let script = "$target=[IO.Path]::GetFullPath($env:WORKBENCH_APPLICATION_EXE); $items=@(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and ([IO.Path]::GetFullPath($_.ExecutablePath) -eq $target) } | ForEach-Object { $p=Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue; @{pid=$_.ProcessId;windows=@($(if($p -and $p.MainWindowHandle -ne 0){@{title=$p.MainWindowTitle;role='Window'}}))} }); @{accessibilityTrusted=$true;processes=$items} | ConvertTo-Json -Depth 6 -Compress";
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("WORKBENCH_APPLICATION_EXE", &executable)
        .output()
        .map_err(|error| RpcError::new("NATIVE_INSPECTION_FAILED", error.to_string()))?;
    if !output.status.success() {
        return Err(RpcError::new(
            "NATIVE_INSPECTION_FAILED",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let inspection: Value = serde_json::from_slice(&output.stdout)
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
    stream
        .read_to_end(&mut response)
        .map_err(|error| RpcError::new("CDP_UNAVAILABLE", error.to_string()))?;
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
}

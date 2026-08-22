use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
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

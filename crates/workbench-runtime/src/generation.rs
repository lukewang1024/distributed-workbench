use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use workbench_core::{atomic_replace, now_ms};
use workbench_protocol::RpcError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterializedGeneration {
    pub generation_id: String,
    pub root: PathBuf,
    pub application_path: PathBuf,
    pub marker_path: PathBuf,
    pub created_at: u64,
    pub derived_from: Option<String>,
    pub clone_hits: u64,
    pub link_hits: u64,
    pub copied_files: u64,
    pub copied_bytes: u64,
}

#[derive(Debug, Default)]
struct MaterializeStats {
    clone_hits: u64,
    link_hits: u64,
    copied_files: u64,
    copied_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Overlay {
    pub source: PathBuf,
    pub target_relative_path: PathBuf,
    #[serde(default)]
    pub replace: bool,
}

pub fn materialize_derived(
    generation_root: &Path,
    generation_id: &str,
    baseline: &Path,
    derived_from: Option<&str>,
) -> Result<MaterializedGeneration, RpcError> {
    validate_generation_id(generation_id)?;
    if generation_root
        .file_name()
        .is_some_and(|name| name == "generations")
    {
        return Err(RpcError::new(
            "INVALID_GENERATION_ROOT",
            "generationRoot is the deployment root; do not include the trailing generations directory",
        ));
    }
    let generations = generation_root.join("generations");
    fs::create_dir_all(&generations)
        .map_err(|error| io_error("GENERATION_CREATE_FAILED", &generations, error))?;
    let root = generations.join(generation_id);
    if root.exists() {
        let marker_path = root.join("generation.json");
        let marker: Value = serde_json::from_slice(
            &fs::read(&marker_path)
                .map_err(|error| io_error("GENERATION_INVALID", &marker_path, error))?,
        )
        .map_err(|error| RpcError::new("GENERATION_INVALID", error.to_string()))?;
        if marker.get("state").and_then(Value::as_str) == Some("materialized") {
            return Ok(MaterializedGeneration {
                generation_id: generation_id.to_owned(),
                root: root.clone(),
                application_path: root.join(
                    marker
                        .get("applicationName")
                        .and_then(Value::as_str)
                        .unwrap_or("application"),
                ),
                marker_path,
                created_at: marker.get("createdAt").and_then(Value::as_u64).unwrap_or(0),
                derived_from: marker
                    .get("derivedFrom")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                clone_hits: marker.get("cloneHits").and_then(Value::as_u64).unwrap_or(0),
                link_hits: marker.get("linkHits").and_then(Value::as_u64).unwrap_or(0),
                copied_files: marker
                    .get("copiedFiles")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                copied_bytes: marker
                    .get("copiedBytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }
        return Err(RpcError::new(
            "GENERATION_EXISTS",
            format!("generation already exists but is not reusable: {generation_id}"),
        ));
    }
    let temporary = generations.join(format!(".{generation_id}.{}.tmp", std::process::id()));
    fs::create_dir(&temporary)
        .map_err(|error| io_error("GENERATION_CREATE_FAILED", &temporary, error))?;
    let baseline_name = baseline
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| RpcError::new("BASELINE_INVALID", "baseline has no UTF-8 name"))?;
    let application_name = baseline_name
        .find(".app")
        .map(|position| &baseline_name[..position + 4])
        .unwrap_or(baseline_name);
    let application_path = temporary.join(application_name);
    let mut stats = MaterializeStats::default();
    if let Err(error) = copy_tree(
        baseline,
        &application_path,
        derived_from.is_some(),
        &mut stats,
    ) {
        let marker = serde_json::json!({
            "generationId": generation_id,
            "state": "failed",
            "error": error.message,
            "createdAt": now_ms(),
        });
        let _ = fs::write(
            temporary.join("generation.json"),
            serde_json::to_vec_pretty(&marker).expect("marker serializes"),
        );
        let _ = fs::rename(&temporary, &root);
        return Err(error);
    }
    let created_at = now_ms();
    let marker = serde_json::json!({
        "generationId": generation_id,
        "state": "materialized",
        "applicationName": application_name,
        "baseline": baseline,
        "derivedFrom": derived_from,
        "cloneHits": stats.clone_hits,
        "linkHits": stats.link_hits,
        "copiedFiles": stats.copied_files,
        "copiedBytes": stats.copied_bytes,
        "createdAt": created_at,
    });
    let marker_path = temporary.join("generation.json");
    fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&marker).expect("marker serializes"),
    )
    .map_err(|error| io_error("GENERATION_CREATE_FAILED", &marker_path, error))?;
    fs::rename(&temporary, &root)
        .map_err(|error| io_error("GENERATION_CREATE_FAILED", &root, error))?;
    Ok(MaterializedGeneration {
        generation_id: generation_id.to_owned(),
        application_path: root.join(application_name),
        marker_path: root.join("generation.json"),
        root,
        created_at,
        derived_from: derived_from.map(str::to_owned),
        clone_hits: stats.clone_hits,
        link_hits: stats.link_hits,
        copied_files: stats.copied_files,
        copied_bytes: stats.copied_bytes,
    })
}

pub fn apply_overlays(application_path: &Path, overlays: &[Overlay]) -> Result<Value, RpcError> {
    let mut applied = Vec::new();
    for overlay in overlays {
        validate_relative(&overlay.target_relative_path)?;
        let target = application_path.join(&overlay.target_relative_path);
        apply_overlay_tree(&overlay.source, &target, overlay.replace)?;
        applied.push(serde_json::json!({
            "source": overlay.source,
            "target": target,
            "replace": overlay.replace,
        }));
    }
    Ok(serde_json::json!({"applied": applied}))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataPackResourceTree {
    pub root_relative: PathBuf,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub exclude_source_maps: bool,
    #[serde(default)]
    pub exclude_root_files: Vec<String>,
}

/// Rebuild the Chromium DataPack consumed by Windows Doubao Office from the
/// final expanded resource tree. This intentionally runs after every overlay,
/// so the pack and the loose runtime cannot represent different generations.
#[allow(clippy::too_many_arguments)]
pub fn pack_chromium_datapack(
    root_path: &Path,
    resource_trees: &[DataPackResourceTree],
    output_relative: &Path,
    platform: &str,
    arch: &str,
    bundle_name: &str,
    base_pack_path: Option<&Path>,
    base_pack_digest: Option<&str>,
    changed_prefixes: &[String],
) -> Result<Value, RpcError> {
    validate_relative(output_relative)?;
    let mut resources = Vec::new();
    for tree in resource_trees {
        validate_relative(&tree.root_relative)?;
        if !tree.prefix.is_empty()
            && (tree.prefix.starts_with('/')
                || tree.prefix.contains('\\')
                || tree
                    .prefix
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == ".."))
        {
            return Err(RpcError::new(
                "DATAPACK_PREFIX_INVALID",
                format!(
                    "DataPack prefix is not a safe resource path: {}",
                    tree.prefix
                ),
            ));
        }
        let tree_root = root_path.join(&tree.root_relative);
        if !tree_root.is_dir() {
            return Err(RpcError::new(
                "DATAPACK_INPUT_MISSING",
                format!("DataPack resource root is missing: {}", tree_root.display()),
            ));
        }
        collect_pack_tree(
            &tree_root,
            &tree_root,
            &tree.prefix,
            tree.exclude_source_maps,
            &tree.exclude_root_files,
            &mut resources,
        )?;
    }
    resources.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    for pair in resources.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(RpcError::new(
                "OFFICE_PACK_DUPLICATE_PATH",
                format!("duplicate packed resource path: {}", pair[0].0),
            ));
        }
    }
    if resources.is_empty() {
        return Err(RpcError::new(
            "OFFICE_PACK_EMPTY",
            "no Office resources to pack",
        ));
    }
    let base_entries = if let Some(base_path) = base_pack_path {
        let expected = base_pack_digest.ok_or_else(|| {
            RpcError::new(
                "BASE_DATAPACK_DIGEST_REQUIRED",
                "incremental DataPack requires the base pak digest",
            )
        })?;
        let mut digest = Sha256::new();
        hash_file_into(base_path, &mut digest)?;
        let actual = format!("sha256:{}", hex::encode(digest.finalize()));
        if actual != expected {
            return Err(RpcError::new(
                "BASE_DATAPACK_DRIFT",
                format!("base DataPack digest changed: expected {expected}, got {actual}"),
            ));
        }
        Some((base_path.to_path_buf(), parse_datapack_entries(base_path)?))
    } else {
        None
    };
    if base_entries.is_some() && changed_prefixes.is_empty() {
        return Err(RpcError::new(
            "DATAPACK_CHANGED_PREFIX_REQUIRED",
            "incremental DataPack requires at least one changed prefix",
        ));
    }
    let resource_count = resources.len() + 1;
    if resource_count > u16::MAX as usize {
        return Err(RpcError::new(
            "OFFICE_PACK_RESOURCE_LIMIT",
            format!("DataPack resource limit exceeded: {resource_count}"),
        ));
    }

    let mut content_digest = Sha256::new();
    for (relative, path) in &resources {
        content_digest.update(relative.as_bytes());
        content_digest.update([0]);
        if let Some((base_path, entries)) = &base_entries
            && !path_matches_prefix(relative, changed_prefixes)
        {
            let (offset, length) = entries.get(relative).ok_or_else(|| {
                RpcError::new(
                    "BASE_DATAPACK_ENTRY_MISSING",
                    format!("unchanged entry is absent from base pak: {relative}"),
                )
            })?;
            hash_file_range_into(base_path, *offset, *length, &mut content_digest)?;
        } else {
            hash_file_into(path, &mut content_digest)?;
        }
    }
    let content_hash = hex::encode(content_digest.finalize());
    let manifest = serde_json::to_vec(&serde_json::json!({
        "arch": arch,
        "bundle_name": bundle_name,
        "code_cache": Value::Null,
        "content_hash": content_hash.clone(),
        "entries": resources.iter().enumerate().map(|(index, (relative, _))| {
            serde_json::json!({"id": index + 2, "path": relative})
        }).collect::<Vec<_>>(),
        "platform": platform,
        "tool_version": "1",
        "v8_version": "",
    }))
    .expect("Office pack manifest serializes");

    let header_size = 12_u64;
    let index_size = ((resource_count + 1) * 6) as u64;
    let mut offsets = Vec::with_capacity(resource_count + 1);
    offsets.push(header_size + index_size);
    offsets.push(offsets[0] + manifest.len() as u64);
    for (relative, path) in &resources {
        let size = if let Some((_, entries)) = &base_entries
            && !path_matches_prefix(relative, changed_prefixes)
        {
            entries
                .get(relative)
                .ok_or_else(|| {
                    RpcError::new(
                        "BASE_DATAPACK_ENTRY_MISSING",
                        format!("unchanged entry is absent from base pak: {relative}"),
                    )
                })?
                .1
        } else {
            fs::metadata(path)
                .map_err(|error| io_error("OFFICE_PACK_READ_FAILED", path, error))?
                .len()
        };
        offsets.push(offsets.last().copied().unwrap_or(0) + size);
    }
    if offsets.last().copied().unwrap_or(0) > u32::MAX as u64 {
        return Err(RpcError::new(
            "OFFICE_PACK_SIZE_LIMIT",
            format!(
                "DataPack exceeds 4 GiB offset limit: {}",
                offsets.last().unwrap()
            ),
        ));
    }

    let output = root_path.join(output_relative);
    let parent = output.parent().ok_or_else(|| {
        RpcError::new(
            "OFFICE_PACK_OUTPUT_INVALID",
            "Office pack output has no parent",
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", parent, error))?;
    let temporary = parent.join(format!(".doubao_office.pak.{}.tmp", std::process::id()));
    let result = (|| -> Result<(), RpcError> {
        let mut target = fs::File::create(&temporary)
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        target
            .write_all(&5_u32.to_le_bytes())
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        target
            .write_all(&[0, 0, 0, 0])
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        target
            .write_all(&(resource_count as u16).to_le_bytes())
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        target
            .write_all(&0_u16.to_le_bytes())
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        for (index, offset) in offsets.iter().take(resource_count).enumerate() {
            target
                .write_all(&((index + 1) as u16).to_le_bytes())
                .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
            target
                .write_all(&(*offset as u32).to_le_bytes())
                .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        }
        target
            .write_all(&0_u16.to_le_bytes())
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        target
            .write_all(&(offsets[resource_count] as u32).to_le_bytes())
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        target
            .write_all(&manifest)
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        for (relative, path) in &resources {
            if let Some((base_path, entries)) = &base_entries
                && !path_matches_prefix(relative, changed_prefixes)
            {
                let (offset, length) = entries.get(relative).expect("base entry validated");
                copy_file_range(base_path, *offset, *length, &mut target)?;
            } else {
                let mut source = fs::File::open(path)
                    .map_err(|error| io_error("OFFICE_PACK_READ_FAILED", path, error))?;
                std::io::copy(&mut source, &mut target)
                    .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
            }
        }
        target
            .sync_all()
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &temporary, error))?;
        atomic_replace(&temporary, &output)
            .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", &output, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;

    let mut digest = Sha256::new();
    hash_file_into(&output, &mut digest)?;
    Ok(serde_json::json!({
        "output": output,
        "resources": resource_count,
        "entries": resources.len(),
        "size": fs::metadata(&output).map_err(|error| io_error("OFFICE_PACK_READ_FAILED", &output, error))?.len(),
        "sha256": format!("sha256:{}", hex::encode(digest.finalize())),
        "contentHash": format!("sha256:{content_hash}"),
        "incremental": base_entries.is_some(),
        "basePack": base_pack_path,
        "changedPrefixes": changed_prefixes,
    }))
}

fn path_matches_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        path == prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn parse_datapack_entries(path: &Path) -> Result<HashMap<String, (u64, u64)>, RpcError> {
    let mut source =
        fs::File::open(path).map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    let mut header = [0_u8; 12];
    source
        .read_exact(&mut header)
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    if u32::from_le_bytes(header[0..4].try_into().unwrap()) != 5 {
        return Err(RpcError::new(
            "BASE_DATAPACK_INVALID",
            "base pak is not DataPack version 5",
        ));
    }
    let count = u16::from_le_bytes(header[8..10].try_into().unwrap()) as usize;
    if count < 1 {
        return Err(RpcError::new(
            "BASE_DATAPACK_INVALID",
            "base pak has no manifest entry",
        ));
    }
    let mut index = vec![0_u8; (count + 1) * 6];
    source
        .read_exact(&mut index)
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    let offsets = (0..=count)
        .map(|position| {
            let start = position * 6 + 2;
            u32::from_le_bytes(index[start..start + 4].try_into().unwrap()) as u64
        })
        .collect::<Vec<_>>();
    if offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(RpcError::new(
            "BASE_DATAPACK_INVALID",
            "base pak offsets are not monotonic",
        ));
    }
    let file_size = source
        .metadata()
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?
        .len();
    if offsets.last().copied().unwrap_or(0) > file_size {
        return Err(RpcError::new(
            "BASE_DATAPACK_INVALID",
            "base pak offsets exceed file size",
        ));
    }
    source
        .seek(SeekFrom::Start(offsets[0]))
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    let mut manifest = vec![0_u8; (offsets[1] - offsets[0]) as usize];
    source
        .read_exact(&mut manifest)
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    let manifest: Value = serde_json::from_slice(&manifest)
        .map_err(|error| RpcError::new("BASE_DATAPACK_INVALID", error.to_string()))?;
    let entries = manifest
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RpcError::new("BASE_DATAPACK_INVALID", "base pak manifest has no entries")
        })?;
    if entries.len() + 1 != count {
        return Err(RpcError::new(
            "BASE_DATAPACK_INVALID",
            "base pak manifest/index entry counts differ",
        ));
    }
    let mut result = HashMap::new();
    for (position, entry) in entries.iter().enumerate() {
        let name = entry
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new("BASE_DATAPACK_INVALID", "base pak entry has no path"))?;
        result.insert(
            name.to_owned(),
            (
                offsets[position + 1],
                offsets[position + 2] - offsets[position + 1],
            ),
        );
    }
    Ok(result)
}

fn hash_file_range_into(
    path: &Path,
    offset: u64,
    length: u64,
    digest: &mut Sha256,
) -> Result<(), RpcError> {
    let mut source =
        fs::File::open(path).map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    source
        .seek(SeekFrom::Start(offset))
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    let mut limited = source.take(length);
    let copied = std::io::copy(&mut limited, &mut DigestWriter(digest))
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    if copied != length {
        return Err(RpcError::new(
            "BASE_DATAPACK_INVALID",
            "base pak entry is truncated",
        ));
    }
    Ok(())
}

struct DigestWriter<'a>(&'a mut Sha256);
impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn copy_file_range(
    path: &Path,
    offset: u64,
    length: u64,
    target: &mut fs::File,
) -> Result<(), RpcError> {
    let mut source =
        fs::File::open(path).map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    source
        .seek(SeekFrom::Start(offset))
        .map_err(|error| io_error("BASE_DATAPACK_INVALID", path, error))?;
    let copied = std::io::copy(&mut source.take(length), target)
        .map_err(|error| io_error("OFFICE_PACK_WRITE_FAILED", path, error))?;
    if copied != length {
        return Err(RpcError::new(
            "BASE_DATAPACK_INVALID",
            "base pak entry is truncated",
        ));
    }
    Ok(())
}

fn collect_pack_tree(
    root: &Path,
    current: &Path,
    prefix: &str,
    exclude_source_maps: bool,
    exclude_root_files: &[String],
    resources: &mut Vec<(String, PathBuf)>,
) -> Result<(), RpcError> {
    let mut entries = fs::read_dir(current)
        .map_err(|error| io_error("OFFICE_PACK_READ_FAILED", current, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io_error("OFFICE_PACK_READ_FAILED", current, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|error| io_error("OFFICE_PACK_READ_FAILED", &path, error))?;
        if metadata.is_dir() {
            collect_pack_tree(
                root,
                &path,
                prefix,
                exclude_source_maps,
                exclude_root_files,
                resources,
            )?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).expect("walk remains under root");
            let relative = relative.to_string_lossy().replace('\\', "/");
            if (!relative.contains('/')
                && exclude_root_files
                    .iter()
                    .any(|excluded| excluded == &relative))
                || (exclude_source_maps && relative.ends_with(".map"))
            {
                continue;
            }
            resources.push((
                if prefix.is_empty() {
                    relative
                } else {
                    format!("{prefix}/{relative}")
                },
                path,
            ));
        }
    }
    Ok(())
}

fn hash_file_into(path: &Path, digest: &mut Sha256) -> Result<(), RpcError> {
    let mut source =
        fs::File::open(path).map_err(|error| io_error("OFFICE_PACK_READ_FAILED", path, error))?;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|error| io_error("OFFICE_PACK_READ_FAILED", path, error))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(())
}

pub fn record_state(
    generation_root: &Path,
    generation_id: &str,
    state: &str,
    evidence: Value,
) -> Result<Value, RpcError> {
    validate_generation_id(generation_id)?;
    const STATES: &[&str] = &[
        "materialized",
        "applied",
        "validated",
        "finalized",
        "smoke-passed",
        "ready",
        "active",
        "failed",
    ];
    if !STATES.contains(&state) {
        return Err(RpcError::new(
            "INVALID_GENERATION_STATE",
            format!("unsupported generation state: {state}"),
        ));
    }
    let marker_path = generation_root
        .join("generations")
        .join(generation_id)
        .join("generation.json");
    let mut marker: Value = serde_json::from_slice(
        &fs::read(&marker_path)
            .map_err(|error| io_error("GENERATION_INVALID", &marker_path, error))?,
    )
    .map_err(|error| RpcError::new("GENERATION_INVALID", error.to_string()))?;
    marker["state"] = Value::String(state.to_owned());
    marker["updatedAt"] = Value::from(now_ms());
    marker["evidence"] = evidence;
    let temporary = marker_path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&marker).expect("marker serializes"),
    )
    .map_err(|error| io_error("GENERATION_UPDATE_FAILED", &temporary, error))?;
    atomic_replace(&temporary, &marker_path)
        .map_err(|error| io_error("GENERATION_UPDATE_FAILED", &marker_path, error))?;
    Ok(marker)
}

pub fn activate(generation_root: &Path, generation_id: &str) -> Result<Value, RpcError> {
    validate_generation_id(generation_id)?;
    let generations = generation_root.join("generations");
    let target = generations.join(generation_id);
    let marker_path = target.join("generation.json");
    if !marker_path.is_file() {
        return Err(RpcError::new(
            "GENERATION_NOT_READY",
            format!("generation has no marker: {generation_id}"),
        ));
    }
    let marker: Value = serde_json::from_slice(
        &fs::read(&marker_path)
            .map_err(|error| io_error("GENERATION_INVALID", &marker_path, error))?,
    )
    .map_err(|error| RpcError::new("GENERATION_INVALID", error.to_string()))?;
    if marker.get("state").and_then(Value::as_str) != Some("ready") {
        return Err(RpcError::new(
            "GENERATION_NOT_READY",
            format!(
                "generation state is {}, expected ready",
                marker
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ),
        ));
    }
    fs::create_dir_all(generation_root)
        .map_err(|error| io_error("ACTIVATION_FAILED", generation_root, error))?;
    let current = generation_root.join("current");
    let previous = generation_root.join("previous");
    let next = generation_root.join(format!(".current.{}.tmp", std::process::id()));
    let old_target = read_directory_link(&current).ok();
    if next.exists() {
        remove_directory_link(&next)
            .map_err(|error| io_error("ACTIVATION_FAILED", &next, error))?;
    }
    symlink_dir(Path::new("generations").join(generation_id), &next)
        .map_err(|error| io_error("ACTIVATION_FAILED", &next, error))?;
    replace_directory_link(&next, &current)
        .map_err(|error| io_error("ACTIVATION_FAILED", &current, error))?;
    if let Some(ref old) = old_target {
        let previous_next = generation_root.join(format!(".previous.{}.tmp", std::process::id()));
        let _ = remove_directory_link(&previous_next);
        symlink_dir(old, &previous_next)
            .map_err(|error| io_error("ACTIVATION_FAILED", &previous_next, error))?;
        replace_directory_link(&previous_next, &previous)
            .map_err(|error| io_error("ACTIVATION_FAILED", &previous, error))?;
    }
    Ok(serde_json::json!({
        "generationId": generation_id,
        "current": current,
        "previous": old_target,
        "activatedAt": now_ms(),
    }))
}

fn copy_tree(
    source: &Path,
    target: &Path,
    derived: bool,
    stats: &mut MaterializeStats,
) -> Result<(), RpcError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| io_error("MATERIALIZE_FAILED", source, error))?;
    if metadata.file_type().is_symlink() {
        let link =
            fs::read_link(source).map_err(|error| io_error("MATERIALIZE_FAILED", source, error))?;
        if target.exists() || target.is_symlink() {
            fs::remove_file(target)
                .map_err(|error| io_error("MATERIALIZE_FAILED", target, error))?;
        }
        symlink_path(&link, target)
            .map_err(|error| io_error("MATERIALIZE_FAILED", target, error))?;
    } else if metadata.is_dir() {
        fs::create_dir_all(target)
            .map_err(|error| io_error("MATERIALIZE_FAILED", target, error))?;
        copy_permissions(&metadata, target)?;
        for entry in
            fs::read_dir(source).map_err(|error| io_error("MATERIALIZE_FAILED", source, error))?
        {
            let entry = entry.map_err(|error| io_error("MATERIALIZE_FAILED", source, error))?;
            copy_tree(
                &entry.path(),
                &target.join(entry.file_name()),
                derived,
                stats,
            )?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("MATERIALIZE_FAILED", parent, error))?;
        }
        clone_link_or_copy(source, target, derived, stats)?;
        copy_permissions(&metadata, target)?;
    } else {
        return Err(RpcError::new(
            "MATERIALIZE_FAILED",
            format!("unsupported baseline entry: {}", source.display()),
        ));
    }
    Ok(())
}

fn apply_overlay_tree(source: &Path, target: &Path, replace: bool) -> Result<(), RpcError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| io_error("OVERLAY_APPLY_FAILED", source, error))?;
    if metadata.is_dir() && !replace {
        fs::create_dir_all(target)
            .map_err(|error| io_error("OVERLAY_APPLY_FAILED", target, error))?;
        for entry in
            fs::read_dir(source).map_err(|error| io_error("OVERLAY_APPLY_FAILED", source, error))?
        {
            let entry = entry.map_err(|error| io_error("OVERLAY_APPLY_FAILED", source, error))?;
            apply_overlay_tree(&entry.path(), &target.join(entry.file_name()), false)?;
        }
        return Ok(());
    }
    let parent = target
        .parent()
        .ok_or_else(|| RpcError::new("OVERLAY_APPLY_FAILED", "overlay target has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| io_error("OVERLAY_APPLY_FAILED", parent, error))?;
    let temporary = parent.join(format!(".overlay.{}.{}.tmp", std::process::id(), now_ms()));
    let mut stats = MaterializeStats::default();
    copy_tree(source, &temporary, false, &mut stats)?;
    if target.exists() || target.is_symlink() {
        let old = parent.join(format!(".overlay.{}.{}.old", std::process::id(), now_ms()));
        fs::rename(target, &old)
            .map_err(|error| io_error("OVERLAY_REPLACE_FAILED", target, error))?;
        if let Err(error) = fs::rename(&temporary, target) {
            let _ = fs::rename(&old, target);
            return Err(io_error("OVERLAY_REPLACE_FAILED", target, error));
        }
        if old.is_dir() {
            fs::remove_dir_all(old)
        } else {
            fs::remove_file(old)
        }
        .map_err(|error| io_error("OVERLAY_REPLACE_FAILED", target, error))?;
    } else {
        fs::rename(&temporary, target)
            .map_err(|error| io_error("OVERLAY_REPLACE_FAILED", target, error))?;
    }
    Ok(())
}

fn clone_link_or_copy(
    source: &Path,
    target: &Path,
    _derived: bool,
    stats: &mut MaterializeStats,
) -> Result<(), RpcError> {
    #[cfg(windows)]
    if _derived && fs::hard_link(source, target).is_ok() {
        stats.link_hits += 1;
        return Ok(());
    }
    let size = fs::metadata(source)
        .map_err(|error| io_error("MATERIALIZE_FAILED", source, error))?
        .len();
    let cloned = clone_or_copy(source, target)?;
    if cloned {
        stats.clone_hits += 1;
    } else {
        stats.copied_files += 1;
        stats.copied_bytes += size;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_dir(source: impl AsRef<Path>, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(unix)]
fn remove_directory_link(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

#[cfg(unix)]
fn read_directory_link(path: &Path) -> std::io::Result<PathBuf> {
    fs::read_link(path)
}

#[cfg(windows)]
fn read_directory_link(path: &Path) -> std::io::Result<PathBuf> {
    junction::get_target(path)
}

#[cfg(windows)]
fn remove_directory_link(path: &Path) -> std::io::Result<()> {
    match junction::delete(path) {
        Ok(()) => fs::remove_dir(path),
        Err(error) => fs::remove_dir(path).map_err(|_| error),
    }
}

#[cfg(unix)]
fn replace_directory_link(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_directory_link(source: &Path, target: &Path) -> std::io::Result<()> {
    let link_target = junction::get_target(source)?;
    if fs::symlink_metadata(target).is_ok() {
        remove_directory_link(target)?;
    }
    junction::create(link_target, target)?;
    junction::delete(source)
}

#[cfg(windows)]
fn symlink_dir(source: impl AsRef<Path>, target: &Path) -> std::io::Result<()> {
    let source = source.as_ref();
    let resolved = if source.is_absolute() {
        source.to_path_buf()
    } else {
        target
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(source)
    };
    junction::create(resolved, target)
}

#[cfg(unix)]
fn symlink_path(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn symlink_path(source: &Path, target: &Path) -> std::io::Result<()> {
    let resolved = target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(source);
    if resolved.is_dir() {
        junction::create(resolved, target)
    } else {
        std::os::windows::fs::symlink_file(source, target)
    }
}

#[cfg(unix)]
fn copy_permissions(metadata: &fs::Metadata, target: &Path) -> Result<(), RpcError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        target,
        fs::Permissions::from_mode(metadata.permissions().mode()),
    )
    .map_err(|error| io_error("MATERIALIZE_FAILED", target, error))
}

#[cfg(windows)]
fn copy_permissions(_metadata: &fs::Metadata, _target: &Path) -> Result<(), RpcError> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_or_copy(source: &Path, target: &Path) -> Result<bool, RpcError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_c = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| RpcError::new("MATERIALIZE_FAILED", "source path contains NUL"))?;
    let target_c = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| RpcError::new("MATERIALIZE_FAILED", "target path contains NUL"))?;
    if unsafe { libc::clonefile(source_c.as_ptr(), target_c.as_ptr(), 0) } == 0 {
        return Ok(true);
    }
    fs::copy(source, target)
        .map(|_| false)
        .map_err(|error| io_error("MATERIALIZE_FAILED", target, error))
}

#[cfg(not(target_os = "macos"))]
fn clone_or_copy(source: &Path, target: &Path) -> Result<bool, RpcError> {
    fs::copy(source, target)
        .map(|_| false)
        .map_err(|error| io_error("MATERIALIZE_FAILED", target, error))
}

fn validate_generation_id(value: &str) -> Result<(), RpcError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RpcError::new(
            "INVALID_GENERATION_ID",
            "generation ID must be a safe path component",
        ));
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), RpcError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RpcError::new(
            "INVALID_OVERLAY_PATH",
            format!(
                "overlay target must be a safe relative path: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn io_error(code: &str, path: &Path, error: std::io::Error) -> RpcError {
    RpcError::new(code, format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_root_rejects_the_internal_generations_directory() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("Baseline.app");
        fs::create_dir_all(&baseline).unwrap();
        let error =
            materialize_derived(&directory.path().join("generations"), "g1", &baseline, None)
                .unwrap_err();
        assert_eq!(error.code, "INVALID_GENERATION_ROOT");
    }

    #[test]
    fn overlay_merges_and_activation_retains_previous() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("Example.app");
        fs::create_dir(&baseline).unwrap();
        fs::write(baseline.join("kept"), "baseline").unwrap();
        let overlay = directory.path().join("overlay");
        fs::create_dir(&overlay).unwrap();
        fs::write(overlay.join("added"), "new").unwrap();
        let root = directory.path().join("client");
        let first = materialize_derived(&root, "one", &baseline, None).unwrap();
        apply_overlays(
            &first.application_path,
            &[Overlay {
                source: overlay.clone(),
                target_relative_path: PathBuf::from("runtime"),
                replace: false,
            }],
        )
        .unwrap();
        assert!(first.application_path.join("kept").exists());
        record_state(&root, "one", "ready", Value::Null).unwrap();
        assert!(first.application_path.join("runtime/added").exists());
        activate(&root, "one").unwrap();
        materialize_derived(&root, "two", &baseline, None).unwrap();
        record_state(&root, "two", "ready", Value::Null).unwrap();
        activate(&root, "two").unwrap();
        assert!(
            read_directory_link(&root.join("current"))
                .unwrap()
                .ends_with(Path::new("generations/two"))
        );
        assert!(
            read_directory_link(&root.join("previous"))
                .unwrap()
                .ends_with(Path::new("generations/one"))
        );
    }

    #[test]
    fn runtime_record_accepts_active_generation_state() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("Example.app");
        fs::create_dir(&baseline).unwrap();
        let root = directory.path().join("client");
        materialize_derived(&root, "active-generation", &baseline, None).unwrap();

        let result = record_state(
            &root,
            "active-generation",
            "active",
            serde_json::json!({"runtimeMarker": {"executorId": "mac-rust"}}),
        )
        .unwrap();

        assert_eq!(result["state"], "active");
        let marker: Value = serde_json::from_slice(
            &fs::read(root.join("generations/active-generation/generation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["state"], "active");
        assert_eq!(
            marker["evidence"]["runtimeMarker"]["executorId"],
            "mac-rust"
        );
    }

    #[test]
    fn replacing_complete_overlay_removes_baseline_files() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("Example.app");
        fs::create_dir_all(application.join("runtime")).unwrap();
        fs::write(application.join("runtime/stale"), "baseline").unwrap();
        let overlay = directory.path().join("overlay");
        fs::create_dir(&overlay).unwrap();
        fs::write(overlay.join("current"), "release").unwrap();

        apply_overlays(
            &application,
            &[Overlay {
                source: overlay,
                target_relative_path: PathBuf::from("runtime"),
                replace: true,
            }],
        )
        .unwrap();

        assert!(!application.join("runtime/stale").exists());
        assert_eq!(
            fs::read_to_string(application.join("runtime/current")).unwrap(),
            "release"
        );
    }

    #[test]
    fn overlay_replaces_a_shared_file_without_mutating_its_peer() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("application");
        fs::create_dir(&application).unwrap();
        let base = directory.path().join("base");
        fs::write(&base, "baseline-long-value").unwrap();
        fs::hard_link(&base, application.join("runtime.js")).unwrap();
        let overlay = directory.path().join("runtime.js");
        fs::write(&overlay, "new").unwrap();
        apply_overlays(
            &application,
            &[Overlay {
                source: overlay,
                target_relative_path: PathBuf::from("runtime.js"),
                replace: false,
            }],
        )
        .unwrap();
        assert_eq!(fs::read_to_string(base).unwrap(), "baseline-long-value");
        assert_eq!(
            fs::read_to_string(application.join("runtime.js")).unwrap(),
            "new"
        );
    }

    #[test]
    fn derived_generation_survives_removal_of_its_base() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("base/Doubao.app");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("payload"), "base").unwrap();
        let root = directory.path().join("client");
        let derived =
            materialize_derived(&root, "derived", &base, Some("base-generation")).unwrap();
        fs::remove_dir_all(base.parent().unwrap()).unwrap();
        assert_eq!(
            fs::read_to_string(derived.application_path.join("payload")).unwrap(),
            "base"
        );
        assert_eq!(derived.derived_from.as_deref(), Some("base-generation"));
    }

    #[test]
    fn office_pack_uses_final_overlay_tree_and_flow_biz_resources() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("Doubao");
        let webcontents = application.join("resources/local_webcontents");
        let office = webcontents.join("apps/doubao-office");
        let word = office.join("static/v/w");
        let biz = webcontents.join("biz/static/js");
        fs::create_dir_all(&word).unwrap();
        fs::create_dir_all(&biz).unwrap();
        fs::write(office.join("index.html"), "office").unwrap();
        fs::write(word.join("runtime.js"), "selected-bear").unwrap();
        fs::write(word.join("formula.js"), "equation").unwrap();
        fs::write(biz.join("entry.js"), "flow").unwrap();
        fs::write(biz.join("entry.js.map"), "source-map").unwrap();
        fs::write(office.join("doubao_office.pak"), "stale-pack").unwrap();

        let result = pack_chromium_datapack(
            &application,
            &[
                DataPackResourceTree {
                    root_relative: PathBuf::from("resources/local_webcontents/apps/doubao-office"),
                    prefix: String::new(),
                    exclude_source_maps: false,
                    exclude_root_files: vec!["doubao_office.pak".to_owned()],
                },
                DataPackResourceTree {
                    root_relative: PathBuf::from("resources/local_webcontents/biz"),
                    prefix: "biz".to_owned(),
                    exclude_source_maps: true,
                    exclude_root_files: Vec::new(),
                },
            ],
            Path::new("resources/local_webcontents/apps/doubao-office/doubao_office.pak"),
            "win",
            "x64",
            "doubao-office",
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(result["entries"], 4);
        assert!(result["sha256"].as_str().unwrap().starts_with("sha256:"));
        let packed = fs::read(result["output"].as_str().unwrap()).unwrap();
        assert_eq!(u32::from_le_bytes(packed[0..4].try_into().unwrap()), 5);
        let manifest_offset = u32::from_le_bytes(packed[14..18].try_into().unwrap()) as usize;
        let next_offset = u32::from_le_bytes(packed[20..24].try_into().unwrap()) as usize;
        let manifest: Value =
            serde_json::from_slice(&packed[manifest_offset..next_offset]).unwrap();
        let paths = manifest["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"static/v/w/runtime.js"));
        assert!(paths.contains(&"static/v/w/formula.js"));
        assert!(paths.contains(&"biz/static/js/entry.js"));
        assert!(!paths.contains(&"biz/static/js/entry.js.map"));
        assert!(!paths.contains(&"doubao_office.pak"));

        let base_pack = directory.path().join("base-doubao-office.pak");
        fs::copy(result["output"].as_str().unwrap(), &base_pack).unwrap();
        let base_digest = result["sha256"].as_str().unwrap().to_owned();
        fs::write(word.join("runtime.js"), "selected-bear-v2-longer").unwrap();
        let full = pack_chromium_datapack(
            &application,
            &[
                DataPackResourceTree {
                    root_relative: PathBuf::from("resources/local_webcontents/apps/doubao-office"),
                    prefix: String::new(),
                    exclude_source_maps: false,
                    exclude_root_files: vec!["doubao_office.pak".to_owned()],
                },
                DataPackResourceTree {
                    root_relative: PathBuf::from("resources/local_webcontents/biz"),
                    prefix: "biz".to_owned(),
                    exclude_source_maps: true,
                    exclude_root_files: Vec::new(),
                },
            ],
            Path::new("resources/local_webcontents/apps/doubao-office/doubao_office.pak"),
            "win",
            "x64",
            "doubao-office",
            None,
            None,
            &[],
        )
        .unwrap();
        let full_bytes = fs::read(full["output"].as_str().unwrap()).unwrap();
        let incremental = pack_chromium_datapack(
            &application,
            &[
                DataPackResourceTree {
                    root_relative: PathBuf::from("resources/local_webcontents/apps/doubao-office"),
                    prefix: String::new(),
                    exclude_source_maps: false,
                    exclude_root_files: vec!["doubao_office.pak".to_owned()],
                },
                DataPackResourceTree {
                    root_relative: PathBuf::from("resources/local_webcontents/biz"),
                    prefix: "biz".to_owned(),
                    exclude_source_maps: true,
                    exclude_root_files: Vec::new(),
                },
            ],
            Path::new("resources/local_webcontents/apps/doubao-office/doubao_office.pak"),
            "win",
            "x64",
            "doubao-office",
            Some(&base_pack),
            Some(&base_digest),
            &["static/v/w".to_owned()],
        )
        .unwrap();
        assert!(incremental["incremental"].as_bool().unwrap());
        assert_eq!(
            fs::read(incremental["output"].as_str().unwrap()).unwrap(),
            full_bytes
        );
        assert_eq!(incremental["sha256"], full["sha256"]);
        assert_eq!(incremental["contentHash"], full["contentHash"]);
    }

    #[test]
    fn materialize_normalizes_provenance_suffix_after_app() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("Example.app.validated-123");
        fs::create_dir(&baseline).unwrap();
        fs::write(baseline.join("payload"), b"ok").unwrap();
        let root = directory.path().join("client");
        let generation = materialize_derived(&root, "generation-1", &baseline, None).unwrap();
        assert_eq!(
            generation.application_path.file_name().unwrap(),
            "Example.app"
        );
        assert!(generation.application_path.join("payload").is_file());
    }
}

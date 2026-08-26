use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use workbench_core::now_ms;
use workbench_protocol::Request;

static LOG: OnceLock<LogSink> = OnceLock::new();

struct LogSink {
    component: String,
    path: PathBuf,
    lock: Mutex<()>,
}

pub fn init_logging(component: impl Into<String>, path: impl Into<PathBuf>) {
    let _ = LOG.set(LogSink {
        component: component.into(),
        path: path.into(),
        lock: Mutex::new(()),
    });
}

pub fn request_event(level: &str, event: &str, request: &Request, fields: Value) {
    event_fields(
        level,
        event,
        json!({
            "requestId": request.request_id,
            "correlationId": request.correlation_id.as_deref().unwrap_or(&request.request_id),
            "parentRequestId": request.parent_request_id,
            "action": request.action,
            "fields": fields,
        }),
    );
}

pub fn event_fields(level: &str, event: &str, fields: Value) {
    let Some(sink) = LOG.get() else { return };
    let record = json!({"timestamp": now_ms(), "level": level, "component": sink.component, "event": event, "data": fields});
    let Ok(_guard) = sink.lock.lock() else { return };
    if let Some(parent) = Path::new(&sink.path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sink.path)
    {
        let _ = serde_json::to_writer(&mut file, &record);
        let _ = file.write_all(b"\n");
    }
}

pub fn task_event(task_id: &str, level: &str, event: &str, fields: Value) {
    let Some(sink) = LOG.get() else { return };
    if !task_id.starts_with("task_")
        || task_id.len() > 64
        || !task_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return;
    }
    let root = sink
        .path
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let path = root.join("tasks").join(task_id).join("events.jsonl");
    let record = json!({"timestamp": now_ms(), "level": level, "component": sink.component, "event": event, "data": fields});
    let Ok(_guard) = sink.lock.lock() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
        && let Ok(encoded) = serde_json::to_vec(&record)
    {
        let _ = file.write_all(&encoded);
        let _ = file.write_all(b"\n");
    }
}

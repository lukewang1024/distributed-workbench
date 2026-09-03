use crate::now_ms;
use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;
use uuid::Uuid;
use workbench_schema::{Task, TaskError as TaskFailure, TaskEvent, TaskState};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TaskError {
    #[error("task not found")]
    NotFound,
    #[error("invalid transition from {from:?} to {to:?}")]
    InvalidTransition { from: TaskState, to: TaskState },
}

#[derive(Debug, Default)]
pub struct TaskTable {
    tasks: BTreeMap<String, Task>,
    idempotency: BTreeMap<String, String>,
}

impl TaskTable {
    pub fn from_tasks(tasks: impl IntoIterator<Item = Task>) -> Self {
        let mut table = Self::default();
        for task in tasks {
            table.idempotency.insert(
                scoped_idempotency_key(
                    &task.workspace_session_id,
                    &task.executor_id,
                    &task.capability,
                    &task.idempotency_key,
                ),
                task.id.clone(),
            );
            table.tasks.insert(task.id.clone(), task);
        }
        table
    }

    pub fn submit(
        &mut self,
        workspace_session_id: impl Into<String>,
        executor_id: impl Into<String>,
        capability: impl Into<String>,
        input: Value,
        idempotency_key: impl Into<String>,
    ) -> (Task, bool) {
        self.submit_traced(
            workspace_session_id,
            executor_id,
            capability,
            input,
            idempotency_key,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_traced(
        &mut self,
        workspace_session_id: impl Into<String>,
        executor_id: impl Into<String>,
        capability: impl Into<String>,
        input: Value,
        idempotency_key: impl Into<String>,
        correlation_id: Option<String>,
        request_id: Option<String>,
    ) -> (Task, bool) {
        let workspace_session_id = workspace_session_id.into();
        let executor_id = executor_id.into();
        let capability = capability.into();
        let idempotency_key = idempotency_key.into();
        let scoped_key = scoped_idempotency_key(
            &workspace_session_id,
            &executor_id,
            &capability,
            &idempotency_key,
        );
        if let Some(id) = self.idempotency.get(&scoped_key) {
            return (self.tasks[id].clone(), true);
        }
        let now = now_ms();
        let mut task = Task {
            id: format!("task_{}", Uuid::new_v4().simple()),
            correlation_id,
            request_id,
            workspace_session_id,
            executor_id,
            capability,
            input,
            output: None,
            error: None,
            idempotency_key: idempotency_key.clone(),
            state: TaskState::Queued,
            attempt: 0,
            created_at: now,
            updated_at: now,
            events: Vec::new(),
        };
        Self::push_event(&mut task, "task.queued", Value::Null);
        self.idempotency.insert(scoped_key, task.id.clone());
        self.tasks.insert(task.id.clone(), task.clone());
        (task, false)
    }

    pub fn transition(
        &mut self,
        id: &str,
        state: TaskState,
        output: Option<Value>,
        error: Option<TaskFailure>,
    ) -> Result<Task, TaskError> {
        let task = self.tasks.get_mut(id).ok_or(TaskError::NotFound)?;
        let allowed = matches!(
            (&task.state, &state),
            (TaskState::Queued, TaskState::Running)
                | (TaskState::Queued, TaskState::Cancelled)
                | (TaskState::Running, TaskState::CancelRequested)
                | (TaskState::CancelRequested, TaskState::Cancelling)
                | (TaskState::Cancelling, TaskState::Cancelled)
                | (TaskState::Cancelling, TaskState::FailedToCancel)
                | (TaskState::Running, TaskState::Succeeded)
                | (TaskState::Running, TaskState::Failed)
                | (TaskState::Running, TaskState::Cancelled)
                | (TaskState::Running, TaskState::TimedOut)
                | (TaskState::Running, TaskState::OutcomeUnknown)
        );
        if !allowed {
            return Err(TaskError::InvalidTransition {
                from: task.state.clone(),
                to: state,
            });
        }
        if matches!(state, TaskState::Running) {
            task.attempt += 1;
        }
        task.state = state;
        task.output = output;
        task.error = error;
        task.updated_at = now_ms();
        let event_type = format!(
            "task.{}",
            serde_json::to_value(&task.state).unwrap().as_str().unwrap()
        );
        Self::push_event(task, &event_type, Value::Null);
        Ok(task.clone())
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn retry(&mut self, id: &str) -> Result<Task, TaskError> {
        let task = self.tasks.get_mut(id).ok_or(TaskError::NotFound)?;
        if !matches!(
            task.state,
            TaskState::Failed
                | TaskState::TimedOut
                | TaskState::OutcomeUnknown
                | TaskState::FailedToCancel
        ) {
            return Err(TaskError::InvalidTransition {
                from: task.state.clone(),
                to: TaskState::Queued,
            });
        }
        task.state = TaskState::Queued;
        task.output = None;
        task.error = None;
        task.updated_at = now_ms();
        Self::push_event(task, "task.retried", Value::Null);
        Ok(task.clone())
    }

    pub fn snapshot(&self) -> Vec<Task> {
        self.tasks.values().cloned().collect()
    }

    pub fn persistence_snapshot(&self) -> Vec<Task> {
        self.tasks
            .values()
            .cloned()
            .map(|mut task| {
                redact_sensitive_value(&mut task.input);
                if let Some(output) = &mut task.output {
                    redact_sensitive_value(output);
                }
                if let Some(error) = &mut task.error {
                    if error.message.contains("token_") {
                        error.message = "<redacted>".into();
                    }
                    redact_sensitive_value(&mut error.details);
                }
                task
            })
            .collect()
    }

    pub fn prune_terminal_before(&mut self, cutoff: u64) -> Vec<Task> {
        let ids: Vec<String> = self
            .tasks
            .values()
            .filter(|task| task.state.terminal() && task.updated_at < cutoff)
            .map(|task| task.id.clone())
            .collect();
        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(task) = self.tasks.remove(&id) {
                self.idempotency.remove(&scoped_idempotency_key(
                    &task.workspace_session_id,
                    &task.executor_id,
                    &task.capability,
                    &task.idempotency_key,
                ));
                removed.push(task);
            }
        }
        removed
    }

    pub fn prune_retention(
        &mut self,
        now: u64,
        retention_ms: u64,
        max_count: usize,
        max_bytes: usize,
    ) -> Vec<Task> {
        let cutoff = now.saturating_sub(retention_ms);
        let mut remove_ids: std::collections::BTreeSet<String> = self
            .tasks
            .values()
            .filter(|task| task.state.terminal() && task.updated_at < cutoff)
            .map(|task| task.id.clone())
            .collect();
        let mut retained_count = self.tasks.len().saturating_sub(remove_ids.len());
        let mut retained_bytes = self
            .tasks
            .values()
            .filter(|task| !remove_ids.contains(&task.id))
            .map(|task| serde_json::to_vec(task).map_or(0, |encoded| encoded.len()))
            .sum::<usize>();
        let mut terminal = self
            .tasks
            .values()
            .filter(|task| task.state.terminal() && !remove_ids.contains(&task.id))
            .map(|task| {
                (
                    task.updated_at,
                    task.id.clone(),
                    serde_json::to_vec(task).map_or(0, |encoded| encoded.len()),
                )
            })
            .collect::<Vec<_>>();
        terminal.sort_by_key(|(updated_at, id, _)| (*updated_at, id.clone()));
        for (_, id, bytes) in terminal {
            if retained_count <= max_count && retained_bytes <= max_bytes {
                break;
            }
            remove_ids.insert(id);
            retained_count = retained_count.saturating_sub(1);
            retained_bytes = retained_bytes.saturating_sub(bytes);
        }
        let mut removed = Vec::with_capacity(remove_ids.len());
        for id in remove_ids {
            if let Some(task) = self.tasks.remove(&id) {
                self.idempotency.remove(&scoped_idempotency_key(
                    &task.workspace_session_id,
                    &task.executor_id,
                    &task.capability,
                    &task.idempotency_key,
                ));
                removed.push(task);
            }
        }
        removed
    }

    pub fn recover_orphans(&mut self) -> Vec<Task> {
        let mut recovered = Vec::new();
        for task in self
            .tasks
            .values_mut()
            .filter(|task| matches!(task.state, TaskState::Queued | TaskState::Running))
        {
            task.state = TaskState::OutcomeUnknown;
            task.error = Some(TaskFailure {
                code: "CONTROLLER_RESTARTED".to_owned(),
                message: "controller restarted before the task outcome was recorded".to_owned(),
                retryable: true,
                details: Value::Null,
            });
            task.updated_at = now_ms();
            Self::push_event(task, "task.outcome-unknown", Value::Null);
            recovered.push(task.clone());
        }
        recovered
    }

    fn push_event(task: &mut Task, event_type: &str, details: Value) {
        task.events.push(TaskEvent {
            sequence: task.events.len() as u64 + 1,
            timestamp: now_ms(),
            event_type: event_type.to_owned(),
            details,
        });
    }
}

fn redact_sensitive_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, item) in object {
                let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
                if ["token", "secret", "password", "credential"]
                    .iter()
                    .any(|suffix| normalized.ends_with(suffix))
                {
                    *item = Value::String("<redacted>".into());
                } else {
                    redact_sensitive_value(item);
                }
            }
        }
        Value::Array(values) => {
            let mut redact_next = false;
            for value in values {
                if redact_next {
                    *value = Value::String("<redacted>".into());
                    redact_next = false;
                    continue;
                }
                if let Value::String(text) = value {
                    let normalized = text.to_ascii_lowercase().replace(['_', '-'], "");
                    redact_next = ["token", "secret", "password", "credential"]
                        .iter()
                        .any(|marker| normalized.ends_with(marker));
                }
                redact_sensitive_value(value);
            }
        }
        Value::String(text) if text.contains("token_") => {
            *text = "<redacted>".into();
        }
        _ => {}
    }
}

fn scoped_idempotency_key(
    workspace_session_id: &str,
    executor_id: &str,
    capability: &str,
    key: &str,
) -> String {
    format!("{workspace_session_id}\0{executor_id}\0{capability}\0{key}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn persistence_snapshot_redacts_nested_task_secrets_without_mutating_live_input() {
        let mut table = TaskTable::default();
        let (task, _) = table.submit(
            "s",
            "e",
            "artifact.build",
            json!({"driverLeaseToken":"secret","nested":{"api_secret":"hidden"},"argv":["--notify-token","token_value"],"stdout":"prefix token_embedded suffix","safe":"kept"}),
            "build",
        );
        table
            .transition(&task.id, TaskState::Running, None, None)
            .unwrap();
        table
            .transition(
                &task.id,
                TaskState::Failed,
                None,
                Some(TaskFailure {
                    code: "FAILED".into(),
                    message: "leaked token_value".into(),
                    retryable: false,
                    details: Value::Null,
                }),
            )
            .unwrap();
        let persisted = table.persistence_snapshot();
        assert_eq!(persisted[0].input["driverLeaseToken"], "<redacted>");
        assert_eq!(persisted[0].input["nested"]["api_secret"], "<redacted>");
        assert_eq!(persisted[0].input["argv"][1], "<redacted>");
        assert_eq!(persisted[0].input["stdout"], "<redacted>");
        assert_eq!(persisted[0].input["safe"], "kept");
        assert_eq!(persisted[0].error.as_ref().unwrap().message, "<redacted>");
        assert_eq!(
            table.get(&task.id).unwrap().input["driverLeaseToken"],
            "secret"
        );
    }

    #[test]
    fn submission_is_idempotent_and_transitions_are_checked() {
        let mut table = TaskTable::default();
        let (first, reused) = table.submit("s", "e", "artifact.build", Value::Null, "same");
        assert!(!reused);
        let (second, reused) = table.submit("s", "e", "artifact.build", Value::Null, "same");
        assert!(reused);
        assert_eq!(first.id, second.id);
        table
            .transition(&first.id, TaskState::Running, None, None)
            .unwrap();
        table
            .transition(
                &first.id,
                TaskState::Succeeded,
                Some(Value::Bool(true)),
                None,
            )
            .unwrap();
        assert!(table.get(&first.id).unwrap().state.terminal());

        let (different_capability, reused) =
            table.submit("s", "e", "artifact.transfer", Value::Null, "same");
        assert!(!reused);
        assert_ne!(different_capability.id, first.id);

        let removed = table.prune_terminal_before(now_ms().saturating_add(1));
        assert_eq!(removed.len(), 1);
        let (resubmitted, reused) = table.submit("s", "e", "artifact.build", Value::Null, "same");
        assert!(!reused);
        assert_ne!(resubmitted.id, first.id);
    }

    #[test]
    fn restart_marks_inflight_tasks_outcome_unknown_and_retryable() {
        let mut table = TaskTable::default();
        let (task, _) = table.submit("s", "e", "artifact.build", Value::Null, "build");
        table
            .transition(&task.id, TaskState::Running, None, None)
            .unwrap();
        let recovered = table.recover_orphans();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, TaskState::OutcomeUnknown);
        assert!(recovered[0].error.as_ref().unwrap().retryable);
    }

    #[test]
    fn retention_keeps_active_tasks_and_evicts_oldest_terminal_tasks() {
        let mut table = TaskTable::default();
        let (first, _) = table.submit("s", "e", "filesystem.read", Value::Null, "first");
        table
            .transition(&first.id, TaskState::Running, None, None)
            .unwrap();
        table
            .transition(
                &first.id,
                TaskState::Succeeded,
                Some(json!({"value": "old"})),
                None,
            )
            .unwrap();
        let (second, _) = table.submit("s", "e", "filesystem.read", Value::Null, "second");
        table
            .transition(&second.id, TaskState::Running, None, None)
            .unwrap();
        table
            .transition(
                &second.id,
                TaskState::Succeeded,
                Some(json!({"value": "new"})),
                None,
            )
            .unwrap();
        let (running, _) = table.submit("s", "e", "command.run", Value::Null, "running");

        let removed = table.prune_retention(now_ms().saturating_add(1), u64::MAX, 2, usize::MAX);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, first.id);
        assert!(table.get(&second.id).is_some());
        assert!(table.get(&running.id).is_some());
    }
}

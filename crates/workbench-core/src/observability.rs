use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;
use workbench_schema::{Observation, Run, RunHealth, RunStatus};

const RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ObservabilityError {
    #[error("observability database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("invalid observability data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("run not found: {0}")]
    RunNotFound(String),
    #[error("invalid run transition: {0}")]
    InvalidTransition(String),
}

#[derive(Debug, Default, Clone)]
pub struct ObservationQuery {
    pub run_id: Option<String>,
    pub after_event_id: Option<u64>,
    pub limit: usize,
}

pub struct ObservabilityStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl ObservabilityStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ObservabilityError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS runs (run_id TEXT PRIMARY KEY, payload TEXT NOT NULL, started_at INTEGER NOT NULL, status TEXT NOT NULL); CREATE TABLE IF NOT EXISTS observations (event_id INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT, timestamp INTEGER NOT NULL, payload TEXT NOT NULL); CREATE INDEX IF NOT EXISTS observations_run_cursor ON observations(run_id,event_id); CREATE INDEX IF NOT EXISTS observations_time ON observations(timestamp);")?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
        })
    }

    pub fn start_run(&self, run: &Run) -> Result<(), ObservabilityError> {
        self.connection.lock().unwrap().execute(
            "INSERT INTO runs(run_id,payload,started_at,status) VALUES(?1,?2,?3,?4)",
            params![
                run.run_id,
                serde_json::to_string(run)?,
                run.started_at,
                "running"
            ],
        )?;
        Ok(())
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>, ObservabilityError> {
        let payload: Option<String> = self
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT payload FROM runs WHERE run_id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        payload
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub fn finish_run(
        &self,
        id: &str,
        status: RunStatus,
        finished_at: u64,
        outcome: Option<String>,
    ) -> Result<Run, ObservabilityError> {
        if status == RunStatus::Running {
            return Err(ObservabilityError::InvalidTransition(
                "finished run cannot remain running".into(),
            ));
        }
        let mut run = self
            .get_run(id)?
            .ok_or_else(|| ObservabilityError::RunNotFound(id.into()))?;
        if run.status != RunStatus::Running {
            return Err(ObservabilityError::InvalidTransition(format!(
                "run {id} is already terminal"
            )));
        }
        run.status = status;
        run.finished_at = Some(finished_at);
        run.business_outcome = outcome;
        run.health = if run.business_outcome.as_deref() == Some("failed") {
            RunHealth::Failed
        } else {
            match run.status {
                RunStatus::Completed => RunHealth::Healthy,
                RunStatus::Cancelled | RunStatus::Interrupted => RunHealth::Degraded,
                RunStatus::Running => RunHealth::Unknown,
            }
        };
        self.connection.lock().unwrap().execute(
            "UPDATE runs SET payload=?2,status=?3 WHERE run_id=?1",
            params![
                id,
                serde_json::to_string(&run)?,
                format!("{:?}", run.status).to_lowercase()
            ],
        )?;
        Ok(run)
    }

    pub fn list_runs(&self, limit: usize) -> Result<Vec<Run>, ObservabilityError> {
        let connection = self.connection.lock().unwrap();
        let mut statement =
            connection.prepare("SELECT payload FROM runs ORDER BY started_at DESC LIMIT ?1")?;
        let values = statement
            .query_map([limit.clamp(1, 500) as u64], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .collect()
    }

    pub fn append(&self, observation: &Observation) -> Result<u64, ObservabilityError> {
        let mut value = observation.clone();
        value.event_id = 0;
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "INSERT INTO observations(run_id,timestamp,payload) VALUES(?1,?2,?3)",
            params![
                value.run_id,
                value.timestamp,
                serde_json::to_string(&value)?
            ],
        )?;
        Ok(connection.last_insert_rowid() as u64)
    }

    pub fn query(&self, query: ObservationQuery) -> Result<Vec<Observation>, ObservabilityError> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare("SELECT event_id,payload FROM observations WHERE (?1 IS NULL OR run_id=?1) AND event_id>?2 ORDER BY event_id LIMIT ?3")?;
        let rows = statement
            .query_map(
                params![
                    query.run_id,
                    query.after_event_id.unwrap_or(0),
                    query.limit.clamp(1, 1000) as u64
                ],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, payload)| {
                let mut value: Observation = serde_json::from_str(&payload)?;
                value.event_id = id;
                Ok(value)
            })
            .collect()
    }

    pub fn prune(&self, now_ms: u64) -> Result<usize, ObservabilityError> {
        self.prune_with_max_bytes(now_ms, MAX_BYTES)
    }

    fn prune_with_max_bytes(
        &self,
        now_ms: u64,
        max_bytes: u64,
    ) -> Result<usize, ObservabilityError> {
        let cutoff = now_ms.saturating_sub(RETENTION_MS);
        let connection = self.connection.lock().unwrap();
        let mut removed =
            connection.execute("DELETE FROM observations WHERE timestamp<?1", [cutoff])?;
        connection.execute(
            "DELETE FROM runs WHERE started_at<?1 AND status!='running'",
            [cutoff],
        )?;
        while database_bytes(&self.path) > max_bytes {
            let batch = connection.execute("DELETE FROM observations WHERE event_id IN (SELECT event_id FROM observations ORDER BY event_id LIMIT MAX(1,(SELECT COUNT(*)/10 FROM observations)))", [])?;
            if batch == 0 {
                break;
            }
            removed += batch;
            connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }
        Ok(removed)
    }
}

fn database_bytes(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
    .iter()
    .filter_map(|item| std::fs::metadata(item).ok())
    .map(|metadata| metadata.len())
    .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn lifecycle_and_cursor_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObservabilityStore::open(dir.path().join("observability.db")).unwrap();
        let run = Run {
            run_id: "run_1".into(),
            workspace_session_id: None,
            agent_session_id: None,
            target_summary: "test".into(),
            created_by: "agent".into(),
            status: RunStatus::Running,
            health: RunHealth::Unknown,
            started_at: 1,
            finished_at: None,
            business_outcome: None,
        };
        store.start_run(&run).unwrap();
        for timestamp in [2, 3] {
            store
                .append(&Observation {
                    event_id: 0,
                    run_id: Some("run_1".into()),
                    timestamp,
                    node_id: "node".into(),
                    role: "controller".into(),
                    kind: "rpc".into(),
                    name: "status".into(),
                    status: "completed".into(),
                    duration_ms: None,
                    span_id: None,
                    parent_span_id: None,
                    request_id: None,
                    task_id: None,
                    process_id: None,
                    connection_id: None,
                    attributes: json!({}),
                })
                .unwrap();
        }
        let first = store
            .query(ObservationQuery {
                run_id: Some("run_1".into()),
                after_event_id: None,
                limit: 1,
            })
            .unwrap();
        assert_eq!(first.len(), 1);
        let second = store
            .query(ObservationQuery {
                run_id: Some("run_1".into()),
                after_event_id: Some(first[0].event_id),
                limit: 10,
            })
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(
            store
                .finish_run("run_1", RunStatus::Completed, 4, None)
                .unwrap()
                .status,
            RunStatus::Completed
        );
    }
    #[test]
    fn prunes_expired_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObservabilityStore::open(dir.path().join("o.db")).unwrap();
        let mut o = Observation {
            event_id: 0,
            run_id: None,
            timestamp: 1,
            node_id: "n".into(),
            role: "r".into(),
            kind: "k".into(),
            name: "n".into(),
            status: "s".into(),
            duration_ms: None,
            span_id: None,
            parent_span_id: None,
            request_id: None,
            task_id: None,
            process_id: None,
            connection_id: None,
            attributes: json!({}),
        };
        store.append(&o).unwrap();
        o.timestamp = RETENTION_MS + 2;
        assert_eq!(store.prune(o.timestamp).unwrap(), 1);
    }
    #[test]
    fn wal_reopens_after_unclean_style_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        {
            let store = ObservabilityStore::open(&path).unwrap();
            store
                .append(&Observation {
                    event_id: 0,
                    run_id: None,
                    timestamp: 1,
                    node_id: "n".into(),
                    role: "r".into(),
                    kind: "k".into(),
                    name: "n".into(),
                    status: "s".into(),
                    duration_ms: None,
                    span_id: None,
                    parent_span_id: None,
                    request_id: None,
                    task_id: None,
                    process_id: None,
                    connection_id: None,
                    attributes: json!({}),
                })
                .unwrap();
        }
        let reopened = ObservabilityStore::open(path).unwrap();
        assert_eq!(
            reopened
                .query(ObservationQuery {
                    limit: 10,
                    ..Default::default()
                })
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn size_pruning_repeats_until_no_observations_remain() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObservabilityStore::open(dir.path().join("o.db")).unwrap();
        for timestamp in 1..=25 {
            store
                .append(&Observation {
                    event_id: 0,
                    run_id: None,
                    timestamp,
                    node_id: "n".into(),
                    role: "r".into(),
                    kind: "k".into(),
                    name: "n".into(),
                    status: "s".into(),
                    duration_ms: None,
                    span_id: None,
                    parent_span_id: None,
                    request_id: None,
                    task_id: None,
                    process_id: None,
                    connection_id: None,
                    attributes: json!({"payload":"x".repeat(4096)}),
                })
                .unwrap();
        }
        assert_eq!(store.prune_with_max_bytes(25, 1).unwrap(), 25);
        assert!(
            store
                .query(ObservationQuery {
                    limit: 100,
                    ..Default::default()
                })
                .unwrap()
                .is_empty()
        );
    }
}

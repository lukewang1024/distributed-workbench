//! Executor-local arbitration shared by every incoming Controller.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf, sync::Mutex};
use workbench_core::{LeaseTable, atomic_replace, now_ms};
use workbench_protocol::RpcError;
use workbench_schema::Lease;

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    leases: Vec<Lease>,
    fences: std::collections::BTreeMap<String, u64>,
    active: BTreeSet<String>,
}
#[derive(Default)]
pub(crate) struct Resources {
    table: LeaseTable,
    active: BTreeSet<String>,
    quarantined: BTreeSet<String>,
    path: Option<PathBuf>,
}
pub(crate) struct Permit<'a> {
    state: &'a Mutex<Resources>,
    resources: Vec<String>,
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if self.resources.is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("resources");
        for key in &self.resources {
            state.active.remove(key);
        }
        // Failure leaves the durable active marker in place, so restart fails closed.
        let _ = state.persist();
    }
}
fn lease_error(error: workbench_core::LeaseError) -> RpcError {
    use workbench_core::LeaseError;
    RpcError::new(
        match error {
            LeaseError::Active { .. } => "RESOURCE_BUSY",
            LeaseError::NotFound => "RESOURCE_LEASE_REQUIRED",
            _ => "RESOURCE_LEASE_INVALID",
        },
        error.to_string(),
    )
}
fn string<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::new("INVALID_PARAMS", format!("{key} is required")))
}
impl Resources {
    pub fn open(path: PathBuf) -> Result<Self, RpcError> {
        let snapshot: Snapshot = if path.exists() {
            serde_json::from_slice(
                &std::fs::read(&path)
                    .map_err(|e| RpcError::new("RESOURCE_STATE_FAILED", e.to_string()))?,
            )
            .map_err(|e| RpcError::new("RESOURCE_STATE_FAILED", e.to_string()))?
        } else {
            Snapshot::default()
        };
        Ok(Self {
            table: LeaseTable::from_snapshot(snapshot.leases, snapshot.fences),
            quarantined: snapshot.active,
            path: Some(path),
            ..Self::default()
        })
    }
    fn persist(&self) -> Result<(), RpcError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut active = self.active.clone();
        active.extend(self.quarantined.iter().cloned());
        let snapshot = Snapshot {
            leases: self.table.persistence_snapshot(),
            fences: self.table.fence_snapshot(),
            active,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RpcError::new("RESOURCE_STATE_FAILED", e.to_string()))?;
        }
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(&snapshot).unwrap())
            .and_then(|()| atomic_replace(&temp, path))
            .map_err(|e| RpcError::new("RESOURCE_STATE_FAILED", e.to_string()))
    }
    fn available(&self, resource: &str) -> Result<(), RpcError> {
        if self.quarantined.contains(resource) {
            return Err(RpcError::new(
                "RESOURCE_RECOVERY_REQUIRED",
                format!("uncertain operation on {resource} survived Executor restart"),
            ));
        }
        if self.active.contains(resource) {
            return Err(RpcError::new(
                "RESOURCE_BUSY",
                format!("operation is still executing on {resource}"),
            ));
        }
        Ok(())
    }
    pub fn command(&mut self, action: &str, params: &Value) -> Result<Value, RpcError> {
        if action == "resource.lease.list" {
            let prefix = params.get("prefix").and_then(Value::as_str).unwrap_or("");
            let leases: Vec<_> = self
                .table
                .snapshot()
                .into_iter()
                .filter(|l| l.resource.starts_with(prefix))
                .map(|mut l| {
                    l.token.clear();
                    l
                })
                .collect();
            return Ok(json!(leases));
        }
        let resource = string(params, "resource")?;
        if action == "resource.lease.status" {
            let mut lease = self.table.get(resource).cloned();
            if let Some(value) = &mut lease {
                value.token.clear();
            }
            return Ok(
                json!({"lease": lease, "active": self.active.contains(resource), "quarantined": self.quarantined.contains(resource)}),
            );
        }
        if action == "resource.lease.recover" {
            if params
                .get("confirmExecutionStopped")
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err(RpcError::new(
                    "RESOURCE_RECOVERY_CONFIRMATION_REQUIRED",
                    "confirm the old operation and any child process have stopped",
                ));
            }
            if self.active.contains(resource) {
                return Err(RpcError::new(
                    "RESOURCE_BUSY",
                    "operation is still executing",
                ));
            }
            self.quarantined.remove(resource);
            self.persist()?;
            return Ok(json!({"resource":resource, "recovered":true}));
        }
        let owner = string(params, "owner")?;
        let ttl = params
            .get("ttlMs")
            .and_then(Value::as_u64)
            .unwrap_or(300_000);
        if ttl == 0 || ttl > 86_400_000 {
            return Err(RpcError::new(
                "INVALID_PARAMS",
                "TTL must be between 1 ms and 24 hours",
            ));
        }
        let lease = match action {
            "resource.lease.acquire" => {
                self.available(resource)?;
                self.table
                    .acquire(workbench_schema::LeaseKind::Resource, resource, owner, ttl)
                    .map_err(lease_error)?
            }
            "resource.lease.validate" => self
                .table
                .validate(resource, owner, string(params, "token")?)
                .map_err(lease_error)?
                .clone(),
            "resource.lease.renew" => self
                .table
                .renew(resource, owner, string(params, "token")?, ttl)
                .map_err(lease_error)?,
            "resource.lease.release" => {
                self.available(resource)?;
                self.table
                    .release(resource, owner, string(params, "token")?)
                    .map_err(lease_error)?
            }
            _ => return Err(RpcError::new("UNKNOWN_ACTION", action)),
        };
        self.persist()?;
        Ok(json!(lease))
    }
    pub fn begin<'a>(
        state: &'a Mutex<Self>,
        mut resources: Vec<String>,
        grants: &[Value],
    ) -> Result<Permit<'a>, RpcError> {
        resources.sort();
        resources.dedup();
        let mut locked = state.lock().expect("resources");
        for resource in &resources {
            locked.available(resource)?;
            let grant = grants
                .iter()
                .find(|g| g.get("resource").and_then(Value::as_str) == Some(resource.as_str()));
            if let Some(grant) = grant {
                locked
                    .table
                    .validate(resource, string(grant, "owner")?, string(grant, "token")?)
                    .map_err(lease_error)?;
            } else if locked
                .table
                .get(resource)
                .is_some_and(|l| l.expires_at > now_ms())
            {
                return Err(RpcError::new(
                    "RESOURCE_BUSY",
                    format!("{resource} has an outstanding lease"),
                ));
            }
        }
        for grant in grants {
            if !resources
                .iter()
                .any(|r| Some(r.as_str()) == grant.get("resource").and_then(Value::as_str))
            {
                return Err(RpcError::new(
                    "RESOURCE_LEASE_SCOPE_MISMATCH",
                    "lease does not authorize this operation",
                ));
            }
        }
        locked.active.extend(resources.iter().cloned());
        if !resources.is_empty() {
            locked.persist()?;
        }
        Ok(Permit { state, resources })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controllers_share_arbitration_and_old_tokens_cannot_release_new_lease() {
        let state = Mutex::new(Resources::default());
        let a = json!({"resource":"tunnel:e:t", "owner":"controller-a/session/agent"});
        let first = state
            .lock()
            .unwrap()
            .command("resource.lease.acquire", &a)
            .unwrap();
        assert!(
            state
                .lock()
                .unwrap()
                .command(
                    "resource.lease.acquire",
                    &json!({"resource":"tunnel:e:t", "owner":"controller-b/session/agent"})
                )
                .is_err()
        );
        let grant = json!({"resource":"tunnel:e:t", "owner":a["owner"], "token":first["token"]});
        let permit = Resources::begin(
            &state,
            vec!["tunnel:e:t".into()],
            std::slice::from_ref(&grant),
        )
        .unwrap();
        assert!(
            state
                .lock()
                .unwrap()
                .command("resource.lease.release", &grant)
                .is_err()
        );
        assert!(
            Resources::begin(
                &state,
                vec!["tunnel:e:t".into()],
                std::slice::from_ref(&grant)
            )
            .is_err()
        );
        drop(permit);
        state
            .lock()
            .unwrap()
            .command("resource.lease.release", &grant)
            .unwrap();
        let second = state
            .lock()
            .unwrap()
            .command("resource.lease.acquire", &a)
            .unwrap();
        assert!(second["fence"].as_u64() > first["fence"].as_u64());
        assert!(
            state
                .lock()
                .unwrap()
                .command("resource.lease.release", &grant)
                .is_err()
        );
        assert!(Resources::begin(&state, vec!["tunnel:e:t".into()], &[grant]).is_err());
    }
    #[test]
    fn expired_lease_cannot_be_taken_over_while_the_operation_is_active() {
        let state = Mutex::new(Resources::default());
        let lease = state
            .lock()
            .unwrap()
            .command(
                "resource.lease.acquire",
                &json!({"resource":"r", "owner":"a", "ttlMs":10}),
            )
            .unwrap();
        let grant = json!({"resource":"r", "owner":"a", "token":lease["token"]});
        let permit =
            Resources::begin(&state, vec!["r".into()], std::slice::from_ref(&grant)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(
            state
                .lock()
                .unwrap()
                .command(
                    "resource.lease.acquire",
                    &json!({"resource":"r", "owner":"b"})
                )
                .unwrap_err()
                .code,
            "RESOURCE_BUSY"
        );
        drop(permit);
        assert!(Resources::begin(&state, vec!["r".into()], &[grant]).is_err());
        assert!(
            state
                .lock()
                .unwrap()
                .command(
                    "resource.lease.acquire",
                    &json!({"resource":"r", "owner":"b"})
                )
                .is_ok()
        );
    }

    #[test]
    fn restart_retains_leases_and_quarantines_uncertain_operations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resources.json");
        let state = Mutex::new(Resources::open(path.clone()).unwrap());
        let lease = state
            .lock()
            .unwrap()
            .command(
                "resource.lease.acquire",
                &json!({"resource":"a", "owner":"one"}),
            )
            .unwrap();
        let grant = json!({"resource":"a", "owner":"one", "token":lease["token"]});
        let permit = Resources::begin(&state, vec!["a".into()], &[grant]).unwrap();
        let reopened = Mutex::new(Resources::open(path).unwrap());
        assert_eq!(
            Resources::begin(&reopened, vec!["a".into()], &[])
                .err()
                .unwrap()
                .code,
            "RESOURCE_RECOVERY_REQUIRED"
        );
        drop(permit);
    }
}

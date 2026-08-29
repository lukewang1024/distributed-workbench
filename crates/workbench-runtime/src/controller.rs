use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;
use workbench_core::{
    JsonStore, LeaseError, LeaseTable, ObservabilityStore, ObservationQuery, TaskTable,
    WorkbenchState, now_ms, sha256_bytes,
};
use workbench_protocol::{Request, Response, RpcError};
use workbench_schema::{
    ActivationTransaction, AgentInstance, AgentMutationOperation, AgentState, Approval,
    ApprovalState, Artifact, ArtifactLocation, CapabilityDescriptor, ControllerPeer,
    DriverHandoffRequest, DriverHandoffState, ExecutionKind, Executor, ExecutorEndpoint,
    Generation, GenerationState, Handoff, HealthStatus, LeaseKind, Metadata, Observation,
    Provenance, ReadGrant, ReadGrantState, Run, RunHealth, RunStatus, SessionAuthority,
    SessionState, Task, TaskState, TransactionJournalEntry, TransactionState, TransactionStepState,
    WorkspaceSession,
};

use crate::telemetry::{event_fields, request_event, task_event};
use crate::transport::{call_executor, call_executor_with_timeout};

#[derive(Clone, Default)]
struct TraceContext {
    correlation_id: String,
    request_id: String,
    run_id: Option<String>,
    span_id: Option<String>,
    agent_session_id: Option<String>,
}

#[derive(Clone)]
struct OperatorGrant {
    operator_id: String,
    token_digest: String,
    expires_at: u64,
}
#[derive(Clone)]
struct OperatorNonce {
    operator_id: String,
    action: String,
    target: String,
    reason: String,
    expires_at: u64,
}

thread_local! { static CURRENT_TRACE: RefCell<Option<TraceContext>> = const { RefCell::new(None) }; }

fn traced_request(action: impl Into<String>, params: Value) -> Request {
    let mut request = Request::new(action, params);
    CURRENT_TRACE.with(|current| {
        if let Some(trace) = current.borrow().as_ref() {
            request.correlation_id = Some(trace.correlation_id.clone());
            request.parent_request_id = Some(trace.request_id.clone());
            request.run_id = trace.run_id.clone();
            request.parent_span_id = trace.span_id.clone();
            request.agent_session_id = trace.agent_session_id.clone();
        }
    });
    request
}

fn current_trace() -> (Option<String>, Option<String>) {
    CURRENT_TRACE.with(|current| {
        current
            .borrow()
            .as_ref()
            .map(|trace| {
                (
                    Some(trace.correlation_id.clone()),
                    Some(trace.request_id.clone()),
                )
            })
            .unwrap_or_default()
    })
}
fn current_run_id() -> Option<String> {
    CURRENT_TRACE.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(|trace| trace.run_id.clone())
    })
}

fn transfer_event(stage: &str, fields: Value) {
    let (correlation_id, request_id) = current_trace();
    event_fields(
        "info",
        "artifact.transfer.stage",
        json!({"stage": stage, "correlationId": correlation_id, "requestId": request_id, "fields": fields}),
    );
}

fn transfer_error(mut error: RpcError, stage: &str, offset: u64, chunks: u64) -> RpcError {
    let original_details = std::mem::take(&mut error.details);
    error.details = json!({
        "stage": stage,
        "failureOffset": offset,
        "transferredBytes": offset,
        "chunks": chunks,
        "retryCount": 0,
        "cause": original_details,
    });
    transfer_event(
        "failed",
        json!({
            "stage": stage,
            "failureOffset": offset,
            "transferredBytes": offset,
            "chunks": chunks,
            "retryCount": 0,
            "errorCode": error.code,
            "errorMessage": error.message,
        }),
    );
    error
}

#[derive(Clone)]
pub struct Controller {
    id: String,
    store: JsonStore,
    state: Arc<Mutex<WorkbenchState>>,
    leases: Arc<Mutex<LeaseTable>>,
    tasks: Arc<Mutex<TaskTable>>,
    session_gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    observations: Arc<ObservabilityStore>,
    operator_grants: Arc<Mutex<HashMap<String, OperatorGrant>>>,
    operator_nonces: Arc<Mutex<HashMap<String, OperatorNonce>>>,
    async_active: Arc<AtomicUsize>,
    async_maximum: usize,
    async_resume_tokens: Arc<Mutex<HashMap<String, String>>>,
}

impl Controller {
    pub fn open(store: JsonStore) -> Result<Self, workbench_core::StoreError> {
        Self::open_with_id(store, None)
    }

    pub fn open_with_id(
        store: JsonStore,
        configured_id: Option<String>,
    ) -> Result<Self, workbench_core::StoreError> {
        let mut state = store.load()?;
        let observations =
            ObservabilityStore::open(store.path().with_file_name("observability.db"))
                .map_err(|error| workbench_core::StoreError::Observability(error.to_string()))?;
        let _ = observations.prune(now_ms());
        if let (Some(stored), Some(configured)) = (&state.controller_id, &configured_id)
            && stored != configured
        {
            return Err(workbench_core::StoreError::ControllerIdentityMismatch {
                stored: stored.clone(),
                configured: configured.clone(),
            });
        }
        let id = configured_id
            .or_else(|| state.controller_id.clone())
            .unwrap_or_else(|| format!("controller_{}", Uuid::new_v4().simple()));
        let mut identity_changed = state.controller_id.as_deref() != Some(&id);
        state.controller_id = Some(id.clone());
        for session in &mut state.sessions {
            if session.authority.is_none() {
                session.authority = Some(SessionAuthority {
                    controller_id: id.clone(),
                    epoch: 1,
                    pending_controller_id: None,
                });
                identity_changed = true;
            }
        }
        let compacted = compact_persisted_evidence(&mut state);
        let mut leases =
            LeaseTable::from_snapshot(state.leases.clone(), state.lease_fences.clone());
        let reaped = leases.reap_expired();
        let mut tasks = TaskTable::from_tasks(state.tasks.clone());
        let recovered = tasks.recover_orphans();
        if identity_changed || compacted || !reaped.is_empty() || !recovered.is_empty() {
            state.leases = leases.persistence_snapshot();
            state.lease_fences = leases.fence_snapshot();
            state.tasks = tasks.persistence_snapshot();
            store.save(&state)?;
        }
        Ok(Self {
            id,
            leases: Arc::new(Mutex::new(leases)),
            tasks: Arc::new(Mutex::new(tasks)),
            session_gates: Arc::new(Mutex::new(HashMap::new())),
            state: Arc::new(Mutex::new(state)),
            store,
            observations: Arc::new(observations),
            operator_grants: Arc::new(Mutex::new(HashMap::new())),
            operator_nonces: Arc::new(Mutex::new(HashMap::new())),
            async_active: Arc::new(AtomicUsize::new(0)),
            async_maximum: std::env::var("WORKBENCH_CONTROLLER_ASYNC_CONCURRENCY")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|value: &usize| *value > 0)
                .unwrap_or(32),
            async_resume_tokens: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn handle(&self, request: Request) -> Response {
        let correlation_id = request
            .correlation_id
            .clone()
            .unwrap_or_else(|| request.request_id.clone());
        let previous = CURRENT_TRACE.with(|current| {
            current.replace(Some(TraceContext {
                correlation_id,
                request_id: request.request_id.clone(),
                run_id: request.run_id.clone(),
                span_id: request.span_id.clone(),
                agent_session_id: request.agent_session_id.clone(),
            }))
        });
        let started = Instant::now();
        request_event(
            "info",
            "request.started",
            &request,
            safe_request_fields(&request.action, &request.params),
        );
        let finish_fields = json!({
            "requestId": request.request_id.clone(),
            "correlationId": request.correlation_id.as_deref().unwrap_or(&request.request_id),
            "parentRequestId": request.parent_request_id.clone(),
            "action": request.action.clone(),
        });
        let lifecycle_attributes = safe_lifecycle_attributes(&request.params);
        let session_gate = self
            .requires_session_gate(&request.action)
            .then(|| self.session_id_for_action(&request.action, &request.params))
            .flatten()
            .map(|session_id| self.session_gate(&session_id));
        let _session_guard = session_gate
            .as_ref()
            .map(|gate| gate.lock().expect("session gate"));
        let reaped = self.leases.lock().expect("lease lock").reap_expired();
        let result = if reaped.is_empty() {
            self.dispatch(&request.action, request.params)
        } else {
            self.persist()
                .and_then(|()| self.dispatch(&request.action, request.params))
        };
        let response = match result {
            Ok(value) => Response::success(request.request_id.clone(), value),
            Err(error) => Response::failure(request.request_id.clone(), error),
        };
        event_fields(
            if response.ok { "info" } else { "error" },
            "request.finished",
            json!({"request": finish_fields, "ok": response.ok, "durationMs": started.elapsed().as_millis()}),
        );
        let _ = self.observations.append(&Observation {
            event_id: 0,
            run_id: request.run_id.clone(),
            timestamp: now_ms(),
            node_id: self.id.clone(),
            role: "controller".to_owned(),
            kind: "rpc".to_owned(),
            name: request.action.clone(),
            status: if response.ok { "completed" } else { "failed" }.to_owned(),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            span_id: request.span_id.clone(),
            parent_span_id: request.parent_span_id.clone(),
            request_id: Some(request.request_id.clone()),
            task_id: None,
            process_id: None,
            connection_id: None,
            attributes: response
                .error
                .as_ref()
                .map(|error| json!({"errorCode": error.code}))
                .unwrap_or_else(|| json!({})),
        });
        if let Some(kind) = lifecycle_observation_kind(&request.action) {
            let _ = self.observations.append(&Observation {
                event_id: 0,
                run_id: request.run_id.clone(),
                timestamp: now_ms(),
                node_id: self.id.clone(),
                role: "controller".into(),
                kind: kind.into(),
                name: request.action.clone(),
                status: if response.ok { "completed" } else { "failed" }.into(),
                duration_ms: Some(started.elapsed().as_millis() as u64),
                span_id: request.span_id.clone(),
                parent_span_id: request.parent_span_id.clone(),
                request_id: Some(request.request_id.clone()),
                task_id: lifecycle_attributes
                    .get("taskId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                process_id: lifecycle_attributes
                    .get("processId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                connection_id: None,
                attributes: lifecycle_attributes,
            });
        }
        CURRENT_TRACE.with(|current| {
            current.replace(previous);
        });
        response
    }

    fn dispatch(&self, action: &str, params: Value) -> Result<Value, RpcError> {
        if let Some(routed) = self.route_session_action(action, &params)? {
            return Ok(routed);
        }
        match action {
            "ping" => Ok(json!({
                "controller": {"id": self.id, "status": "ready", "protocolFeatures": protocol_features()},
            })),
            "status" => {
                let state = self.state.lock().expect("state lock");
                let mut controllers = state.controllers.clone();
                let mut executors = state.executors.clone();
                refresh_local_endpoint_health(&mut controllers, &mut executors);
                if params.get("verbose").and_then(Value::as_bool) == Some(true) {
                    return Ok(json!({
                        "controller": {"id": self.id, "status": "ready", "protocolFeatures": protocol_features()},
                        "controllers": controllers,
                        "executors": executors,
                        "leases": self.leases.lock().expect("lease lock").snapshot(),
                        "tasks": self.tasks.lock().expect("task lock").snapshot(),
                    }));
                }
                let controllers = controllers
                    .iter()
                    .map(|controller| {
                        json!({
                            "id": controller.metadata.id,
                            "health": controller.health,
                            "endpoint": controller.endpoint,
                        })
                    })
                    .collect::<Vec<_>>();
                let executors = executors
                    .iter()
                    .map(|executor| {
                        json!({
                            "id": executor.metadata.id,
                            "health": executor.health,
                            "capabilityCount": executor.capabilities.len(),
                            "endpoint": executor.endpoint,
                        })
                    })
                    .collect::<Vec<_>>();
                let tasks = self.tasks.lock().expect("task lock").snapshot();
                let mut task_counts = BTreeMap::new();
                for task in &tasks {
                    *task_counts
                        .entry(format!("{:?}", task.state).to_lowercase())
                        .or_insert(0_u64) += 1;
                }
                let leases = self.leases.lock().expect("lease lock").snapshot();
                Ok(json!({
                    "controller": {"id": self.id, "status": "ready", "protocolFeatures": protocol_features()},
                    "controllers": controllers,
                    "executors": executors,
                    "leases": {"count": leases.len()},
                    "tasks": {"count": tasks.len(), "byState": task_counts},
                }))
            }
            "executor.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").executors,
            )
            .expect("executors serialize")),
            "controller.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").controllers,
            )
            .expect("controllers serialize")),
            "executor.unregister" => {
                let id = required_str(&params, "executorId")?;
                let mut state = self.state.lock().expect("state lock");
                let before = state.executors.len();
                state
                    .executors
                    .retain(|executor| executor.metadata.id != id);
                let removed = state.executors.len() != before;
                drop(state);
                if removed {
                    self.persist()?;
                }
                Ok(json!({"executorId": id, "removed": removed}))
            }
            "controller.unregister" => {
                let id = required_str(&params, "controllerId")?;
                if id == self.id {
                    return Err(RpcError::new(
                        "CONTROLLER_SELF_UNREGISTER",
                        "a Controller cannot unregister itself",
                    ));
                }
                let mut state = self.state.lock().expect("state lock");
                let before = state.controllers.len();
                state
                    .controllers
                    .retain(|controller| controller.metadata.id != id);
                let removed = state.controllers.len() != before;
                drop(state);
                if removed {
                    self.persist()?;
                }
                Ok(json!({"controllerId": id, "removed": removed}))
            }
            "controller.register" => {
                let id = required_str(&params, "controllerId")?.to_owned();
                let endpoint: ExecutorEndpoint = serde_json::from_value(
                    params
                        .get("endpoint")
                        .cloned()
                        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "endpoint is required"))?,
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let response = call_executor(&endpoint, &traced_request("ping", Value::Null))
                    .map_err(|error| {
                        let mut rpc = RpcError::new("CONTROLLER_UNAVAILABLE", error.to_string());
                        rpc.retryable = true;
                        rpc
                    })?;
                if !response.ok {
                    return Err(response.error.unwrap_or_else(|| {
                        RpcError::new("CONTROLLER_FAILED", "controller failed")
                    }));
                }
                let status = response.result.unwrap_or(Value::Null);
                let actual_id = status
                    .get("controller")
                    .and_then(|controller| controller.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        RpcError::new(
                            "CONTROLLER_INVALID",
                            "controller status did not report an identity",
                        )
                    })?;
                if actual_id != id {
                    return Err(RpcError::new(
                        "CONTROLLER_IDENTITY_MISMATCH",
                        format!("endpoint reported Controller {actual_id}, expected {id}"),
                    ));
                }
                let now = now_ms();
                let controller = ControllerPeer {
                    api_version: "workbench.dev/v1".to_owned(),
                    metadata: Metadata {
                        id: id.clone(),
                        labels: Default::default(),
                        created_at: now,
                        updated_at: now,
                    },
                    endpoint,
                    health: HealthStatus::Ready,
                };
                let mut state = self.state.lock().expect("state lock");
                upsert_by(&mut state.controllers, controller.clone(), |existing| {
                    existing.metadata.id == id
                });
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(controller).expect("controller serializes"))
            }
            "controller.call" => {
                let id = required_str(&params, "controllerId")?;
                let controller = self
                    .state
                    .lock()
                    .expect("state lock")
                    .controllers
                    .iter()
                    .find(|controller| controller.metadata.id == id)
                    .cloned()
                    .ok_or_else(|| {
                        RpcError::new("CONTROLLER_NOT_FOUND", format!("unknown controller: {id}"))
                    })?;
                let nested = traced_request(
                    required_str(&params, "action")?,
                    params.get("params").cloned().unwrap_or(Value::Null),
                );
                let response = call_executor(&controller.endpoint, &nested).map_err(|error| {
                    let mut rpc = RpcError::new("CONTROLLER_UNAVAILABLE", error.to_string());
                    rpc.retryable = true;
                    rpc
                })?;
                if response.ok {
                    Ok(response.result.unwrap_or(Value::Null))
                } else {
                    Err(response
                        .error
                        .unwrap_or_else(|| RpcError::new("CONTROLLER_FAILED", "controller failed")))
                }
            }
            "doctor" => {
                let executors = self.state.lock().expect("state lock").executors.clone();
                let checks: Vec<Value> = executors
                    .iter()
                    .map(|executor| {
                        match call_executor(
                            &executor.endpoint,
                            &traced_request("status", Value::Null),
                        ) {
                            Ok(response) if response.ok => json!({
                                "executorId": executor.metadata.id,
                                "status": "ready",
                                "capabilities": executor.capabilities.len()
                            }),
                            Ok(response) => json!({
                                "executorId": executor.metadata.id,
                                "status": "failed",
                                "error": response.error
                            }),
                            Err(error) => json!({
                                "executorId": executor.metadata.id,
                                "status": "offline",
                                "error": error.to_string()
                            }),
                        }
                    })
                    .collect();
                let healthy = checks.iter().all(|check| check["status"] == "ready");
                Ok(json!({
                    "healthy": healthy,
                    "controller": {"id": self.id, "status": "ready"},
                    "checks": checks
                }))
            }
            "dashboard.snapshot" => {
                let state = self.state.lock().expect("state lock").clone();
                let leases = self.leases.lock().expect("lease lock").snapshot();
                let tasks = self.tasks.lock().expect("task lock").snapshot();
                let process_nodes = thread::scope(|scope| {
                    state.executors.iter().map(|executor|scope.spawn(move||{let result=call_executor(&executor.endpoint,&Request::new("process.list",Value::Null));(executor.metadata.id.clone(),result)})).collect::<Vec<_>>().into_iter().map(|handle|handle.join().expect("process list worker")).map(|(executor_id,result)|match result{Ok(response)if response.ok=>json!({"executorId":executor_id,"stale":false,"processes":response.result.and_then(|value|value.get("processes").and_then(Value::as_array).cloned()).unwrap_or_default().iter().map(|item|json!({"id":item.get("id"),"state":item.get("state"),"pid":item.get("pid"),"restartable":item.get("restartable"),"readiness":item.get("readiness"),"startedAt":item.get("startedAt"),"updatedAt":item.get("updatedAt")})).collect::<Vec<_>>() }),_=>json!({"executorId":executor_id,"stale":true,"processes":[]})}).collect::<Vec<_>>()
                });
                let tunnels = thread::scope(|scope| {
                    state
                        .executors
                        .iter()
                        .filter(|executor| {
                            executor
                                .capabilities
                                .iter()
                                .any(|capability| capability.name == "tunnel.list")
                        })
                        .map(|executor| {
                            scope.spawn(move || {
                                let result = call_executor(
                                    &executor.endpoint,
                                    &Request::new("tunnel.list", Value::Null),
                                );
                                (executor.metadata.id.clone(), result)
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .flat_map(|handle| {
                            let (executor_id, result) = handle.join().expect("tunnel list worker");
                            match result {
                                Ok(response) if response.ok => response
                                    .result
                                    .and_then(|value| {
                                        value.get("tunnels").and_then(Value::as_array).cloned()
                                    })
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|mut tunnel| {
                                        tunnel["executorId"] = Value::String(executor_id.clone());
                                        tunnel
                                    })
                                    .collect::<Vec<_>>(),
                                _ => Vec::new(),
                            }
                        })
                        .collect::<Vec<_>>()
                });
                Ok(json!({
                    "generatedAt": now_ms(),
                    "controller": {"id": self.id, "status": "ready"},
                    "controllers": state.controllers.iter().map(|item| json!({"id":item.metadata.id,"health":item.health})).collect::<Vec<_>>(),
                    "sessions": state.sessions.iter().map(|item| json!({"id":item.metadata.id,"objective":item.objective,"state":item.state,"observability":item.observability})).collect::<Vec<_>>(),
                    "executors": state.executors.iter().map(|item| json!({"id":item.metadata.id,"health":item.health,"capabilityCount":item.capabilities.len()})).collect::<Vec<_>>(),
                    "leases": leases.iter().map(|item|json!({"id":item.id,"kind":item.kind,"resource":item.resource,"owner":item.owner,"fence":item.fence,"expiresAt":item.expires_at,"handoffTo":item.handoff_to})).collect::<Vec<_>>(),
                    "ports": leases.iter().filter(|item|item.resource.starts_with("port:")).map(|item|json!({"id":item.id,"resource":item.resource,"owner":item.owner,"expiresAt":item.expires_at})).collect::<Vec<_>>(),
                    "approvals": state.approvals.iter().map(|item|json!({"id":item.id,"owner":item.owner,"state":item.state,"digest":item.digest,"createdAt":item.created_at,"expiresAt":item.expires_at})).collect::<Vec<_>>(),
                    "processNodes":process_nodes,
                    "tunnels": tunnels,
                    "tasks": tasks.iter().map(|item| {
                        let timeout_ms = state.executors.iter()
                            .find(|executor| executor.metadata.id == item.executor_id)
                            .and_then(|executor| executor.capabilities.iter().find(|capability| capability.name == item.capability))
                            .map(|capability| capability.timeout_ms);
                        json!({"id":item.id,"workspaceSessionId":item.workspace_session_id,"executorId":item.executor_id,"capability":item.capability,"state":item.state,"createdAt":item.created_at,"updatedAt":item.updated_at,"timeoutMs":timeout_ms,"expectedBy":timeout_ms.map(|timeout|item.created_at.saturating_add(timeout))})
                    }).collect::<Vec<_>>(),
                    "artifacts": state.artifacts.iter().map(|item| json!({"digest":item.digest,"artifactType":item.artifact_type,"size":item.size,"createdAt":item.created_at})).collect::<Vec<_>>(),
                    "generations": state.generations.iter().map(|item| json!({"id":item.id,"state":item.state})).collect::<Vec<_>>(),
                    "agents": state.agents.iter().map(|item| json!({"id":item.id,"workspaceSessionId":item.workspace_session_id,"state":item.state,"role":item.role,"executorId":item.executor_id})).collect::<Vec<_>>()
                }))
            }
            "run.start" => {
                let run = Run {
                    run_id: params
                        .get("runId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("run_{}", Uuid::new_v4().simple())),
                    workspace_session_id: optional_str(&params, "workspaceSessionId")?,
                    agent_session_id: optional_str(&params, "agentSessionId")?,
                    target_summary: required_str(&params, "targetSummary")?
                        .chars()
                        .take(500)
                        .collect(),
                    created_by: optional_str(&params, "createdBy")?
                        .unwrap_or_else(|| "agent".to_owned()),
                    status: RunStatus::Running,
                    health: RunHealth::Unknown,
                    started_at: now_ms(),
                    finished_at: None,
                    business_outcome: None,
                };
                self.observations
                    .start_run(&run)
                    .map_err(observability_rpc_error)?;
                let _ = self.observations.append(&Observation {
                    event_id: 0,
                    run_id: Some(run.run_id.clone()),
                    timestamp: run.started_at,
                    node_id: self.id.clone(),
                    role: "controller".into(),
                    kind: "run".into(),
                    name: "run.start".into(),
                    status: "running".into(),
                    duration_ms: None,
                    span_id: None,
                    parent_span_id: None,
                    request_id: None,
                    task_id: None,
                    process_id: None,
                    connection_id: None,
                    attributes: json!({"createdBy":run.created_by}),
                });
                Ok(serde_json::to_value(run).expect("run serializes"))
            }
            "run.finish" => {
                let status: RunStatus = serde_json::from_value(
                    params
                        .get("status")
                        .cloned()
                        .unwrap_or_else(|| json!("completed")),
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let run_id = required_str(&params, "runId")?;
                let outcome = optional_str(&params, "businessOutcome")?;
                match self.observations.finish_run(run_id,status,now_ms(),outcome) {
                    Ok(run)=>{
                        let _ = self.observations.append(&Observation {
                            event_id: 0,
                            run_id: Some(run.run_id.clone()),
                            timestamp: run.finished_at.unwrap_or_else(now_ms),
                            node_id: self.id.clone(), role: "controller".into(), kind: "run".into(),
                            name: "run.finish".into(), status: serde_json::to_value(&run.status).ok().and_then(|value|value.as_str().map(str::to_owned)).unwrap_or_else(||"completed".into()),
                            duration_ms: Some(run.finished_at.unwrap_or_else(now_ms).saturating_sub(run.started_at)),
                            span_id: None, parent_span_id: None, request_id: None,
                            task_id: None, process_id: None, connection_id: None,
                            attributes: json!({"health":run.health,"businessOutcome":run.business_outcome}),
                        });
                        Ok(serde_json::to_value(run).expect("run serializes"))
                    },
                    Err(workbench_core::ObservabilityError::RunNotFound(_)) if params.get("localOnly").and_then(Value::as_bool)!=Some(true)=>self.call_peer_until_success("run.finish",json!({"runId":run_id,"status":params.get("status"),"businessOutcome":params.get("businessOutcome"),"localOnly":true})),
                    Err(error)=>Err(observability_rpc_error(error)),
                }
            }
            "run.get" => {
                let run_id = required_str(&params, "runId")?;
                match self
                    .observations
                    .get_run(run_id)
                    .map_err(observability_rpc_error)?
                {
                    Some(run) => Ok(serde_json::to_value(run).unwrap()),
                    None if params.get("localOnly").and_then(Value::as_bool) != Some(true) => self
                        .call_peer_until_success(
                            "run.get",
                            json!({"runId":run_id,"localOnly":true}),
                        ),
                    None => Err(RpcError::new("RUN_NOT_FOUND", "run not found")),
                }
            }
            "run.list" => {
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(100)
                    .clamp(1, 500) as usize;
                let offset = params
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(10_000) as usize;
                let fetch_limit = limit.saturating_add(offset).min(500);
                let status_filter = optional_str(&params, "status")?;
                let mut runs = self
                    .observations
                    .list_runs(fetch_limit)
                    .map_err(observability_rpc_error)?;
                if params.get("localOnly").and_then(Value::as_bool) != Some(true) {
                    let controllers = self.state.lock().expect("state lock").controllers.clone();
                    let results = thread::scope(|scope| {
                        controllers
                            .into_iter()
                            .map(|controller| {
                                scope.spawn(move || {
                                    call_executor(
                                        &controller.endpoint,
                                        &Request::new(
                                            "run.list",
                                            json!({"limit":fetch_limit,"localOnly":true}),
                                        ),
                                    )
                                })
                            })
                            .collect::<Vec<_>>()
                            .into_iter()
                            .map(|handle| handle.join().expect("run list worker"))
                            .collect::<Vec<_>>()
                    });
                    for response in results.into_iter().flatten().filter(|response| response.ok) {
                        if let Some(values) =
                            response.result.and_then(|value| value.as_array().cloned())
                        {
                            for value in values {
                                if let Ok(run) = serde_json::from_value::<Run>(value) {
                                    runs.push(run)
                                }
                            }
                        }
                    }
                    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
                    let mut seen = std::collections::HashSet::new();
                    runs.retain(|run| seen.insert(run.run_id.clone()));
                }
                if let Some(status) = status_filter {
                    runs.retain(|run| {
                        serde_json::to_value(&run.status)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned))
                            .as_deref()
                            == Some(status.as_str())
                    });
                }
                runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
                let runs = runs
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .collect::<Vec<_>>();
                Ok(serde_json::to_value(runs).unwrap())
            }
            "observability.query" => {
                let local_only = params.get("localOnly").and_then(Value::as_bool) == Some(true);
                let requested_cursors = params.get("cursors").and_then(Value::as_object);
                let local_cursor = requested_cursors
                    .and_then(|values| values.get(&self.id))
                    .and_then(Value::as_u64)
                    .or_else(|| params.get("cursor").and_then(Value::as_u64));
                let mut events = self
                    .observations
                    .query(ObservationQuery {
                        run_id: optional_str(&params, "runId")?,
                        after_event_id: local_cursor,
                        limit: params.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize,
                    })
                    .map_err(observability_rpc_error)?;
                let cursor = events
                    .last()
                    .map(|event| event.event_id)
                    .unwrap_or(local_cursor.unwrap_or(0));
                if local_only {
                    return Ok(
                        json!({"events":events,"cursor":cursor,"nodeId":self.id,"stale":false}),
                    );
                }
                let controllers = self.state.lock().expect("state lock").controllers.clone();
                let run_id = optional_str(&params, "runId")?;
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(200)
                    .clamp(1, 1000) as usize;
                let remote_results = thread::scope(|scope| {
                    let handles=controllers.into_iter().map(|controller| {
                        let remote_cursor=requested_cursors.and_then(|values|values.get(&controller.metadata.id)).and_then(Value::as_u64).unwrap_or(0);
                        let remote_run_id=run_id.clone();
                        scope.spawn(move || {
                            let node_id=controller.metadata.id.clone();
                            let request=traced_request("observability.query",json!({"runId":remote_run_id,"cursor":remote_cursor,"limit":limit,"localOnly":true}));
                            let result=call_executor(&controller.endpoint,&request);
                            (node_id,remote_cursor,result)
                        })
                    }).collect::<Vec<_>>();
                    handles
                        .into_iter()
                        .map(|handle| handle.join().expect("observation query worker"))
                        .collect::<Vec<_>>()
                });
                let mut cursors = serde_json::Map::new();
                cursors.insert(self.id.clone(), json!(cursor));
                let mut nodes = vec![json!({"nodeId":self.id,"stale":false,"cursor":cursor})];
                for (node_id, previous_cursor, result) in remote_results {
                    match result {
                        Ok(response) if response.ok => {
                            let value = response.result.unwrap_or(Value::Null);
                            let remote_cursor = value
                                .get("cursor")
                                .and_then(Value::as_u64)
                                .unwrap_or(previous_cursor);
                            cursors.insert(node_id.clone(), json!(remote_cursor));
                            nodes.push(
                                json!({"nodeId":node_id,"stale":false,"cursor":remote_cursor}),
                            );
                            if let Some(remote_events) =
                                value.get("events").and_then(Value::as_array)
                            {
                                for value in remote_events {
                                    if let Ok(event) =
                                        serde_json::from_value::<Observation>(value.clone())
                                    {
                                        events.push(event);
                                    }
                                }
                            }
                        }
                        _ => {
                            cursors.insert(node_id.clone(), json!(previous_cursor));
                            nodes.push(
                                json!({"nodeId":node_id,"stale":true,"cursor":previous_cursor}),
                            );
                        }
                    }
                }
                events.sort_by(|left, right| {
                    left.timestamp
                        .cmp(&right.timestamp)
                        .then_with(|| left.node_id.cmp(&right.node_id))
                        .then_with(|| left.event_id.cmp(&right.event_id))
                });
                let mut seen = std::collections::HashSet::new();
                events.retain(|event| seen.insert((event.node_id.clone(), event.event_id)));
                if events.len() > limit {
                    events = events.split_off(events.len() - limit);
                }
                Ok(json!({"events":events,"cursor":cursor,"cursors":cursors,"nodes":nodes}))
            }
            "observation.append" => {
                let mut observation: Observation = serde_json::from_value(params)
                    .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                observation.event_id = 0;
                observation.attributes = safe_observation_attributes(&observation.attributes);
                let event_id = self
                    .observations
                    .append(&observation)
                    .map_err(observability_rpc_error)?;
                Ok(json!({"eventId":event_id}))
            }
            "operator.grant" => {
                let operator_id = required_str(&params, "operatorId")?.to_owned();
                let token = format!("grant_{}", Uuid::new_v4().simple());
                let expires_at = now_ms()
                    + params
                        .get("ttlMs")
                        .and_then(Value::as_u64)
                        .unwrap_or(300_000)
                        .min(3_600_000);
                self.operator_grants
                    .lock()
                    .expect("operator grants")
                    .insert(
                        operator_id.clone(),
                        OperatorGrant {
                            operator_id: operator_id.clone(),
                            token_digest: sha256_bytes(token.as_bytes()),
                            expires_at,
                        },
                    );
                Ok(json!({"operatorId":operator_id,"grantToken":token,"expiresAt":expires_at}))
            }
            "operator.nonce" => {
                let grant_token = required_str(&params, "grantToken")?;
                let action = required_force_action(&params)?.to_owned();
                let target = required_str(&params, "target")?.to_owned();
                let reason = required_str(&params, "reason")?
                    .chars()
                    .take(500)
                    .collect::<String>();
                if params.get("confirmed").and_then(Value::as_bool) != Some(true) {
                    return Err(RpcError::new(
                        "CONFIRMATION_REQUIRED",
                        "operator action requires explicit confirmation",
                    ));
                }
                let digest = sha256_bytes(grant_token.as_bytes());
                let operator = self
                    .operator_grants
                    .lock()
                    .expect("operator grants")
                    .values()
                    .find(|grant| grant.token_digest == digest && grant.expires_at > now_ms())
                    .cloned()
                    .ok_or_else(|| {
                        RpcError::new(
                            "OPERATOR_GRANT_INVALID",
                            "operator grant is invalid or expired",
                        )
                    })?;
                let nonce = format!("nonce_{}", Uuid::new_v4().simple());
                let expires_at = now_ms() + 60_000;
                self.operator_nonces
                    .lock()
                    .expect("operator nonces")
                    .insert(
                        nonce.clone(),
                        OperatorNonce {
                            operator_id: operator.operator_id,
                            action: action.clone(),
                            target: target.clone(),
                            reason,
                            expires_at,
                        },
                    );
                Ok(
                    json!({"actionNonce":nonce,"action":action,"target":target,"expiresAt":expires_at}),
                )
            }
            "operator.action" => {
                let action = required_operator_action(&params)?.to_owned();
                let target = required_str(&params, "target")?.to_owned();
                if !matches!(action.as_str(), "cancel-task" | "stop-process") {
                    let digest = sha256_bytes(required_str(&params, "grantToken")?.as_bytes());
                    let operator = self
                        .operator_grants
                        .lock()
                        .expect("operator grants")
                        .values()
                        .find(|grant| grant.token_digest == digest && grant.expires_at > now_ms())
                        .cloned()
                        .ok_or_else(|| {
                            RpcError::new(
                                "OPERATOR_GRANT_INVALID",
                                "operator grant is invalid or expired",
                            )
                        })?;
                    let result = match action.as_str() {
                        "retry-task" => self.dispatch("task.retry", json!({"taskId":target,"_operatorAuthorized":true}))?,
                        "restart-process" => self.call_registered_executor(required_str(&params,"executorId")?, "process.restart", json!({"processId":target}))?,
                        "driver-acquire" => self.dispatch("driver.acquire", json!({"resource":target,"owner":operator.operator_id,"ttlMs":params.get("ttlMs").cloned().unwrap_or(json!(300000))}))?,
                        "driver-handoff" => self.dispatch("driver.handoff", json!({"resource":target,"owner":required_str(&params,"owner")?,"token":required_str(&params,"driverToken")?,"target":required_str(&params,"handoffTarget")?}))?,
                        "driver-release" => self.dispatch("driver.release", json!({"resource":target,"owner":required_str(&params,"owner")?,"token":required_str(&params,"driverToken")?}))?,
                        "approval-approve" => self.dispatch("approval.approve", json!({"approvalId":target,"ttlMs":params.get("ttlMs").cloned().unwrap_or(json!(300000))}))?,
                        "approval-revoke" => self.dispatch("approval.revoke", json!({"approvalId":target}))?,
                        _ => unreachable!(),
                    };
                    let _ = self.observations.append(&Observation {
                        event_id: 0,
                        run_id: optional_str(&params, "runId")?,
                        timestamp: now_ms(),
                        node_id: self.id.clone(),
                        role: "operator".into(),
                        kind: "operator-action".into(),
                        name: action,
                        status: "completed".into(),
                        duration_ms: None,
                        span_id: None,
                        parent_span_id: None,
                        request_id: None,
                        task_id: None,
                        process_id: None,
                        connection_id: None,
                        attributes: json!({"operatorId":operator.operator_id,"target":target}),
                    });
                    return Ok(result);
                }
                let nonce_id = required_str(&params, "actionNonce")?;
                let nonce = self
                    .operator_nonces
                    .lock()
                    .expect("operator nonces")
                    .remove(nonce_id)
                    .ok_or_else(|| {
                        RpcError::new(
                            "ACTION_NONCE_INVALID",
                            "action nonce is missing or already consumed",
                        )
                    })?;
                if nonce.expires_at <= now_ms() || nonce.action != action || nonce.target != target
                {
                    return Err(RpcError::new(
                        "ACTION_NONCE_INVALID",
                        "action nonce is expired or does not match target",
                    ));
                }
                let before = match action.as_str() {
                    "cancel-task" => self.dispatch("task.get", json!({"taskId":target}))?,
                    "stop-process" => Value::Null,
                    _ => unreachable!(),
                };
                let result = match action.as_str() {
                    "cancel-task" => self.dispatch(
                        "task.cancel",
                        json!({"taskId":target,"_operatorAuthorized":true}),
                    )?,
                    "stop-process" => self.call_registered_executor(
                        required_str(&params, "executorId")?,
                        "process.stop",
                        json!({"processId":target}),
                    )?,
                    _ => unreachable!(),
                };
                let _=self.observations.append(&Observation{event_id:0,run_id:optional_str(&params,"runId")?,timestamp:now_ms(),node_id:self.id.clone(),role:"operator".into(),kind:"operator-action".into(),name:action,status:"completed".into(),duration_ms:None,span_id:None,parent_span_id:None,request_id:None,task_id:if nonce.action=="cancel-task"{Some(target.clone())}else{None},process_id:if nonce.action=="stop-process"{Some(target.clone())}else{None},connection_id:None,attributes:json!({"action":nonce.action,"reason":nonce.reason,"operatorId":nonce.operator_id,"before":safe_summary(&before),"after":safe_summary(&result)})});
                Ok(result)
            }
            "session.put" => {
                let mut session: WorkspaceSession = serde_json::from_value(params)
                    .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                match &session.authority {
                    None => {
                        session.authority = Some(SessionAuthority {
                            controller_id: self.id.clone(),
                            epoch: 1,
                            pending_controller_id: None,
                        });
                    }
                    Some(authority) if authority.controller_id == self.id => {
                        let existing = self
                            .state
                            .lock()
                            .expect("state lock")
                            .sessions
                            .iter()
                            .find(|existing| existing.metadata.id == session.metadata.id)
                            .and_then(|existing| existing.authority.as_ref())
                            .cloned();
                        if existing.as_ref() != Some(authority) {
                            return Err(RpcError::new(
                                "INVALID_AUTHORITY",
                                "session authority can only change through handoff",
                            ));
                        }
                    }
                    Some(authority) => {
                        return Err(RpcError::new(
                            "NOT_SESSION_AUTHORITY",
                            format!(
                                "session {} is owned by controller {}",
                                session.metadata.id, authority.controller_id
                            ),
                        ));
                    }
                }
                let archived_grants = if session.state == SessionState::Archived {
                    self.state
                        .lock()
                        .expect("state lock")
                        .read_grants
                        .iter()
                        .filter(|grant| {
                            grant.workspace_session_id == session.metadata.id
                                && grant.state == ReadGrantState::Approved
                        })
                        .map(|grant| (grant.id.clone(), grant.executor_id.clone()))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                for (grant_id, executor_id) in &archived_grants {
                    self.call_registered_executor(
                        executor_id,
                        "read-grant.revoke",
                        json!({"grantId": grant_id}),
                    )?;
                }
                let mut state = self.state.lock().expect("state lock");
                upsert_by(&mut state.sessions, session.clone(), |item| {
                    item.metadata.id == session.metadata.id
                });
                for (grant_id, _) in archived_grants {
                    if let Some(grant) = state
                        .read_grants
                        .iter_mut()
                        .find(|grant| grant.id == grant_id)
                    {
                        grant.state = ReadGrantState::Revoked;
                        grant.revoked_at = Some(now_ms());
                    }
                }
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(session).expect("session serializes"))
            }
            "session.accept-handoff" => {
                let session: WorkspaceSession = serde_json::from_value(
                    params
                        .get("session")
                        .cloned()
                        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "session is required"))?,
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let authority = session.authority.as_ref().ok_or_else(|| {
                    RpcError::new("INVALID_AUTHORITY", "handoff has no session authority")
                })?;
                if authority.controller_id != self.id {
                    return Err(RpcError::new(
                        "INVALID_AUTHORITY",
                        "handoff target does not match this controller",
                    ));
                }
                let mut state = self.state.lock().expect("state lock");
                if let Some(existing) = state
                    .sessions
                    .iter()
                    .find(|existing| existing.metadata.id == session.metadata.id)
                {
                    if existing.authority.as_ref() == Some(authority) {
                        return Ok(serde_json::to_value(existing).expect("session serializes"));
                    }
                    if existing
                        .authority
                        .as_ref()
                        .is_some_and(|current| current.epoch >= authority.epoch)
                    {
                        return Err(RpcError::new(
                            "STALE_AUTHORITY_EPOCH",
                            "handoff epoch must increase",
                        ));
                    }
                }
                upsert_by(&mut state.sessions, session.clone(), |existing| {
                    existing.metadata.id == session.metadata.id
                });
                for task in optional_array::<Task>(&params, "tasks")? {
                    let id = task.id.clone();
                    upsert_by(&mut state.tasks, task, |existing| existing.id == id);
                }
                for artifact in optional_array::<Artifact>(&params, "artifacts")? {
                    let digest = artifact.digest.clone();
                    upsert_by(&mut state.artifacts, artifact, |existing| {
                        existing.digest == digest
                    });
                }
                for generation in optional_array::<Generation>(&params, "generations")? {
                    let id = generation.id.clone();
                    upsert_by(&mut state.generations, generation, |existing| {
                        existing.id == id
                    });
                }
                for transaction in optional_array::<ActivationTransaction>(&params, "transactions")?
                {
                    let id = transaction.id.clone();
                    upsert_by(&mut state.transactions, transaction, |existing| {
                        existing.id == id
                    });
                }
                for agent in optional_array::<AgentInstance>(&params, "agents")? {
                    let id = agent.id.clone();
                    upsert_by(&mut state.agents, agent, |existing| existing.id == id);
                }
                for handoff in optional_array::<Handoff>(&params, "handoffs")? {
                    let id = handoff.id.clone();
                    upsert_by(&mut state.handoffs, handoff, |existing| existing.id == id);
                }
                *self.tasks.lock().expect("task lock") = TaskTable::from_tasks(state.tasks.clone());
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(session).expect("session serializes"))
            }
            "session.handoff" => {
                let session_id = required_str(&params, "sessionId")?;
                let target_id = required_str(&params, "targetControllerId")?;
                let target = self
                    .state
                    .lock()
                    .expect("state lock")
                    .controllers
                    .iter()
                    .find(|controller| controller.metadata.id == target_id)
                    .cloned()
                    .ok_or_else(|| {
                        RpcError::new("CONTROLLER_NOT_FOUND", "target not registered")
                    })?;
                if self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .snapshot()
                    .iter()
                    .any(|task| {
                        task.workspace_session_id == session_id
                            && !matches!(
                                task.state,
                                TaskState::Succeeded
                                    | TaskState::Failed
                                    | TaskState::Cancelled
                                    | TaskState::TimedOut
                                    | TaskState::OutcomeUnknown
                            )
                    })
                {
                    return Err(RpcError::new(
                        "SESSION_NOT_QUIESCENT",
                        "session has queued or running tasks",
                    ));
                }
                let mut session = self
                    .state
                    .lock()
                    .expect("state lock")
                    .sessions
                    .iter()
                    .find(|session| session.metadata.id == session_id)
                    .cloned()
                    .ok_or_else(|| RpcError::new("SESSION_NOT_FOUND", "session not found"))?;
                let current = session.authority.clone().ok_or_else(|| {
                    RpcError::new("INVALID_AUTHORITY", "session has no authority")
                })?;
                if current.controller_id != self.id {
                    return Err(RpcError::new(
                        "NOT_SESSION_AUTHORITY",
                        format!("session is owned by {}", current.controller_id),
                    ));
                }
                if current
                    .pending_controller_id
                    .as_deref()
                    .is_some_and(|pending| pending != target_id)
                {
                    return Err(RpcError::new(
                        "HANDOFF_IN_PROGRESS",
                        format!(
                            "handoff is already pending to {:?}",
                            current.pending_controller_id
                        ),
                    ));
                }
                if current.pending_controller_id.is_none() {
                    session.authority = Some(SessionAuthority {
                        controller_id: self.id.clone(),
                        epoch: current.epoch,
                        pending_controller_id: Some(target_id.to_owned()),
                    });
                    let mut state = self.state.lock().expect("state lock");
                    upsert_by(&mut state.sessions, session.clone(), |existing| {
                        existing.metadata.id == session.metadata.id
                    });
                    drop(state);
                    self.persist()?;
                }
                session.authority = Some(SessionAuthority {
                    controller_id: target_id.to_owned(),
                    epoch: current.epoch.saturating_add(1),
                    pending_controller_id: None,
                });
                session.metadata.updated_at = now_ms();
                let bundle = self.session_bundle(&session);
                let response = call_executor(
                    &target.endpoint,
                    &traced_request("session.accept-handoff", bundle),
                )
                .map_err(|error| RpcError::new("CONTROLLER_UNAVAILABLE", error.to_string()))?;
                if !response.ok {
                    return Err(response.error.unwrap_or_else(|| {
                        RpcError::new("HANDOFF_REJECTED", "target rejected session handoff")
                    }));
                }
                let mut state = self.state.lock().expect("state lock");
                upsert_by(&mut state.sessions, session.clone(), |existing| {
                    existing.metadata.id == session.metadata.id
                });
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(session).expect("session serializes"))
            }
            "session.get" => {
                let id = required_str(&params, "sessionId")?;
                let state = self.state.lock().expect("state lock");
                let value = state
                    .sessions
                    .iter()
                    .find(|item| item.metadata.id == id)
                    .ok_or_else(|| {
                        RpcError::new("SESSION_NOT_FOUND", format!("unknown session: {id}"))
                    })?;
                Ok(serde_json::to_value(value).expect("session serializes"))
            }
            "session.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").sessions,
            )
            .expect("sessions serialize")),
            "session.transition" => {
                let id = required_str(&params, "sessionId")?;
                let session_state: SessionState = serde_json::from_value(
                    params
                        .get("state")
                        .cloned()
                        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "state is required"))?,
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let mut state = self.state.lock().expect("state lock");
                let session = state
                    .sessions
                    .iter_mut()
                    .find(|item| item.metadata.id == id)
                    .ok_or_else(|| {
                        RpcError::new("SESSION_NOT_FOUND", format!("unknown session: {id}"))
                    })?;
                session.state = session_state;
                session.metadata.updated_at = now_ms();
                let result = session.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(result).expect("session serializes"))
            }
            "artifact.put" => {
                let artifact: Artifact = serde_json::from_value(params)
                    .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let mut state = self.state.lock().expect("state lock");
                upsert_by(&mut state.artifacts, artifact.clone(), |item| {
                    item.digest == artifact.digest
                });
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(artifact).expect("artifact serializes"))
            }
            "artifact.get" => {
                let digest = required_str(&params, "digest")?;
                let state = self.state.lock().expect("state lock");
                let value = state
                    .artifacts
                    .iter()
                    .find(|item| item.digest == digest)
                    .ok_or_else(|| {
                        RpcError::new("ARTIFACT_NOT_FOUND", format!("unknown artifact: {digest}"))
                    })?;
                Ok(serde_json::to_value(value).expect("artifact serializes"))
            }
            "artifact.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").artifacts,
            )
            .expect("artifacts serialize")),
            "artifact.transfer" => self.relay_artifact(&params),
            "generation.put" => {
                let generation: Generation = serde_json::from_value(params)
                    .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let mut state = self.state.lock().expect("state lock");
                if let Some(existing) = state
                    .generations
                    .iter()
                    .find(|item| item.id == generation.id)
                {
                    if existing.workspace_session_id != generation.workspace_session_id
                        || existing.root != generation.root
                        || existing.baseline.digest != generation.baseline.digest
                    {
                        return Err(RpcError::new(
                            "GENERATION_IDENTITY_CONFLICT",
                            "generation identity fields are immutable",
                        ));
                    }
                    if !generation_transition_allowed(&existing.state, &generation.state) {
                        return Err(RpcError::new(
                            "INVALID_GENERATION_STATE",
                            format!(
                                "invalid generation transition {:?} -> {:?}",
                                existing.state, generation.state
                            ),
                        ));
                    }
                }
                upsert_by(&mut state.generations, generation.clone(), |item| {
                    item.id == generation.id
                });
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(generation).expect("generation serializes"))
            }
            "generation.get" => {
                let id = required_str(&params, "generationId")?;
                let state = self.state.lock().expect("state lock");
                let value = state
                    .generations
                    .iter()
                    .find(|item| item.id == id)
                    .ok_or_else(|| {
                        RpcError::new("GENERATION_NOT_FOUND", format!("unknown generation: {id}"))
                    })?;
                Ok(serde_json::to_value(value).expect("generation serializes"))
            }
            "generation.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").generations,
            )
            .expect("generations serialize")),
            "agent.put" => {
                let agent: AgentInstance = serde_json::from_value(params)
                    .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let mut state = self.state.lock().expect("state lock");
                upsert_by(&mut state.agents, agent.clone(), |item| item.id == agent.id);
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(agent).expect("agent serializes"))
            }
            "agent.get" => {
                let id = required_str(&params, "agentId")?;
                let state = self.state.lock().expect("state lock");
                let value = state
                    .agents
                    .iter()
                    .find(|item| item.id == id)
                    .ok_or_else(|| {
                        RpcError::new("AGENT_NOT_FOUND", format!("unknown agent: {id}"))
                    })?;
                Ok(serde_json::to_value(value).expect("agent serializes"))
            }
            "agent.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").agents,
            )
            .expect("agents serialize")),
            "agent.transition" => {
                let id = required_str(&params, "agentId")?;
                let agent_state: AgentState = serde_json::from_value(
                    params
                        .get("state")
                        .cloned()
                        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "state is required"))?,
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let mut state = self.state.lock().expect("state lock");
                let agent = state
                    .agents
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| {
                        RpcError::new("AGENT_NOT_FOUND", format!("unknown agent: {id}"))
                    })?;
                agent.state = agent_state;
                let result = agent.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(result).expect("agent serializes"))
            }
            "executor.put" => {
                let executor: Executor = serde_json::from_value(params)
                    .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let mut state = self.state.lock().expect("state lock");
                if let Some(existing) = state
                    .executors
                    .iter_mut()
                    .find(|existing| existing.metadata.id == executor.metadata.id)
                {
                    *existing = executor.clone();
                } else {
                    state.executors.push(executor.clone());
                }
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(executor).expect("executor serializes"))
            }
            "executor.register" => {
                let id = required_str(&params, "executorId")?.to_owned();
                let endpoint: ExecutorEndpoint = serde_json::from_value(
                    params
                        .get("endpoint")
                        .cloned()
                        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "endpoint is required"))?,
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let response = call_executor(&endpoint, &traced_request("status", Value::Null))
                    .map_err(|error| {
                        let mut rpc = RpcError::new("EXECUTOR_UNAVAILABLE", error.to_string());
                        rpc.retryable = true;
                        rpc
                    })?;
                if !response.ok {
                    return Err(response
                        .error
                        .unwrap_or_else(|| RpcError::new("EXECUTOR_FAILED", "executor failed")));
                }
                let status = response.result.unwrap_or(Value::Null);
                let actual_id = status
                    .get("executorId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        RpcError::new(
                            "EXECUTOR_INVALID",
                            "executor status did not report an identity",
                        )
                    })?;
                if actual_id != id {
                    return Err(RpcError::new(
                        "EXECUTOR_IDENTITY_MISMATCH",
                        format!("endpoint reported Executor {actual_id}, expected {id}"),
                    ));
                }
                let capabilities: Vec<CapabilityDescriptor> = serde_json::from_value(
                    status
                        .get("capabilities")
                        .cloned()
                        .unwrap_or_else(|| json!([])),
                )
                .map_err(|error| RpcError::new("EXECUTOR_INVALID", error.to_string()))?;
                let allowed_roots: Vec<String> = serde_json::from_value(
                    status
                        .get("allowedRoots")
                        .cloned()
                        .unwrap_or_else(|| json!([])),
                )
                .map_err(|error| RpcError::new("EXECUTOR_INVALID", error.to_string()))?;
                let now = now_ms();
                let executor = Executor {
                    api_version: "workbench.dev/v1".to_owned(),
                    metadata: Metadata {
                        id: id.clone(),
                        labels: Default::default(),
                        created_at: now,
                        updated_at: now,
                    },
                    endpoint,
                    capabilities,
                    allowed_roots,
                    health: HealthStatus::Ready,
                };
                let mut state = self.state.lock().expect("state lock");
                upsert_by(&mut state.executors, executor.clone(), |existing| {
                    existing.metadata.id == id
                });
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(executor).expect("executor serializes"))
            }
            "executor.remove" => {
                let id = required_str(&params, "executorId")?;
                let mut state = self.state.lock().expect("state lock");
                let before = state.executors.len();
                state
                    .executors
                    .retain(|executor| executor.metadata.id != id);
                if state.executors.len() == before {
                    return Err(RpcError::new(
                        "EXECUTOR_NOT_FOUND",
                        format!("unknown executor: {id}"),
                    ));
                }
                drop(state);
                self.persist()?;
                Ok(json!({"executorId": id, "removed": true}))
            }
            "executor.call" => {
                let id = required_str(&params, "executorId")?;
                let executor = self
                    .state
                    .lock()
                    .expect("state lock")
                    .executors
                    .iter()
                    .find(|executor| executor.metadata.id == id)
                    .cloned()
                    .ok_or_else(|| {
                        RpcError::new("EXECUTOR_NOT_FOUND", format!("unknown executor: {id}"))
                    })?;
                let nested_action = required_str(&params, "action")?;
                let mut nested_params = params.get("params").cloned().unwrap_or(Value::Null);
                let lease_params = params
                    .get("leases")
                    .and_then(Value::as_array)
                    .cloned()
                    .or_else(|| {
                        params.get("leaseResource").map(|resource| {
                            vec![json!({
                                "resource": resource,
                                "owner": params.get("owner"),
                                "token": params.get("token"),
                            })]
                        })
                    });
                if let Some(lease_params) = lease_params {
                    let leases = self.leases.lock().expect("lease lock");
                    let authorities = lease_params
                        .iter()
                        .map(|item| {
                            let lease = leases
                                .validate(
                                    required_str(item, "resource")?,
                                    required_str(item, "owner")?,
                                    required_str(item, "token")?,
                                )
                                .map_err(map_lease_error)?;
                            Ok(json!({
                                "controllerId": self.id,
                                "resource": lease.resource,
                                "fence": lease.fence,
                            }))
                        })
                        .collect::<Result<Vec<_>, RpcError>>()?;
                    nested_params["_authority"] = Value::Array(authorities);
                }
                let nested = traced_request(nested_action, nested_params);
                let response = call_executor(&executor.endpoint, &nested).map_err(|error| {
                    let mut rpc = RpcError::new("EXECUTOR_UNAVAILABLE", error.to_string());
                    rpc.retryable = true;
                    rpc
                })?;
                if response.ok {
                    Ok(response.result.unwrap_or(Value::Null))
                } else {
                    Err(response
                        .error
                        .unwrap_or_else(|| RpcError::new("EXECUTOR_FAILED", "executor failed")))
                }
            }
            "approval.request" => {
                let digest = required_str(&params, "digest")?.to_owned();
                let owner = required_str(&params, "owner")?.to_owned();
                let mut state = self.state.lock().expect("state lock");
                if let Some(existing) = state.approvals.iter().find(|approval| {
                    approval.digest == digest
                        && approval.owner == owner
                        && matches!(
                            approval.state,
                            ApprovalState::Pending | ApprovalState::Approved
                        )
                }) {
                    return Ok(serde_json::to_value(existing).expect("approval serializes"));
                }
                let approval = Approval {
                    id: format!("approval_{}", Uuid::new_v4().simple()),
                    digest,
                    owner,
                    reason: required_str(&params, "reason")?.to_owned(),
                    state: ApprovalState::Pending,
                    created_at: now_ms(),
                    approved_at: None,
                    consumed_at: None,
                    expires_at: None,
                };
                state.approvals.push(approval.clone());
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(approval).expect("approval serializes"))
            }
            "approval.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").approvals,
            )
            .expect("approvals serialize")),
            "approval.approve" | "approval.revoke" => {
                let id = required_str(&params, "approvalId")?;
                let mut state = self.state.lock().expect("state lock");
                let approval = state
                    .approvals
                    .iter_mut()
                    .find(|approval| approval.id == id)
                    .ok_or_else(|| {
                        RpcError::new("APPROVAL_NOT_FOUND", format!("unknown approval: {id}"))
                    })?;
                if action == "approval.approve" {
                    if !matches!(approval.state, ApprovalState::Pending) {
                        return Err(RpcError::new(
                            "APPROVAL_NOT_PENDING",
                            "approval is not pending",
                        ));
                    }
                    let now = now_ms();
                    approval.state = ApprovalState::Approved;
                    approval.approved_at = Some(now);
                    approval.expires_at = Some(
                        now.saturating_add(
                            params
                                .get("ttlMs")
                                .and_then(Value::as_u64)
                                .unwrap_or(300_000),
                        ),
                    );
                } else {
                    approval.state = ApprovalState::Revoked;
                }
                let result = approval.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(result).expect("approval serializes"))
            }
            "read-grant.request" => {
                let workspace = required_str(&params, "workspaceSessionId")?;
                let executor = required_str(&params, "executorId")?;
                let requested_root = required_str(&params, "requestedRoot")?;
                let resolved = self.call_registered_executor(
                    executor,
                    "read-grant.resolve",
                    json!({"requestedRoot": requested_root}),
                )?;
                let real_root = required_str(&resolved, "realRoot")?.to_owned();
                let now = now_ms();
                let grant = ReadGrant {
                    id: format!("read_grant_{}", Uuid::new_v4().simple()),
                    workspace_session_id: workspace.to_owned(),
                    executor_id: executor.to_owned(),
                    requested_root: requested_root.to_owned(),
                    real_root,
                    capabilities: vec![
                        "filesystem.resolve",
                        "filesystem.stat",
                        "filesystem.list",
                        "filesystem.read",
                        "filesystem.search",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                    state: ReadGrantState::Requested,
                    requested_by: required_str(&params, "requestedBy")?.to_owned(),
                    created_at: now,
                    approved_at: None,
                    revoked_at: None,
                    approved_by: None,
                    audit: params.get("audit").cloned().unwrap_or_else(|| json!({})),
                };
                self.state
                    .lock()
                    .expect("state lock")
                    .read_grants
                    .push(grant.clone());
                self.persist()?;
                Ok(serde_json::to_value(grant).expect("grant serializes"))
            }
            "read-grant.approve" => {
                let id = required_str(&params, "grantId")?;
                let approver = required_str(&params, "approvedBy")?;
                let mut grant = {
                    let state = self.state.lock().expect("state lock");
                    state
                        .read_grants
                        .iter()
                        .find(|grant| grant.id == id)
                        .cloned()
                        .ok_or_else(|| {
                            RpcError::new(
                                "READ_GRANT_NOT_FOUND",
                                format!("unknown read grant: {id}"),
                            )
                        })?
                };
                if grant.state != ReadGrantState::Requested {
                    return Err(RpcError::new(
                        "INVALID_READ_GRANT_STATE",
                        "only requested grants can be approved",
                    ));
                }
                grant.state = ReadGrantState::Approved;
                grant.approved_at = Some(now_ms());
                grant.approved_by = Some(approver.to_owned());
                self.call_registered_executor(
                    &grant.executor_id,
                    "read-grant.approve",
                    serde_json::to_value(&grant).expect("grant serializes"),
                )?;
                let mut state = self.state.lock().expect("state lock");
                let stored = state
                    .read_grants
                    .iter_mut()
                    .find(|item| item.id == id)
                    .expect("grant exists");
                *stored = grant.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(grant).expect("grant serializes"))
            }
            "read-grant.list" => {
                let state = self.state.lock().expect("state lock");
                let workspace = params.get("workspaceSessionId").and_then(Value::as_str);
                let executor = params.get("executorId").and_then(Value::as_str);
                let grants: Vec<_> = state
                    .read_grants
                    .iter()
                    .filter(|grant| {
                        workspace.is_none_or(|value| value == grant.workspace_session_id)
                            && executor.is_none_or(|value| value == grant.executor_id)
                    })
                    .collect();
                Ok(serde_json::to_value(grants).expect("grants serialize"))
            }
            "read-grant.revoke" => {
                let id = required_str(&params, "grantId")?;
                let executor = {
                    let state = self.state.lock().expect("state lock");
                    state
                        .read_grants
                        .iter()
                        .find(|grant| grant.id == id)
                        .map(|grant| grant.executor_id.clone())
                        .ok_or_else(|| {
                            RpcError::new(
                                "READ_GRANT_NOT_FOUND",
                                format!("unknown read grant: {id}"),
                            )
                        })?
                };
                self.call_registered_executor(
                    &executor,
                    "read-grant.revoke",
                    json!({"grantId": id}),
                )?;
                let mut state = self.state.lock().expect("state lock");
                let grant = state
                    .read_grants
                    .iter_mut()
                    .find(|grant| grant.id == id)
                    .expect("grant exists");
                grant.state = ReadGrantState::Revoked;
                grant.revoked_at = Some(now_ms());
                let result = grant.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(result).expect("grant serializes"))
            }
            "capability.invoke" => {
                let executor_id = required_str(&params, "executorId")?;
                let capability_name = required_str(&params, "capability")?;
                let workspace_session_id = required_str(&params, "workspaceSessionId")?;
                let session_authority = self
                    .state
                    .lock()
                    .expect("state lock")
                    .sessions
                    .iter()
                    .find(|session| session.metadata.id == workspace_session_id)
                    .and_then(|session| session.authority.clone())
                    .ok_or_else(|| {
                        RpcError::new(
                            "SESSION_NOT_FOUND",
                            format!("unknown session: {workspace_session_id}"),
                        )
                    })?;
                if session_authority.controller_id != self.id {
                    return Err(RpcError::new(
                        "NOT_SESSION_AUTHORITY",
                        format!("session is owned by {}", session_authority.controller_id),
                    ));
                }
                let owner = required_str(&params, "owner")?;
                let idempotency_key = required_str(&params, "idempotencyKey")?;
                let mut input = params.get("input").cloned().unwrap_or_else(|| json!({}));
                let executor = self
                    .state
                    .lock()
                    .expect("state lock")
                    .executors
                    .iter()
                    .find(|executor| executor.metadata.id == executor_id)
                    .cloned()
                    .ok_or_else(|| {
                        RpcError::new(
                            "EXECUTOR_NOT_FOUND",
                            format!("unknown executor: {executor_id}"),
                        )
                    })?;
                let contract = executor
                    .capabilities
                    .iter()
                    .find(|capability| capability.name == capability_name)
                    .cloned()
                    .ok_or_else(|| {
                        RpcError::new(
                            "CAPABILITY_NOT_FOUND",
                            format!("executor does not provide {capability_name}"),
                        )
                    })?;
                if contract.version.split('.').next() != Some("2") {
                    return Err(RpcError::new(
                        "PEER_CAPABILITY_VERSION_MISMATCH",
                        format!(
                            "{executor_id}/{capability_name} does not implement authority protocol v2"
                        ),
                    ));
                }
                if matches!(
                    capability_name,
                    "filesystem.resolve"
                        | "filesystem.stat"
                        | "filesystem.read"
                        | "filesystem.list"
                        | "filesystem.search"
                ) {
                    input["_workspaceSessionId"] = Value::String(workspace_session_id.to_owned());
                    if let Some(path) = input.get("path").and_then(Value::as_str) {
                        let state = self.state.lock().expect("state lock");
                        if let Some(grant) = state.read_grants.iter().find(|grant| {
                            grant.workspace_session_id == workspace_session_id
                                && grant.executor_id == executor_id
                                && grant.state == ReadGrantState::Approved
                                && grant
                                    .capabilities
                                    .iter()
                                    .any(|item| item == capability_name)
                                && std::path::Path::new(path)
                                    .starts_with(std::path::Path::new(&grant.real_root))
                        }) {
                            input["_readGrantId"] = Value::String(grant.id.clone());
                        }
                    }
                }
                if matches!(
                    capability_name,
                    "application.inspect" | "application.materialize"
                ) {
                    input["_workspaceSessionId"] = Value::String(workspace_session_id.to_owned());
                    let path_key = if capability_name == "application.inspect" {
                        "applicationPath"
                    } else {
                        "baselinePath"
                    };
                    if let Some(path) = input.get(path_key).and_then(Value::as_str) {
                        let state = self.state.lock().expect("state lock");
                        if let Some(grant) = state.read_grants.iter().find(|grant| {
                            grant.workspace_session_id == workspace_session_id
                                && grant.executor_id == executor_id
                                && grant.state == ReadGrantState::Approved
                                && grant
                                    .capabilities
                                    .iter()
                                    .any(|item| item == "filesystem.read")
                                && std::path::Path::new(path)
                                    .starts_with(std::path::Path::new(&grant.real_root))
                        }) {
                            input["_readGrantId"] = Value::String(grant.id.clone());
                        }
                    }
                }
                let invocation_is_readonly = matches!(
                    contract.authority,
                    workbench_schema::CapabilityAuthority::None
                );
                if matches!(
                    contract.authority,
                    workbench_schema::CapabilityAuthority::WorkspaceDriver
                ) && self.workspace_is_draining(workspace_session_id, owner)
                {
                    return Err(RpcError::new(
                        "DRIVER_DRAINING",
                        "workspace driver is draining for an automatic handoff; new mutations are disabled",
                    ));
                }
                if capability_name == "agent.start" {
                    let role = input
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("agent")
                        .to_owned();
                    let agent_id = input
                        .get("agentId")
                        .and_then(Value::as_str)
                        .unwrap_or(owner)
                        .to_owned();
                    if !input.get("env").is_some_and(Value::is_object) {
                        input["env"] = json!({});
                    }
                    let env = input["env"].as_object_mut().expect("agent env is object");
                    env.insert(
                        "WORKBENCH_WORKSPACE_SESSION_ID".into(),
                        json!(workspace_session_id),
                    );
                    env.insert("WORKBENCH_NODE_ID".into(), json!(executor_id));
                    env.insert("WORKBENCH_AGENT_ROLE".into(), json!(role.clone()));
                    env.insert("WORKBENCH_AGENT_ID".into(), json!(agent_id));
                    env.insert(
                        "WORKBENCH_RUN_SUMMARY".into(),
                        json!(format!("{role} task in {workspace_session_id}")),
                    );
                }
                let required_authority = match &contract.authority {
                    workbench_schema::CapabilityAuthority::None => None,
                    workbench_schema::CapabilityAuthority::WorkspaceDriver => {
                        // `LeaseTable::validate` returns a reference into the table. Keep the
                        // mutex guard in an explicit scope so it is released before task
                        // persistence reacquires the lease lock. A chained temporary here can
                        // otherwise live through the rest of this match arm and deadlock every
                        // mutating capability before `executor.dispatch`.
                        {
                            let leases = self.leases.lock().expect("lease lock");
                            Some(
                                leases
                                    .validate(
                                        &format!("workspace:{workspace_session_id}"),
                                        owner,
                                        required_str(&params, "driverToken")?,
                                    )
                                    .map_err(map_lease_error)?
                                    .clone(),
                            )
                        }
                    }
                    workbench_schema::CapabilityAuthority::ResourceLease { resource } => {
                        let resource =
                            render_capability_authority_resource(resource, executor_id, &input)?;
                        let leases = self.leases.lock().expect("lease lock");
                        Some(
                            leases
                                .validate(
                                    &resource,
                                    owner,
                                    required_str(&params, "authorityLeaseToken")?,
                                )
                                .map_err(map_lease_error)?
                                .clone(),
                        )
                    }
                };
                validate_schema(&contract.input_schema, &input, "input")?;
                let execution_mode = params
                    .get("executionMode")
                    .and_then(Value::as_str)
                    .unwrap_or("auto");
                if !matches!(execution_mode, "auto" | "sync" | "async") {
                    return Err(RpcError::new(
                        "INVALID_PARAMS",
                        "executionMode must be auto, sync, or async",
                    ));
                }
                let resume_task_id = params.get("_resumeTaskId").and_then(Value::as_str);
                let resume_authorized = resume_task_id.is_some_and(|task_id| {
                    let supplied = params.get("_resumeToken").and_then(Value::as_str);
                    let mut tokens = self
                        .async_resume_tokens
                        .lock()
                        .expect("async resume tokens lock");
                    let matches =
                        supplied.is_some() && supplied == tokens.get(task_id).map(String::as_str);
                    if matches {
                        tokens.remove(task_id);
                    }
                    matches
                });
                let default_async = contract.execution_kind == ExecutionKind::Background;
                if resume_task_id.is_none()
                    && (execution_mode == "async" || (execution_mode == "auto" && default_async))
                {
                    if self
                        .async_active
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                            (active < self.async_maximum).then_some(active + 1)
                        })
                        .is_err()
                    {
                        let mut error = RpcError::new(
                            "TASK_QUEUE_FULL",
                            format!("controller async task limit {} reached", self.async_maximum),
                        );
                        error.retryable = true;
                        return Err(error);
                    }
                    let (correlation_id, request_id) = current_trace();
                    let (task, reused) = self.tasks.lock().expect("task lock").submit_traced(
                        workspace_session_id,
                        executor_id,
                        capability_name,
                        input.clone(),
                        idempotency_key,
                        correlation_id.clone(),
                        request_id.clone(),
                    );
                    if reused {
                        self.async_active.fetch_sub(1, Ordering::AcqRel);
                        return Ok(
                            json!({"task": task, "reused": true, "accepted": !task.state.terminal()}),
                        );
                    }
                    if let Err(error) = self.persist() {
                        self.async_active.fetch_sub(1, Ordering::AcqRel);
                        return Err(error);
                    }
                    self.observe_task_state(&task);
                    let mut worker_params = params.clone();
                    worker_params["executionMode"] = json!("sync");
                    worker_params["_resumeTaskId"] = json!(task.id.clone());
                    let resume_token = format!("resume_{}", Uuid::new_v4().simple());
                    self.async_resume_tokens
                        .lock()
                        .expect("async resume tokens lock")
                        .insert(task.id.clone(), resume_token.clone());
                    worker_params["_resumeToken"] = json!(resume_token);
                    let controller = self.clone();
                    let async_active = Arc::clone(&self.async_active);
                    let worker_task_id = task.id.clone();
                    thread::spawn(move || {
                        task_event(&worker_task_id, "info", "task.worker.started", json!({}));
                        event_fields(
                            "info",
                            "task.worker.started",
                            json!({"taskId": worker_task_id}),
                        );
                        let mut request = Request::new("capability.invoke", worker_params);
                        request.correlation_id = correlation_id;
                        request.parent_request_id = request_id;
                        let response = controller.handle(request);
                        task_event(
                            &worker_task_id,
                            if response.ok { "info" } else { "error" },
                            "task.worker.finished",
                            json!({"ok": response.ok, "errorCode": response.error.as_ref().map(|error| error.code.as_str())}),
                        );
                        event_fields(
                            if response.ok { "info" } else { "error" },
                            "task.worker.finished",
                            json!({"taskId": worker_task_id, "ok": response.ok, "errorCode": response.error.as_ref().map(|error| error.code.as_str())}),
                        );
                        async_active.fetch_sub(1, Ordering::AcqRel);
                    });
                    return Ok(json!({"task": task, "reused": false, "accepted": true}));
                }
                if matches!(
                    capability_name,
                    "process.start" | "command.run" | "artifact.build" | "agent.start"
                ) && command_requires_approval(&input)
                {
                    let digest = command_digest(&input)?;
                    let approval_id = required_str(&params, "approvalId")?;
                    let mut state = self.state.lock().expect("state lock");
                    let approval = state
                        .approvals
                        .iter_mut()
                        .find(|approval| approval.id == approval_id)
                        .ok_or_else(|| RpcError::new("APPROVAL_NOT_FOUND", "approval not found"))?;
                    if approval.digest != digest
                        || approval.owner != owner
                        || !matches!(approval.state, ApprovalState::Approved)
                        || approval
                            .expires_at
                            .is_none_or(|expires| expires <= now_ms())
                    {
                        return Err(RpcError::new(
                            "APPROVAL_INVALID",
                            "approval does not authorize this exact command",
                        ));
                    }
                    approval.state = ApprovalState::Consumed;
                    approval.consumed_at = Some(now_ms());
                    input["approvalDigest"] = Value::String(digest);
                    drop(state);
                    self.persist()?;
                }
                let (correlation_id, request_id) = current_trace();
                let (task, reused) = self.tasks.lock().expect("task lock").submit_traced(
                    workspace_session_id,
                    executor_id,
                    capability_name,
                    input.clone(),
                    idempotency_key,
                    correlation_id,
                    request_id,
                );
                let task = if reused {
                    match task.state {
                        TaskState::Succeeded => {
                            return Ok(
                                json!({"task": task, "reused": true, "result": task.output}),
                            );
                        }
                        TaskState::Failed | TaskState::TimedOut | TaskState::OutcomeUnknown
                            if task.attempt < contract.retry.max_attempts =>
                        {
                            self.tasks
                                .lock()
                                .expect("task lock")
                                .retry(&task.id)
                                .map_err(|error| {
                                    RpcError::new("INVALID_TASK_STATE", error.to_string())
                                })?
                        }
                        TaskState::Queued
                            if resume_task_id == Some(task.id.as_str()) && resume_authorized =>
                        {
                            task
                        }
                        TaskState::Queued | TaskState::Running => {
                            return Err(RpcError::new(
                                "TASK_IN_PROGRESS",
                                format!("task {} is already in progress", task.id),
                            ));
                        }
                        _ => {
                            return Err(RpcError::new(
                                "TASK_RETRY_EXHAUSTED",
                                format!("task {} exhausted retry policy", task.id),
                            ));
                        }
                    }
                } else {
                    task
                };
                let task = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .transition(&task.id, TaskState::Running, None, None)
                    .map_err(|error| RpcError::new("INVALID_TASK_STATE", error.to_string()))?;
                let mut resources: Vec<String> = contract
                    .locks
                    .iter()
                    .map(|lock| render_lock(&lock.key, &input))
                    .collect::<Result<_, _>>()?;
                if capability_name == "command.run" && !invocation_is_readonly {
                    resources.extend(command_resources(&input)?);
                }
                let mut acquired = Vec::new();
                for resource in &resources {
                    // Drop the acquisition guard before handling a conflict. The error
                    // branch releases resources already acquired for this invocation and
                    // therefore must be able to lock the same lease table again.
                    let acquisition = {
                        let mut leases = self.leases.lock().expect("lease lock");
                        leases.acquire(
                            LeaseKind::Resource,
                            resource,
                            owner,
                            contract.timeout_ms.saturating_add(30_000),
                        )
                    };
                    match acquisition {
                        Ok(lease) => acquired.push(lease),
                        Err(error) => {
                            release_acquired(
                                &mut self.leases.lock().expect("lease lock"),
                                &acquired,
                            );
                            let error = map_lease_error(error);
                            let _ = self.tasks.lock().expect("task lock").transition(
                                &task.id,
                                TaskState::Failed,
                                None,
                                Some(workbench_schema::TaskError {
                                    code: error.code.clone(),
                                    message: error.message.clone(),
                                    retryable: error.retryable,
                                    details: error.details.clone(),
                                }),
                            );
                            self.persist()?;
                            return Err(error);
                        }
                    }
                }
                if !invocation_is_readonly {
                    input["_workspaceSessionId"] = Value::String(workspace_session_id.to_owned());
                    // The executor validates authority against the capability contract's
                    // declared locks. The workspace Driver lease authorizes lock-free
                    // mutations, while declared capability locks use their matching
                    // Resource leases. Controller-only command resources may still be
                    // acquired above, but are intentionally not forwarded as executor
                    // authority because they are not part of the executor contract.
                    let mut executor_authority = required_authority.iter().collect::<Vec<_>>();
                    executor_authority.extend(acquired.iter().take(contract.locks.len()));
                    input["_authority"] = Value::Array(
                        executor_authority
                            .into_iter()
                            .map(|lease| {
                                json!({
                                    "controllerId": self.id,
                                    "resource": lease.resource,
                                    "fence": executor_execution_fence(lease.fence),
                                })
                            })
                            .collect(),
                    );
                }
                event_fields(
                    "info",
                    "executor.dispatch.started",
                    json!({"taskId": task.id, "executorId": executor_id, "capability": capability_name, "timeoutMs": contract.timeout_ms}),
                );
                let dispatch_started = Instant::now();
                let response = call_executor_with_timeout(
                    &executor.endpoint,
                    &traced_request(capability_name, input.clone()),
                    Duration::from_millis(contract.timeout_ms.max(1)),
                );
                let transport_timed_out = response
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string().contains("deadline"));
                // A timed-out transport may still have live work on the executor.
                // Retain its fenced leases until TTL expiry instead of permitting
                // a second writer while the first outcome is unknown.
                if !transport_timed_out {
                    release_acquired(&mut self.leases.lock().expect("lease lock"), &acquired);
                }
                event_fields(
                    if response.is_ok() { "info" } else { "error" },
                    "executor.dispatch.finished",
                    json!({"taskId": task.id, "executorId": executor_id, "capability": capability_name, "timeoutMs": contract.timeout_ms, "durationMs": dispatch_started.elapsed().as_millis(), "transportOk": response.is_ok()}),
                );
                match response {
                    Ok(response) if response.ok => {
                        let mut result = response.result.unwrap_or(Value::Null);
                        if capability_name == "process.start"
                            && input.get("waitReady").and_then(Value::as_bool) == Some(true)
                        {
                            match wait_for_process_readiness(&executor.endpoint, &input, result) {
                                Ok(ready) => result = ready,
                                Err(error) => {
                                    let _ = self.tasks.lock().expect("task lock").transition(
                                        &task.id,
                                        TaskState::Failed,
                                        None,
                                        Some(workbench_schema::TaskError {
                                            code: error.code.clone(),
                                            message: error.message.clone(),
                                            retryable: error.retryable,
                                            details: error.details.clone(),
                                        }),
                                    );
                                    self.persist()?;
                                    return Err(error);
                                }
                            }
                        }
                        if capability_name == "tunnel.ensure"
                            && input.get("waitReady").and_then(Value::as_bool) == Some(true)
                        {
                            match wait_for_tunnel_readiness(&executor.endpoint, &input, result) {
                                Ok(ready) => result = ready,
                                Err(error) => {
                                    let _ = self.tasks.lock().expect("task lock").transition(
                                        &task.id,
                                        TaskState::Failed,
                                        None,
                                        Some(workbench_schema::TaskError {
                                            code: error.code.clone(),
                                            message: error.message.clone(),
                                            retryable: error.retryable,
                                            details: error.details.clone(),
                                        }),
                                    );
                                    self.persist()?;
                                    return Err(error);
                                }
                            }
                        }
                        if let Err(error) =
                            validate_schema(&contract.output_schema, &result, "output")
                        {
                            let _ = self.tasks.lock().expect("task lock").transition(
                                &task.id,
                                TaskState::Failed,
                                None,
                                Some(workbench_schema::TaskError {
                                    code: error.code.clone(),
                                    message: error.message.clone(),
                                    retryable: false,
                                    details: error.details.clone(),
                                }),
                            );
                            self.persist()?;
                            return Err(error);
                        }
                        let retained_output = retained_task_output(capability_name, &result);
                        let task = self
                            .tasks
                            .lock()
                            .expect("task lock")
                            .transition(&task.id, TaskState::Succeeded, Some(retained_output), None)
                            .map_err(|error| {
                                RpcError::new("INVALID_TASK_STATE", error.to_string())
                            })?;
                        if let Some(artifact) = artifact_from_result(
                            capability_name,
                            &input,
                            &result,
                            workspace_session_id,
                            executor_id,
                            &task.id,
                        ) {
                            let mut state = self.state.lock().expect("state lock");
                            upsert_by(&mut state.artifacts, artifact.clone(), |existing| {
                                existing.digest == artifact.digest
                            });
                        }
                        if capability_name == "agent.start" {
                            let agent = AgentInstance {
                                id: required_str(&input, "agentId")?.to_owned(),
                                workspace_session_id: workspace_session_id.to_owned(),
                                executor_id: executor_id.to_owned(),
                                role: required_str(&input, "role")?.to_owned(),
                                provider: input
                                    .get("provider")
                                    .and_then(Value::as_str)
                                    .unwrap_or("process")
                                    .to_owned(),
                                state: AgentState::Running,
                                metadata: result.clone(),
                            };
                            let mut state = self.state.lock().expect("state lock");
                            upsert_by(&mut state.agents, agent.clone(), |existing| {
                                existing.id == agent.id
                            });
                        } else if capability_name == "agent.stop"
                            && let Some(agent) = self
                                .state
                                .lock()
                                .expect("state lock")
                                .agents
                                .iter_mut()
                                .find(|agent| {
                                    agent.id
                                        == input
                                            .get("agentId")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                })
                        {
                            agent.state = AgentState::Stopped;
                            agent.metadata = result.clone();
                        }
                        self.persist()?;
                        Ok(json!({"task": task, "reused": false, "result": result}))
                    }
                    Ok(response) => {
                        let error = response
                            .error
                            .unwrap_or_else(|| RpcError::new("EXECUTOR_FAILED", "executor failed"));
                        let _ = self.tasks.lock().expect("task lock").transition(
                            &task.id,
                            TaskState::Failed,
                            None,
                            Some(workbench_schema::TaskError {
                                code: error.code.clone(),
                                message: error.message.clone(),
                                retryable: error.retryable,
                                details: error.details.clone(),
                            }),
                        );
                        self.persist()?;
                        Err(error)
                    }
                    Err(error) => {
                        let timed_out = error.to_string().contains("deadline");
                        let mut rpc = RpcError::new(
                            if timed_out {
                                "EXECUTOR_TIMEOUT"
                            } else {
                                "EXECUTOR_UNAVAILABLE"
                            },
                            error.to_string(),
                        );
                        rpc.retryable = true;
                        let _ = self.tasks.lock().expect("task lock").transition(
                            &task.id,
                            if timed_out {
                                TaskState::OutcomeUnknown
                            } else {
                                TaskState::Failed
                            },
                            None,
                            Some(workbench_schema::TaskError {
                                code: rpc.code.clone(),
                                message: rpc.message.clone(),
                                retryable: true,
                                details: Value::Null,
                            }),
                        );
                        self.persist()?;
                        Err(rpc)
                    }
                }
            }
            "driver.status" | "lease.status" => {
                let resource = required_str(&params, "resource")?;
                Ok(
                    serde_json::to_value(self.leases.lock().expect("lease lock").get(resource))
                        .expect("lease serializes"),
                )
            }
            "driver.handoff.status" => {
                let workspace = required_str(&params, "workspaceSessionId")?;
                self.refresh_driver_handoff(workspace)?;
                let state = self.state.lock().expect("state lock");
                Ok(serde_json::to_value(
                    state
                        .driver_handoff_requests
                        .iter()
                        .filter(|request| request.workspace_session_id == workspace)
                        .max_by_key(|request| request.created_at),
                )
                .expect("handoff serializes"))
            }
            "agent.mutation.start" => {
                let workspace = required_str(&params, "workspaceSessionId")?;
                let agent = required_str(&params, "agentId")?;
                if self.workspace_is_draining(workspace, agent) {
                    return Err(RpcError::new(
                        "DRIVER_DRAINING",
                        "workspace driver is draining; mutation hook rejected new write",
                    ));
                }
                let operation = AgentMutationOperation {
                    id: required_str(&params, "operationId")?.to_owned(),
                    workspace_session_id: workspace.to_owned(),
                    agent_id: agent.to_owned(),
                    tool: required_str(&params, "tool")?.to_owned(),
                    started_at: now_ms(),
                };
                let mut state = self.state.lock().expect("state lock");
                if !state
                    .agent_mutations
                    .iter()
                    .any(|item| item.id == operation.id)
                {
                    state.agent_mutations.push(operation.clone());
                }
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(operation).expect("mutation serializes"))
            }
            "agent.mutation.finish" => {
                let id = required_str(&params, "operationId")?;
                let workspace = required_str(&params, "workspaceSessionId")?;
                self.state
                    .lock()
                    .expect("state lock")
                    .agent_mutations
                    .retain(|item| item.id != id);
                self.refresh_driver_handoff(workspace)?;
                self.persist()?;
                Ok(json!({"operationId": id, "finished": true}))
            }
            "driver.handoff.request" => {
                let workspace = required_str(&params, "workspaceSessionId")?;
                let requester = required_str(&params, "owner")?;
                let ttl_ms = params
                    .get("ttlMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300_000);
                let resource = format!("workspace:{workspace}");
                let now = now_ms();
                let existing = self
                    .leases
                    .lock()
                    .expect("lease lock")
                    .get(&resource)
                    .cloned();
                if existing
                    .as_ref()
                    .is_some_and(|lease| lease.owner == requester)
                {
                    return Err(RpcError::new(
                        "ALREADY_DRIVER",
                        "requester already owns workspace driver",
                    ));
                }
                let request = DriverHandoffRequest {
                    id: format!("driver_handoff_{}", Uuid::new_v4().simple()),
                    workspace_session_id: workspace.to_owned(),
                    resource: resource.clone(),
                    requested_by: requester.to_owned(),
                    previous_owner: existing.as_ref().map(|lease| lease.owner.clone()),
                    state: if existing.is_some() {
                        DriverHandoffState::Draining
                    } else {
                        DriverHandoffState::Ready
                    },
                    created_at: now,
                    expires_at: now.saturating_add(ttl_ms),
                    completed_at: None,
                };
                if existing.is_some() {
                    self.leases
                        .lock()
                        .expect("lease lock")
                        .request_handoff(&resource, requester)
                        .map_err(map_lease_error)?;
                }
                self.state
                    .lock()
                    .expect("state lock")
                    .driver_handoff_requests
                    .push(request.clone());
                self.refresh_driver_handoff(workspace)?;
                self.persist()?;
                Ok(serde_json::to_value(request).expect("handoff serializes"))
            }
            "driver.handoff.await" => {
                let workspace = required_str(&params, "workspaceSessionId")?;
                let requester = required_str(&params, "owner")?;
                self.refresh_driver_handoff(workspace)?;
                let resource = format!("workspace:{workspace}");
                let ttl_ms = params
                    .get("ttlMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300_000);
                let mut state = self.state.lock().expect("state lock");
                let request = state
                    .driver_handoff_requests
                    .iter_mut()
                    .rev()
                    .find(|item| {
                        item.workspace_session_id == workspace
                            && item.requested_by == requester
                            && matches!(
                                item.state,
                                DriverHandoffState::Ready | DriverHandoffState::Draining
                            )
                    })
                    .ok_or_else(|| {
                        RpcError::new("DRIVER_HANDOFF_NOT_FOUND", "no active handoff request")
                    })?;
                if request.state != DriverHandoffState::Ready {
                    let mut error = RpcError::new(
                        "DRIVER_HANDOFF_DRAINING",
                        "current writer is still draining",
                    );
                    error.retryable = true;
                    return Err(error);
                }
                let had_owner = request.previous_owner.is_some();
                let lease = if had_owner {
                    self.leases
                        .lock()
                        .expect("lease lock")
                        .take_handoff(&resource, requester, ttl_ms)
                } else {
                    self.leases.lock().expect("lease lock").acquire(
                        LeaseKind::Driver,
                        &resource,
                        requester,
                        ttl_ms,
                    )
                }
                .map_err(map_lease_error)?;
                request.state = DriverHandoffState::Completed;
                request.completed_at = Some(now_ms());
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(lease).expect("lease serializes"))
            }
            "driver.handoff.cancel" => {
                let workspace = required_str(&params, "workspaceSessionId")?;
                let requester = required_str(&params, "owner")?;
                let resource = format!("workspace:{workspace}");
                let mut state = self.state.lock().expect("state lock");
                let request = state
                    .driver_handoff_requests
                    .iter_mut()
                    .rev()
                    .find(|item| {
                        item.workspace_session_id == workspace
                            && item.requested_by == requester
                            && matches!(
                                item.state,
                                DriverHandoffState::Requested
                                    | DriverHandoffState::Draining
                                    | DriverHandoffState::Ready
                            )
                    })
                    .ok_or_else(|| {
                        RpcError::new("DRIVER_HANDOFF_NOT_FOUND", "no active handoff request")
                    })?;
                if request.previous_owner.is_some() {
                    self.leases
                        .lock()
                        .expect("lease lock")
                        .cancel_handoff(&resource, requester)
                        .map_err(map_lease_error)?;
                }
                request.state = DriverHandoffState::Cancelled;
                drop(state);
                self.persist()?;
                Ok(json!({"cancelled": true, "workspaceSessionId": workspace}))
            }
            "driver.acquire" | "lease.acquire" => {
                let resource = required_str(&params, "resource")?;
                let owner = required_str(&params, "owner")?;
                if action == "lease.acquire" {
                    let requested_rank = lease_order_rank(resource);
                    let held = self.leases.lock().expect("lease lock").snapshot();
                    if held.iter().any(|lease| {
                        lease.owner == owner
                            && lease.resource != resource
                            && lease_order_rank(&lease.resource) < requested_rank
                    }) {
                        return Err(RpcError::new(
                            "LEASE_ORDER_VIOLATION",
                            "release workspace driver before runtime lease, and runtime lease before acceptance lease",
                        ));
                    }
                }
                let ttl_ms = params
                    .get("ttlMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300_000);
                let kind = if action.starts_with("driver") {
                    LeaseKind::Driver
                } else {
                    LeaseKind::Resource
                };
                let lease = self
                    .leases
                    .lock()
                    .expect("lease lock")
                    .acquire(kind, resource, owner, ttl_ms)
                    .map_err(map_lease_error)?;
                self.persist()?;
                Ok(serde_json::to_value(lease).expect("lease serializes"))
            }
            "driver.renew" | "lease.renew" => {
                let lease = self
                    .leases
                    .lock()
                    .expect("lease lock")
                    .renew(
                        required_str(&params, "resource")?,
                        required_str(&params, "owner")?,
                        required_str(&params, "token")?,
                        params
                            .get("ttlMs")
                            .and_then(Value::as_u64)
                            .unwrap_or(300_000),
                    )
                    .map_err(map_lease_error)?;
                self.persist()?;
                Ok(serde_json::to_value(lease).expect("lease serializes"))
            }
            "driver.handoff" => {
                let lease = self
                    .leases
                    .lock()
                    .expect("lease lock")
                    .handoff(
                        required_str(&params, "resource")?,
                        required_str(&params, "owner")?,
                        required_str(&params, "token")?,
                        required_str(&params, "target")?,
                    )
                    .map_err(map_lease_error)?;
                self.persist()?;
                Ok(serde_json::to_value(lease).expect("lease serializes"))
            }
            "driver.take" => {
                let resource = required_str(&params, "resource")?;
                let owner = required_str(&params, "owner")?;
                let ttl_ms = params
                    .get("ttlMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300_000);
                let mut leases = self.leases.lock().expect("lease lock");
                let lease = if leases.get(resource).is_some() {
                    leases.take_handoff(resource, owner, ttl_ms)
                } else {
                    leases.acquire(LeaseKind::Driver, resource, owner, ttl_ms)
                }
                .map_err(map_lease_error)?;
                drop(leases);
                self.persist()?;
                Ok(serde_json::to_value(lease).expect("lease serializes"))
            }
            "driver.release" | "lease.release" => {
                let lease = self
                    .leases
                    .lock()
                    .expect("lease lock")
                    .release(
                        required_str(&params, "resource")?,
                        required_str(&params, "owner")?,
                        required_str(&params, "token")?,
                    )
                    .map_err(map_lease_error)?;
                self.persist()?;
                Ok(serde_json::to_value(lease).expect("lease serializes"))
            }
            "task.submit" => {
                let (correlation_id, request_id) = current_trace();
                let (task, reused) = self.tasks.lock().expect("task lock").submit_traced(
                    required_str(&params, "workspaceSessionId")?,
                    required_str(&params, "executorId")?,
                    required_str(&params, "capability")?,
                    params.get("input").cloned().unwrap_or(Value::Null),
                    required_str(&params, "idempotencyKey")?,
                    correlation_id,
                    request_id,
                );
                self.persist()?;
                self.observe_task_state(&task);
                Ok(json!({"task": task, "reused": reused}))
            }
            "task.get" => {
                let id = required_str(&params, "taskId")?;
                let tasks = self.tasks.lock().expect("task lock");
                let task = tasks.get(id).ok_or_else(|| {
                    RpcError::new("TASK_NOT_FOUND", format!("unknown task: {id}"))
                })?;
                Ok(serde_json::to_value(task).expect("task serializes"))
            }
            "task.list" => Ok(serde_json::to_value(
                self.tasks.lock().expect("task lock").snapshot(),
            )
            .expect("tasks serialize")),
            "task.events" => {
                let id = required_str(&params, "taskId")?;
                let tasks = self.tasks.lock().expect("task lock");
                let task = tasks.get(id).ok_or_else(|| {
                    RpcError::new("TASK_NOT_FOUND", format!("unknown task: {id}"))
                })?;
                Ok(json!({"taskId": id, "events": task.events}))
            }
            "task.wait" => {
                let id = required_str(&params, "taskId")?.to_owned();
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(30_000)
                    .min(3_600_000);
                let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                loop {
                    let task = self
                        .tasks
                        .lock()
                        .expect("task lock")
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| {
                            RpcError::new("TASK_NOT_FOUND", format!("unknown task: {id}"))
                        })?;
                    if task.state.terminal() {
                        break Ok(serde_json::to_value(task).expect("task serializes"));
                    }
                    if Instant::now() >= deadline {
                        break Err(RpcError::new(
                            "TASK_WAIT_TIMEOUT",
                            format!("task {id} did not finish before timeout"),
                        ));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
            "task.retry" => {
                let task = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .retry(required_str(&params, "taskId")?)
                    .map_err(|error| RpcError::new("TASK_NOT_RETRYABLE", error.to_string()))?;
                self.persist()?;
                self.observe_task_state(&task);
                Ok(serde_json::to_value(task).expect("task serializes"))
            }
            "task.cancel" => {
                if params.get("_operatorAuthorized").and_then(Value::as_bool) != Some(true) {
                    return Err(RpcError::new(
                        "OPERATOR_REQUIRED",
                        "task cancellation requires an operator nonce",
                    ));
                }
                let task_id = required_str(&params, "taskId")?.to_owned();
                let task = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .get(&task_id)
                    .cloned()
                    .ok_or_else(|| {
                        RpcError::new("TASK_NOT_FOUND", format!("unknown task: {task_id}"))
                    })?;
                if task.state.terminal() {
                    return Ok(serde_json::to_value(task).expect("task serializes"));
                }
                if task.state == TaskState::Queued {
                    let task = self
                        .tasks
                        .lock()
                        .expect("task lock")
                        .transition(&task_id, TaskState::Cancelled, None, None)
                        .map_err(|error| RpcError::new("INVALID_TASK_STATE", error.to_string()))?;
                    self.persist()?;
                    self.observe_task_state(&task);
                    return Ok(serde_json::to_value(task).unwrap());
                }
                let cancel_requested = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .transition(&task_id, TaskState::CancelRequested, None, None)
                    .map_err(|error| RpcError::new("INVALID_TASK_STATE", error.to_string()))?;
                self.observe_task_state(&cancel_requested);
                let cancelling = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .transition(&task_id, TaskState::Cancelling, None, None)
                    .map_err(|error| RpcError::new("INVALID_TASK_STATE", error.to_string()))?;
                self.observe_task_state(&cancelling);
                self.persist()?;
                let process_id =
                    task.input
                        .get("processId")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            task.output
                                .as_ref()
                                .and_then(|output| output.get("id"))
                                .and_then(Value::as_str)
                        });
                let stop_result = if let Some(process_id) = process_id {
                    self.call_registered_executor(
                        &task.executor_id,
                        "process.stop",
                        json!({"processId":process_id}),
                    )
                } else {
                    Err(RpcError::new(
                        "TASK_PROCESS_NOT_REGISTERED",
                        "running task has no registered processId",
                    ))
                };
                let (state, error) = match stop_result {
                    Ok(_) => (TaskState::Cancelled, None),
                    Err(error) => (
                        TaskState::FailedToCancel,
                        Some(workbench_schema::TaskError {
                            code: error.code,
                            message: error.message,
                            retryable: error.retryable,
                            details: error.details,
                        }),
                    ),
                };
                let task = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .transition(&task_id, state, None, error)
                    .map_err(|error| RpcError::new("INVALID_TASK_STATE", error.to_string()))?;
                self.persist()?;
                self.observe_task_state(&task);
                Ok(serde_json::to_value(task).unwrap())
            }
            "task.prune" => {
                let cutoff = params
                    .get("before")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(now_ms);
                let removed = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .prune_terminal_before(cutoff);
                self.persist()?;
                Ok(json!({
                    "pruned": removed.len(),
                    "taskIds": removed.into_iter().map(|task| task.id).collect::<Vec<_>>()
                }))
            }
            "task.transition" => {
                let state: TaskState = serde_json::from_value(
                    params
                        .get("state")
                        .cloned()
                        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "state is required"))?,
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let task = self
                    .tasks
                    .lock()
                    .expect("task lock")
                    .transition(
                        required_str(&params, "taskId")?,
                        state,
                        params.get("output").cloned(),
                        None,
                    )
                    .map_err(|error| RpcError::new("INVALID_TASK_STATE", error.to_string()))?;
                self.persist()?;
                self.observe_task_state(&task);
                Ok(serde_json::to_value(task).expect("task serializes"))
            }
            "port.list" => Ok(serde_json::to_value(
                self.leases
                    .lock()
                    .expect("lease lock")
                    .snapshot()
                    .into_iter()
                    .filter(|lease| lease.resource.starts_with("port:"))
                    .collect::<Vec<_>>(),
            )
            .expect("port leases serialize")),
            "port.allocate" => {
                let executor_id = required_str(&params, "executorId")?;
                let owner = required_str(&params, "owner")?;
                let start = params
                    .get("start")
                    .and_then(Value::as_u64)
                    .unwrap_or(20_000);
                let end = params.get("end").and_then(Value::as_u64).unwrap_or(40_000);
                if start == 0 || end > 65_535 || start > end {
                    return Err(RpcError::new("INVALID_PARAMS", "invalid port range"));
                }
                let executor = self
                    .state
                    .lock()
                    .expect("state lock")
                    .executors
                    .iter()
                    .find(|executor| executor.metadata.id == executor_id)
                    .cloned()
                    .ok_or_else(|| RpcError::new("EXECUTOR_NOT_FOUND", "executor not found"))?;
                for port in start..=end {
                    let resource = format!("port:{executor_id}:{port}");
                    let lease = match self.leases.lock().expect("lease lock").acquire(
                        LeaseKind::Resource,
                        &resource,
                        owner,
                        params
                            .get("ttlMs")
                            .and_then(Value::as_u64)
                            .unwrap_or(300_000),
                    ) {
                        Ok(lease) => lease,
                        Err(LeaseError::Active { .. }) => continue,
                        Err(error) => return Err(map_lease_error(error)),
                    };
                    let response = call_executor(
                        &executor.endpoint,
                        &traced_request("port.check", json!({"port": port})),
                    );
                    let available = response
                        .ok()
                        .and_then(|response| response.result)
                        .and_then(|result| result.get("available").and_then(Value::as_bool))
                        .unwrap_or(false);
                    if available {
                        self.persist()?;
                        return Ok(
                            json!({"executorId": executor_id, "port": port, "lease": lease}),
                        );
                    }
                    let _ = self.leases.lock().expect("lease lock").release(
                        &resource,
                        owner,
                        &lease.token,
                    );
                }
                Err(RpcError::new(
                    "PORT_UNAVAILABLE",
                    format!("no available port in {start}..={end}"),
                ))
            }
            "transaction.begin" => {
                let idempotency_key = required_str(&params, "idempotencyKey")?;
                let mut state = self.state.lock().expect("state lock");
                if let Some(existing) = state
                    .transactions
                    .iter()
                    .find(|transaction| transaction.idempotency_key == idempotency_key)
                {
                    return Ok(json!({"transaction": existing, "reused": true}));
                }
                let now = now_ms();
                let transaction = ActivationTransaction {
                    id: format!("transaction_{}", Uuid::new_v4().simple()),
                    workspace_session_id: required_str(&params, "workspaceSessionId")?.to_owned(),
                    idempotency_key: idempotency_key.to_owned(),
                    target: required_str(&params, "target")?.to_owned(),
                    generation_id: required_str(&params, "generationId")?.to_owned(),
                    previous_generation_id: params
                        .get("previousGenerationId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    state: TransactionState::Planned,
                    created_at: now,
                    updated_at: now,
                    completed_steps: Vec::new(),
                    journal: Vec::new(),
                    lease_fence: params.get("leaseFence").and_then(Value::as_u64),
                    evidence: Vec::new(),
                    error: None,
                };
                state.transactions.push(transaction.clone());
                drop(state);
                self.persist()?;
                Ok(json!({"transaction": transaction, "reused": false}))
            }
            "transaction.get" => {
                let id = required_str(&params, "transactionId")?;
                let state = self.state.lock().expect("state lock");
                let transaction = state
                    .transactions
                    .iter()
                    .find(|transaction| transaction.id == id)
                    .ok_or_else(|| {
                        RpcError::new(
                            "TRANSACTION_NOT_FOUND",
                            format!("unknown transaction: {id}"),
                        )
                    })?;
                Ok(serde_json::to_value(transaction).expect("transaction serializes"))
            }
            "transaction.list" => Ok(serde_json::to_value(
                &self.state.lock().expect("state lock").transactions,
            )
            .expect("transactions serialize")),
            "transaction.record" => {
                let id = required_str(&params, "transactionId")?;
                let step = required_str(&params, "step")?.to_owned();
                let step_state: TransactionStepState = serde_json::from_value(
                    params
                        .get("state")
                        .cloned()
                        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "state is required"))?,
                )
                .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                let mut state = self.state.lock().expect("state lock");
                let transaction = state
                    .transactions
                    .iter_mut()
                    .find(|transaction| transaction.id == id)
                    .ok_or_else(|| {
                        RpcError::new(
                            "TRANSACTION_NOT_FOUND",
                            format!("unknown transaction: {id}"),
                        )
                    })?;
                if let (Some(expected), Some(actual)) = (
                    transaction.lease_fence,
                    params.get("fence").and_then(Value::as_u64),
                ) && expected != actual
                {
                    return Err(RpcError::new(
                        "STALE_FENCING_TOKEN",
                        format!("transaction fence is {expected}, request used {actual}"),
                    ));
                }
                let now = now_ms();
                let attempt = params.get("attempt").and_then(Value::as_u64).unwrap_or(1) as u32;
                transaction.journal.push(TransactionJournalEntry {
                    sequence: transaction.journal.len() as u64 + 1,
                    step: step.clone(),
                    state: step_state.clone(),
                    attempt,
                    started_at: params
                        .get("startedAt")
                        .and_then(Value::as_u64)
                        .unwrap_or(now),
                    finished_at: if matches!(
                        step_state,
                        TransactionStepState::Succeeded
                            | TransactionStepState::Failed
                            | TransactionStepState::Compensated
                            | TransactionStepState::OutcomeUnknown
                    ) {
                        Some(now)
                    } else {
                        None
                    },
                    input_digest: params
                        .get("inputDigest")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    output_digest: params
                        .get("outputDigest")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    executor_id: params
                        .get("executorId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    fence: params.get("fence").and_then(Value::as_u64),
                    details: params.get("details").cloned().unwrap_or(Value::Null),
                });
                transaction.updated_at = now;
                if step == "activate"
                    && let Some(previous) =
                        params.get("previousGenerationId").and_then(Value::as_str)
                {
                    transaction.previous_generation_id = Some(previous.to_owned());
                }
                match step_state {
                    TransactionStepState::Started => transaction.state = TransactionState::Running,
                    TransactionStepState::Succeeded => {
                        if !transaction.completed_steps.contains(&step) {
                            transaction.completed_steps.push(step.clone());
                        }
                        if step == "activate" {
                            transaction.state = TransactionState::Activated;
                        }
                    }
                    TransactionStepState::Failed => transaction.state = TransactionState::Failed,
                    TransactionStepState::OutcomeUnknown => {
                        transaction.state = TransactionState::OutcomeUnknown
                    }
                    TransactionStepState::Compensated => {
                        transaction.state = TransactionState::RolledBack
                    }
                    TransactionStepState::Planned => {}
                }
                let result = transaction.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(result).expect("transaction serializes"))
            }
            "handoff.create" => {
                let mut handoff: Handoff = serde_json::from_value(params)
                    .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))?;
                if handoff.id.is_empty() {
                    handoff.id = format!("handoff_{}", Uuid::new_v4().simple());
                }
                if handoff.created_at == 0 {
                    handoff.created_at = now_ms();
                }
                self.state
                    .lock()
                    .expect("state lock")
                    .handoffs
                    .push(handoff.clone());
                self.persist()?;
                Ok(serde_json::to_value(handoff).expect("handoff serializes"))
            }
            "handoff.get" => {
                let id = required_str(&params, "handoffId")?;
                let state = self.state.lock().expect("state lock");
                let handoff = state
                    .handoffs
                    .iter()
                    .find(|handoff| handoff.id == id)
                    .ok_or_else(|| {
                        RpcError::new("HANDOFF_NOT_FOUND", format!("unknown handoff: {id}"))
                    })?;
                Ok(serde_json::to_value(handoff).expect("handoff serializes"))
            }
            "handoff.list" => {
                let state = self.state.lock().expect("state lock");
                let kind = params.get("kind").and_then(Value::as_str);
                let agent = params.get("agentId").and_then(Value::as_str);
                let handoffs: Vec<_> = state
                    .handoffs
                    .iter()
                    .filter(|handoff| {
                        kind.is_none_or(|value| {
                            value
                                == match handoff.kind {
                                    workbench_schema::HandoffKind::Work => "work",
                                    workbench_schema::HandoffKind::Acceptance => "acceptance",
                                }
                        }) && agent
                            .is_none_or(|value| handoff.to.agent_id.as_deref() == Some(value))
                    })
                    .collect();
                Ok(serde_json::to_value(handoffs).expect("handoffs serialize"))
            }
            "handoff.report" => {
                let id = required_str(&params, "handoffId")?;
                let resource = required_str(&params, "acceptanceResource")?;
                if !resource.starts_with("acceptance:") {
                    return Err(RpcError::new(
                        "INVALID_ACCEPTANCE_RESOURCE",
                        "acceptanceResource must use acceptance:<executor>:<port>",
                    ));
                }
                self.leases
                    .lock()
                    .expect("lease lock")
                    .validate(
                        resource,
                        required_str(&params, "owner")?,
                        required_str(&params, "leaseToken")?,
                    )
                    .map_err(map_lease_error)?;
                let mut state = self.state.lock().expect("state lock");
                let handoff = state
                    .handoffs
                    .iter_mut()
                    .find(|handoff| handoff.id == id)
                    .ok_or_else(|| {
                        RpcError::new("HANDOFF_NOT_FOUND", format!("unknown handoff: {id}"))
                    })?;
                if handoff.kind != workbench_schema::HandoffKind::Acceptance {
                    return Err(RpcError::new(
                        "INVALID_HANDOFF_KIND",
                        "reports are only valid for acceptance handoffs",
                    ));
                }
                handoff.report = Some(params.get("report").cloned().unwrap_or(Value::Null));
                let result = handoff.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(result).expect("handoff serializes"))
            }
            "handoff.acknowledge" | "handoff.complete" => {
                let id = required_str(&params, "handoffId")?;
                let mut state = self.state.lock().expect("state lock");
                let handoff = state
                    .handoffs
                    .iter_mut()
                    .find(|handoff| handoff.id == id)
                    .ok_or_else(|| {
                        RpcError::new("HANDOFF_NOT_FOUND", format!("unknown handoff: {id}"))
                    })?;
                let now = now_ms();
                if action == "handoff.acknowledge" {
                    if handoff.acknowledged_at.is_some() {
                        return Err(RpcError::new(
                            "HANDOFF_ALREADY_ACKNOWLEDGED",
                            format!("handoff already acknowledged: {id}"),
                        ));
                    }
                    handoff.acknowledged_at = Some(now);
                } else {
                    if handoff.acknowledged_at.is_none() {
                        return Err(RpcError::new(
                            "HANDOFF_NOT_ACKNOWLEDGED",
                            "handoff must be acknowledged before completion",
                        ));
                    }
                    if handoff.kind == workbench_schema::HandoffKind::Acceptance {
                        let resource = required_str(&params, "acceptanceResource")?;
                        if !resource.starts_with("acceptance:") {
                            return Err(RpcError::new(
                                "INVALID_ACCEPTANCE_RESOURCE",
                                "acceptance resource is required",
                            ));
                        }
                        self.leases
                            .lock()
                            .expect("lease lock")
                            .validate(
                                resource,
                                required_str(&params, "owner")?,
                                required_str(&params, "leaseToken")?,
                            )
                            .map_err(map_lease_error)?;
                        if handoff.report.is_none() {
                            return Err(RpcError::new(
                                "ACCEPTANCE_REPORT_REQUIRED",
                                "acceptance report is required before completion",
                            ));
                        }
                    }
                    handoff.completed_at = Some(now);
                }
                let result = handoff.clone();
                drop(state);
                self.persist()?;
                Ok(serde_json::to_value(result).expect("handoff serializes"))
            }
            _ => Err(RpcError::new(
                "UNKNOWN_ACTION",
                format!("unknown action: {action}"),
            )),
        }
    }

    fn call_registered_executor(
        &self,
        executor_id: &str,
        action: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let executor = self
            .state
            .lock()
            .expect("state lock")
            .executors
            .iter()
            .find(|executor| executor.metadata.id == executor_id)
            .cloned()
            .ok_or_else(|| {
                RpcError::new(
                    "EXECUTOR_NOT_FOUND",
                    format!("unknown executor: {executor_id}"),
                )
            })?;
        let response = call_executor(&executor.endpoint, &traced_request(action, params)).map_err(
            |error| {
                let mut rpc = RpcError::new("EXECUTOR_UNAVAILABLE", error.to_string());
                rpc.retryable = true;
                rpc
            },
        )?;
        if response.ok {
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            Err(response
                .error
                .unwrap_or_else(|| RpcError::new("EXECUTOR_FAILED", "executor failed")))
        }
    }

    fn call_peer_until_success(&self, action: &str, params: Value) -> Result<Value, RpcError> {
        let controllers = self.state.lock().expect("state lock").controllers.clone();
        for controller in controllers {
            if let Ok(response) = call_executor(
                &controller.endpoint,
                &traced_request(action, params.clone()),
            ) && response.ok
            {
                return Ok(response.result.unwrap_or(Value::Null));
            }
        }
        Err(RpcError::new(
            "RUN_NOT_FOUND",
            "run was not found on any connected Controller",
        ))
    }

    fn observe_task_state(&self, task: &Task) {
        let _ = self.observations.append(&Observation {
            event_id: 0,
            run_id: current_run_id(),
            timestamp: now_ms(),
            node_id: self.id.clone(),
            role: "controller".into(),
            kind: "task-state".into(),
            name: task.capability.clone(),
            status: serde_json::to_value(&task.state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into()),
            duration_ms: None,
            span_id: None,
            parent_span_id: None,
            request_id: task.request_id.clone(),
            task_id: Some(task.id.clone()),
            process_id: task
                .input
                .get("processId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            connection_id: None,
            attributes: json!({"capability":task.capability,"targetNode":task.executor_id}),
        });
    }

    fn relay_artifact(&self, params: &Value) -> Result<Value, RpcError> {
        let transfer_started = Instant::now();
        let source = params
            .get("source")
            .ok_or_else(|| RpcError::new("INVALID_PARAMS", "source is required"))?;
        let destination = params
            .get("destination")
            .ok_or_else(|| RpcError::new("INVALID_PARAMS", "destination is required"))?;
        let source_executor = required_str(source, "executorId")?;
        let source_path = required_str(source, "path")?;
        let destination_executor = required_str(destination, "executorId")?;
        let destination_path = required_str(destination, "path")?;
        let workspace_session_id = required_str(params, "workspaceSessionId")?;
        let owner = required_str(params, "owner")?;
        let driver_token = required_str(params, "driverToken")?;
        let driver = {
            let leases = self.leases.lock().expect("lease lock");
            leases
                .validate(
                    &format!("workspace:{workspace_session_id}"),
                    owner,
                    driver_token,
                )
                .map_err(map_lease_error)?
                .clone()
        };
        let source_authority = json!([{
            "controllerId": self.id,
            "resource": driver.resource,
            "fence": executor_execution_fence(driver.fence),
        }]);
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("mirror");
        if mode != "mirror" {
            return Err(RpcError::new(
                "INVALID_PARAMS",
                "Controller artifact relay currently requires mode=mirror",
            ));
        }
        transfer_event(
            "manifesting",
            json!({"sourceExecutorId": source_executor, "destinationExecutorId": destination_executor, "sourcePath": source_path, "destinationPath": destination_path}),
        );
        let archive_started = Instant::now();
        match self.call_registered_executor(
            source_executor,
            "artifact.relay.archive.create",
            json!({"path": source_path, "_workspaceSessionId": workspace_session_id, "_authority": source_authority}),
        ) {
            Ok(archive) => {
                transfer_event(
                    "archived",
                    json!({"durationMs": archive_started.elapsed().as_millis(), "archiveSize": archive.get("archiveSize"), "size": archive.get("size"), "files": archive.get("files")}),
                );
                let token = required_str(&archive, "token")?.to_owned();
                let result = self.relay_archive(
                    source_executor,
                    destination_executor,
                    destination_path,
                    mode,
                    archive,
                    (workspace_session_id, &source_authority),
                );
                let _ = self.call_registered_executor(
                    source_executor,
                    "artifact.relay.archive.remove",
                    json!({"token": token, "_workspaceSessionId": workspace_session_id, "_authority": source_authority}),
                );
                return result.map(|mut value| {
                    value["transfer"]["totalDurationMs"] =
                        json!(transfer_started.elapsed().as_millis());
                    value
                });
            }
            Err(error) if error.code == "UNKNOWN_ACTION" => {}
            Err(error) => return Err(error),
        }
        let mut manifest = self.call_registered_executor(
            source_executor,
            "artifact.relay.manifest",
            json!({"path": source_path}),
        )?;
        let stability_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            thread::sleep(Duration::from_millis(250));
            let next = self.call_registered_executor(
                source_executor,
                "artifact.relay.manifest",
                json!({"path": source_path}),
            )?;
            if next.get("digest") == manifest.get("digest")
                && next.get("entries") == manifest.get("entries")
            {
                manifest = next;
                break;
            }
            if Instant::now() >= stability_deadline {
                return Err(RpcError::new(
                    "ARTIFACT_NOT_STABLE",
                    "source artifact changed during the stability window",
                ));
            }
            manifest = next;
        }
        let entries = manifest
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| RpcError::new("INVALID_ARTIFACT", "source manifest has no entries"))?;
        if params.get("rejectEmptyFiles").and_then(Value::as_bool) == Some(true)
            && entries.iter().any(|entry| {
                entry.get("kind").and_then(Value::as_str) == Some("file")
                    && entry.get("size").and_then(Value::as_u64) == Some(0)
            })
        {
            return Err(RpcError::new(
                "ARTIFACT_INCOMPLETE",
                "source artifact contains an empty file",
            ));
        }
        let expected_digest = required_str(&manifest, "digest")?.to_owned();
        let resource = format!("artifact-relay:{destination_path}");
        let owner = format!("controller:{}", self.id);
        let lease = self
            .leases
            .lock()
            .expect("lease lock")
            .acquire(
                LeaseKind::Resource,
                resource.clone(),
                owner.clone(),
                3_600_000,
            )
            .map_err(map_lease_error)?;
        self.persist()?;
        let staging = format!(
            "{destination_path}.workbench-relay-{}",
            Uuid::new_v4().simple()
        );
        let authority =
            json!([{"controllerId": self.id, "resource": resource, "fence": lease.fence}]);
        let result = (|| {
            self.call_registered_executor(
                destination_executor,
                "artifact.relay.prepare",
                json!({"destination": destination_path, "staging": staging, "entries": entries, "_authority": authority}),
            )?;
            for entry in &entries {
                if required_str(entry, "kind")? != "file" {
                    continue;
                }
                let relative = required_str(entry, "path")?;
                let size = entry
                    .get("size")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| RpcError::new("INVALID_ARTIFACT", "file size is required"))?;
                let mut offset = 0_u64;
                while offset < size {
                    let chunk = self.call_registered_executor(
                        source_executor,
                        "artifact.relay.read",
                        json!({"path": source_path, "relativePath": relative, "offset": offset, "limit": 1024 * 1024}),
                    )?;
                    let bytes = chunk.get("bytes").and_then(Value::as_u64).ok_or_else(|| {
                        RpcError::new("INVALID_ARTIFACT", "relay chunk has no byte count")
                    })?;
                    if bytes == 0 {
                        return Err(RpcError::new(
                            "ARTIFACT_TRANSFER_FAILED",
                            format!("unexpected EOF while reading {relative}"),
                        ));
                    }
                    self.call_registered_executor(
                        destination_executor,
                        "artifact.relay.write",
                        json!({"destination": destination_path, "staging": staging, "relativePath": relative, "offset": offset, "data": chunk["data"], "_authority": authority}),
                    )?;
                    offset = offset.saturating_add(bytes);
                }
            }
            let committed = self.call_registered_executor(
                destination_executor,
                "artifact.relay.commit",
                json!({"destination": destination_path, "staging": staging, "expectedDigest": expected_digest, "_authority": authority}),
            )?;
            Ok(json!({
                "source": {"executorId": source_executor, "path": source_path},
                "destination": {"executorId": destination_executor, "path": destination_path},
                "mode": mode,
                "digest": committed["digest"], "size": committed["size"], "files": committed["files"]
            }))
        })();
        let _ = self
            .leases
            .lock()
            .expect("lease lock")
            .release(&resource, &owner, &lease.token);
        self.persist()?;
        result
    }

    fn relay_archive(
        &self,
        source_executor: &str,
        destination_executor: &str,
        destination_path: &str,
        mode: &str,
        archive: Value,
        authority_context: (&str, &Value),
    ) -> Result<Value, RpcError> {
        let (workspace_session_id, driver_authority) = authority_context;
        let token = required_str(&archive, "token")?.to_owned();
        let expected_digest = required_str(&archive, "digest")?.to_owned();
        let archive_size = archive
            .get("archiveSize")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::new("INVALID_ARTIFACT", "archive size is required"))?;
        let size = archive
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::new("INVALID_ARTIFACT", "artifact size is required"))?;
        let files = archive
            .get("files")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::new("INVALID_ARTIFACT", "artifact file count is required"))?;
        let kind = archive
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("directory");
        let resource = format!("artifact-relay:{destination_path}");
        let owner = format!("controller:{}", self.id);
        let lease = self
            .leases
            .lock()
            .expect("lease lock")
            .acquire(
                LeaseKind::Resource,
                resource.clone(),
                owner.clone(),
                3_600_000,
            )
            .map_err(map_lease_error)?;
        self.persist()?;
        let staging = format!(
            "{destination_path}.workbench-relay-{}",
            Uuid::new_v4().simple()
        );
        let mut authority = driver_authority.as_array().cloned().unwrap_or_default();
        authority
            .push(json!({"controllerId": self.id, "resource": resource, "fence": lease.fence}));
        let authority = Value::Array(authority);
        let result = (|| {
            let prepare_started = Instant::now();
            transfer_event(
                "preparing",
                json!({"destinationExecutorId": destination_executor, "archiveSize": archive_size}),
            );
            self.call_registered_executor(
                destination_executor,
                "artifact.relay.archive.prepare",
                json!({"destination": destination_path, "staging": staging, "_workspaceSessionId": workspace_session_id, "_authority": authority}),
            )
            .map_err(|error| transfer_error(error, "preparing", 0, 0))?;
            transfer_event(
                "prepared",
                json!({"durationMs": prepare_started.elapsed().as_millis()}),
            );
            let mut offset = 0_u64;
            let mut chunks = 0_u64;
            let mut encoded_bytes = 0_u64;
            let relay_started = Instant::now();
            transfer_event(
                "relay-started",
                json!({"chunkSize": 8 * 1024 * 1024, "archiveSize": archive_size}),
            );
            while offset < archive_size {
                let chunk = self
                    .call_registered_executor(
                        source_executor,
                        "artifact.relay.archive.read",
                        json!({"token": token, "offset": offset, "limit": 8 * 1024 * 1024, "_workspaceSessionId": workspace_session_id, "_authority": driver_authority}),
                    )
                    .map_err(|error| transfer_error(error, "reading", offset, chunks))?;
                let bytes = chunk.get("bytes").and_then(Value::as_u64).ok_or_else(|| {
                    RpcError::new("INVALID_ARTIFACT", "archive chunk has no byte count")
                })?;
                if bytes == 0 {
                    return Err(transfer_error(
                        RpcError::new(
                            "ARTIFACT_TRANSFER_FAILED",
                            "unexpected EOF while reading archive",
                        ),
                        "reading",
                        offset,
                        chunks,
                    ));
                }
                chunks += 1;
                encoded_bytes += chunk
                    .get("data")
                    .and_then(Value::as_str)
                    .map(|data| data.len() as u64)
                    .unwrap_or(0);
                self.call_registered_executor(
                    destination_executor,
                    "artifact.relay.archive.write",
                    json!({"destination": destination_path, "staging": staging, "offset": offset, "data": chunk["data"], "_workspaceSessionId": workspace_session_id, "_authority": authority}),
                )
                .map_err(|error| transfer_error(error, "writing", offset, chunks))?;
                offset = offset.saturating_add(bytes);
                transfer_event(
                    "chunk-transferred",
                    json!({"chunk": chunks, "bytes": bytes, "transferredBytes": offset.min(archive_size), "archiveSize": archive_size}),
                );
            }
            let relay_duration_ms = relay_started.elapsed().as_millis();
            transfer_event(
                "relay-completed",
                json!({"chunks": chunks, "transferredBytes": offset, "encodedBytes": encoded_bytes, "durationMs": relay_duration_ms}),
            );
            let commit_started = Instant::now();
            transfer_event("verifying", json!({"expectedDigest": expected_digest}));
            let committed = self
                .call_registered_executor(
                    destination_executor,
                    "artifact.relay.archive.commit",
                    json!({
                        "destination": destination_path, "staging": staging,
                        "expectedDigest": expected_digest, "archiveSize": archive_size,
                        "size": size, "files": files, "kind": kind, "_workspaceSessionId": workspace_session_id, "_authority": authority
                    }),
                )
                .map_err(|error| transfer_error(error, "committing", offset, chunks))?;
            let commit_duration_ms = commit_started.elapsed().as_millis();
            transfer_event(
                "committed",
                json!({"durationMs": commit_duration_ms, "digest": committed.get("digest")}),
            );
            Ok(json!({
                "source": {"executorId": source_executor},
                "destination": {"executorId": destination_executor, "path": destination_path},
                "mode": mode, "transport": "archive", "compression": "gzip",
                "archiveSize": archive_size,
                "digest": committed["digest"], "size": committed["size"], "files": committed["files"],
                "transfer": {
                    "rawBytes": size, "archiveBytes": archive_size, "transferredBytes": offset,
                    "encodedBytes": encoded_bytes, "chunks": chunks, "chunkSize": 8 * 1024 * 1024,
                    "retryCount": 0,
                    "relayDurationMs": relay_duration_ms, "commitDurationMs": commit_duration_ms,
                    "throughputBytesPerSecond": if relay_duration_ms == 0 { archive_size * 1000 } else { archive_size.saturating_mul(1000) / relay_duration_ms as u64 }
                }
            }))
        })();
        let _ = self
            .leases
            .lock()
            .expect("lease lock")
            .release(&resource, &owner, &lease.token);
        self.persist()?;
        result
    }

    fn route_session_action(
        &self,
        action: &str,
        params: &Value,
    ) -> Result<Option<Value>, RpcError> {
        if matches!(action, "session.put" | "session.accept-handoff") {
            return Ok(None);
        }
        let session_id = self.session_id_for_action(action, params);
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let authority_id = self
            .state
            .lock()
            .expect("state lock")
            .sessions
            .iter()
            .find(|session| session.metadata.id == session_id)
            .and_then(|session| session.authority.as_ref())
            .map(|authority| authority.controller_id.clone());
        let pending = self
            .state
            .lock()
            .expect("state lock")
            .sessions
            .iter()
            .find(|session| session.metadata.id == session_id)
            .and_then(|session| session.authority.as_ref())
            .and_then(|authority| authority.pending_controller_id.clone());
        if let Some(target) = pending
            && !matches!(action, "session.get" | "session.handoff")
        {
            return Err(RpcError::new(
                "SESSION_HANDOFF_IN_PROGRESS",
                format!("session handoff to {target} is in progress"),
            ));
        }
        let Some(authority_id) = authority_id else {
            return Ok(None);
        };
        if authority_id == self.id {
            return Ok(None);
        }
        let controller = self
            .state
            .lock()
            .expect("state lock")
            .controllers
            .iter()
            .find(|controller| controller.metadata.id == authority_id)
            .cloned()
            .ok_or_else(|| {
                RpcError::new(
                    "SESSION_AUTHORITY_UNAVAILABLE",
                    format!("home controller {authority_id} is not registered"),
                )
            })?;
        let response = call_executor(
            &controller.endpoint,
            &traced_request(action, params.clone()),
        )
        .map_err(|error| RpcError::new("SESSION_AUTHORITY_UNAVAILABLE", error.to_string()))?;
        if response.ok {
            Ok(Some(response.result.unwrap_or(Value::Null)))
        } else {
            Err(response.error.unwrap_or_else(|| {
                RpcError::new(
                    "SESSION_AUTHORITY_FAILED",
                    "home controller rejected request",
                )
            }))
        }
    }

    fn session_gate(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut gates = self.session_gates.lock().expect("session gates lock");
        Arc::clone(
            gates
                .entry(session_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    fn workspace_is_draining(&self, workspace: &str, owner: &str) -> bool {
        let now = now_ms();
        self.state
            .lock()
            .expect("state lock")
            .driver_handoff_requests
            .iter()
            .any(|request| {
                request.workspace_session_id == workspace
                    && request.previous_owner.as_deref() == Some(owner)
                    && request.expires_at > now
                    && matches!(
                        request.state,
                        DriverHandoffState::Requested
                            | DriverHandoffState::Draining
                            | DriverHandoffState::Ready
                    )
            })
    }

    fn refresh_driver_handoff(&self, workspace: &str) -> Result<(), RpcError> {
        let now = now_ms();
        let task_busy = self
            .tasks
            .lock()
            .expect("task lock")
            .snapshot()
            .iter()
            .any(|task| {
                task.workspace_session_id == workspace
                    && matches!(
                        task.state,
                        TaskState::Queued | TaskState::Running | TaskState::Cancelling
                    )
            });
        let mut changed = false;
        let mut state = self.state.lock().expect("state lock");
        let busy = task_busy
            || state
                .agent_mutations
                .iter()
                .any(|operation| operation.workspace_session_id == workspace);
        for request in state
            .driver_handoff_requests
            .iter_mut()
            .filter(|item| item.workspace_session_id == workspace)
        {
            if matches!(
                request.state,
                DriverHandoffState::Requested
                    | DriverHandoffState::Draining
                    | DriverHandoffState::Ready
            ) {
                if request.expires_at <= now {
                    request.state = DriverHandoffState::Expired;
                    if request.previous_owner.is_some() {
                        let _ = self
                            .leases
                            .lock()
                            .expect("lease lock")
                            .cancel_handoff(&request.resource, &request.requested_by);
                    }
                    changed = true;
                } else if !busy && request.state != DriverHandoffState::Ready {
                    request.state = DriverHandoffState::Ready;
                    changed = true;
                }
            }
        }
        drop(state);
        if changed {
            self.persist()?;
        }
        Ok(())
    }

    fn requires_session_gate(&self, action: &str) -> bool {
        // Capability execution is already fenced by driver/resource leases.
        // Holding the session gate while waiting for a long build prevents
        // renew, status, task cancellation, and a compensating process.stop.
        !matches!(
            action,
            "capability.invoke"
                | "status"
                | "doctor"
                | "task.get"
                | "task.list"
                | "task.events"
                | "task.wait"
                | "task.cancel"
                | "task.retry"
                | "driver.status"
                | "driver.renew"
                | "lease.status"
                | "lease.renew"
        )
    }

    fn session_id_for_action(&self, action: &str, params: &Value) -> Option<String> {
        let mut session_id = params
            .get("workspaceSessionId")
            .or_else(|| params.get("sessionId"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if session_id.is_none() && action == "session.put" {
            session_id = params
                .pointer("/metadata/id")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if session_id.is_none() && action == "session.accept-handoff" {
            session_id = params
                .pointer("/session/metadata/id")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if session_id.is_none() && action == "artifact.put" {
            session_id = params
                .pointer("/provenance/workspaceSessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if session_id.is_none() && (action.starts_with("driver.") || action.starts_with("lease.")) {
            session_id = params
                .get("resource")
                .and_then(Value::as_str)
                .and_then(|resource| resource.strip_prefix("workspace:"))
                .map(str::to_owned);
        }
        if session_id.is_none() {
            let state = self.state.lock().expect("state lock");
            session_id = match action {
                name if name.starts_with("agent.") => params
                    .get("agentId")
                    .and_then(Value::as_str)
                    .and_then(|id| state.agents.iter().find(|agent| agent.id == id))
                    .map(|agent| agent.workspace_session_id.clone()),
                name if name.starts_with("task.") => params
                    .get("taskId")
                    .and_then(Value::as_str)
                    .and_then(|id| state.tasks.iter().find(|task| task.id == id))
                    .map(|task| task.workspace_session_id.clone()),
                name if name.starts_with("transaction.") => params
                    .get("transactionId")
                    .and_then(Value::as_str)
                    .and_then(|id| {
                        state
                            .transactions
                            .iter()
                            .find(|transaction| transaction.id == id)
                    })
                    .map(|transaction| transaction.workspace_session_id.clone()),
                name if name.starts_with("handoff.") => params
                    .get("handoffId")
                    .and_then(Value::as_str)
                    .and_then(|id| state.handoffs.iter().find(|handoff| handoff.id == id))
                    .map(|handoff| handoff.workspace_session_id.clone()),
                _ => None,
            };
        }
        session_id
    }

    fn session_bundle(&self, session: &WorkspaceSession) -> Value {
        let session_id = &session.metadata.id;
        let state = self.state.lock().expect("state lock");
        json!({
            "session": session,
            "tasks": self.tasks.lock().expect("task lock").snapshot().into_iter()
                .filter(|task| &task.workspace_session_id == session_id).collect::<Vec<_>>(),
            "artifacts": state.artifacts.iter()
                .filter(|artifact| &artifact.provenance.workspace_session_id == session_id).cloned().collect::<Vec<_>>(),
            "generations": state.generations.iter()
                .filter(|generation| &generation.workspace_session_id == session_id).cloned().collect::<Vec<_>>(),
            "transactions": state.transactions.iter()
                .filter(|transaction| &transaction.workspace_session_id == session_id).cloned().collect::<Vec<_>>(),
            "agents": state.agents.iter()
                .filter(|agent| &agent.workspace_session_id == session_id).cloned().collect::<Vec<_>>(),
            "handoffs": state.handoffs.iter()
                .filter(|handoff| &handoff.workspace_session_id == session_id).cloned().collect::<Vec<_>>(),
        })
    }

    fn persist(&self) -> Result<(), RpcError> {
        let mut state = self.state.lock().expect("state lock");
        let leases = self.leases.lock().expect("lease lock");
        state.leases = leases.persistence_snapshot();
        state.lease_fences = leases.fence_snapshot();
        drop(leases);
        state.tasks = self.tasks.lock().expect("task lock").persistence_snapshot();
        self.store
            .save(&state)
            .map_err(|error| RpcError::new("STATE_WRITE_FAILED", error.to_string()))
    }
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RpcError::new("INVALID_PARAMS", format!("{key} is required")))
}

fn optional_str(params: &Value, key: &str) -> Result<Option<String>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(RpcError::new(
            "INVALID_PARAMS",
            format!("{key} must be a string"),
        )),
    }
}

fn observability_rpc_error(error: workbench_core::ObservabilityError) -> RpcError {
    let code = match error {
        workbench_core::ObservabilityError::RunNotFound(_) => "RUN_NOT_FOUND",
        workbench_core::ObservabilityError::InvalidTransition(_) => "INVALID_RUN_TRANSITION",
        _ => "OBSERVABILITY_STORE_FAILED",
    };
    RpcError::new(code, error.to_string())
}

fn safe_observation_attributes(attributes: &Value) -> Value {
    const ALLOWED: &[&str] = &[
        "capability",
        "component",
        "stage",
        "targetNode",
        "targetRepo",
        "errorCode",
        "artifactDigest",
        "approvalId",
        "action",
        "workspaceSessionId",
        "agentId",
        "blockerId",
        "blockerKind",
        "blockerReason",
        "waitingOn",
    ];
    let Some(object) = attributes.as_object() else {
        return json!({});
    };
    Value::Object(
        object
            .iter()
            .filter(|(key, _)| ALLOWED.contains(&key.as_str()))
            .map(|(key, value)| {
                let safe = match value {
                    Value::String(text) => Value::String(text.chars().take(500).collect()),
                    Value::Bool(_) | Value::Number(_) | Value::Null => value.clone(),
                    _ => Value::Null,
                };
                (key.clone(), safe)
            })
            .collect(),
    )
}

fn required_operator_action(params: &Value) -> Result<&str, RpcError> {
    let action = required_str(params, "action")?;
    if matches!(
        action,
        "cancel-task"
            | "stop-process"
            | "retry-task"
            | "restart-process"
            | "driver-acquire"
            | "driver-handoff"
            | "driver-release"
            | "approval-approve"
            | "approval-revoke"
    ) {
        Ok(action)
    } else {
        Err(RpcError::new(
            "OPERATOR_ACTION_NOT_ALLOWED",
            "operator action is not in the fixed allowlist",
        ))
    }
}
fn required_force_action(params: &Value) -> Result<&str, RpcError> {
    let action = required_operator_action(params)?;
    if matches!(action, "cancel-task" | "stop-process") {
        Ok(action)
    } else {
        Err(RpcError::new(
            "OPERATOR_ACTION_NOT_ALLOWED",
            "only cancel-task and stop-process support force actions",
        ))
    }
}
fn safe_summary(value: &Value) -> Value {
    json!({"id":value.get("id").or_else(||value.get("taskId")).cloned(),"state":value.get("state").cloned(),"status":value.get("status").cloned()})
}
fn lifecycle_observation_kind(action: &str) -> Option<&'static str> {
    if action.starts_with("driver.") || action.starts_with("lease.") {
        Some("lease")
    } else if action.starts_with("approval.") {
        Some("approval")
    } else if action.starts_with("artifact.") {
        Some("artifact")
    } else if action.starts_with("generation.") {
        Some("generation")
    } else if action.starts_with("handoff.") || action.starts_with("session.handoff") {
        Some("handoff")
    } else if action.starts_with("agent.") {
        Some("agent")
    } else {
        None
    }
}
fn safe_lifecycle_attributes(params: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for key in [
        "taskId",
        "processId",
        "approvalId",
        "generationId",
        "agentId",
        "executorId",
        "workspaceSessionId",
        "targetControllerId",
        "digest",
    ] {
        if let Some(value) = params.get(key)
            && (value.is_string() || value.is_number())
        {
            result.insert(key.into(), value.clone());
        }
    }
    Value::Object(result)
}

fn safe_request_fields(action: &str, params: &Value) -> Value {
    let mut fields = serde_json::Map::new();
    for key in [
        "executorId",
        "capability",
        "workspaceSessionId",
        "sessionId",
        "taskId",
        "processId",
        "executionMode",
    ] {
        if let Some(value) = params.get(key)
            && (value.is_string() || value.is_number() || value.is_boolean())
        {
            fields.insert(key.into(), value.clone());
        }
    }
    if action == "capability.invoke" {
        let input = params.get("input").unwrap_or(&Value::Null);
        fields.insert(
            "inputSummary".into(),
            json!({
                "keys": input.as_object().map(|object| object.keys().take(32).cloned().collect::<Vec<_>>()).unwrap_or_default(),
                "encodedBytes": serde_json::to_vec(input).map(|bytes| bytes.len()).unwrap_or(0),
            }),
        );
    }
    Value::Object(fields)
}

fn optional_array<T: serde::de::DeserializeOwned>(
    params: &Value,
    key: &str,
) -> Result<Vec<T>, RpcError> {
    params
        .get(key)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| RpcError::new("INVALID_PARAMS", format!("{key}: {error}")))
        .map(Option::unwrap_or_default)
}

fn validate_schema(schema: &Value, value: &Value, path: &str) -> Result<(), RpcError> {
    if schema.as_object().is_none_or(|object| object.is_empty()) {
        return Ok(());
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(schema_error(path, "value is not in enum"));
    }
    if let Some(types) = schema.get("type") {
        let matches = match types {
            Value::String(kind) => value_matches_type(value, kind),
            Value::Array(kinds) => kinds
                .iter()
                .filter_map(Value::as_str)
                .any(|kind| value_matches_type(value, kind)),
            _ => false,
        };
        if !matches {
            return Err(schema_error(path, &format!("does not match type {types}")));
        }
    }
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && value.as_f64().is_some_and(|number| number < minimum)
    {
        return Err(schema_error(path, &format!("is below minimum {minimum}")));
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
        && value.as_f64().is_some_and(|number| number > maximum)
    {
        return Err(schema_error(path, &format!("is above maximum {maximum}")));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(schema_error(&format!("{path}.{key}"), "is required"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, child) in object {
                if let Some(child_schema) = properties.get(key) {
                    validate_schema(child_schema, child, &format!("{path}.{key}"))?;
                } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                    return Err(schema_error(&format!("{path}.{key}"), "is not allowed"));
                }
            }
        }
    }
    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for (index, child) in values.iter().enumerate() {
            validate_schema(items, child, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

fn value_matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn retained_task_output(capability: &str, result: &Value) -> Value {
    if capability != "ui.native-inspect" {
        return result.clone();
    }
    let processes = result
        .pointer("/inspection/processes")
        .and_then(Value::as_array)
        .map(|processes| {
            processes
                .iter()
                .map(|process| {
                    let windows = process
                        .get("windows")
                        .and_then(Value::as_array)
                        .map(|windows| {
                            windows
                                .iter()
                                .map(|window| {
                                    let mut accessible_names = Vec::new();
                                    collect_native_accessible_names(
                                        window,
                                        &mut accessible_names,
                                        128,
                                    );
                                    json!({
                                        "role": window.get("role").cloned().unwrap_or(Value::Null),
                                        "title": window.get("title").cloned().unwrap_or(Value::Null),
                                        "value": window.get("value").cloned().unwrap_or(Value::Null),
                                        "description": window.get("description").cloned().unwrap_or(Value::Null),
                                        "accessibleNames": accessible_names,
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    json!({
                        "pid": process.get("pid").cloned().unwrap_or(Value::Null),
                        "windows": windows,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "applicationPath": result.get("applicationPath").cloned().unwrap_or(Value::Null),
        "pids": result.get("pids").cloned().unwrap_or_else(|| json!([])),
        "inspection": {
            "accessibilityTrusted": result.pointer("/inspection/accessibilityTrusted")
                .cloned().unwrap_or(Value::Bool(false)),
            "processCount": result.pointer("/inspection/processes")
                .and_then(Value::as_array).map_or(0, Vec::len),
            "processes": processes,
        },
        "inspectedAt": result.get("inspectedAt").cloned().unwrap_or(Value::Null),
    })
}

fn collect_native_accessible_names(value: &Value, names: &mut Vec<String>, limit: usize) {
    if names.len() >= limit {
        return;
    }
    if let Some(role) = value.get("role").and_then(Value::as_str)
        && matches!(
            role,
            "AXMenuBar" | "AXMenuBarItem" | "AXMenu" | "AXMenuItem"
        )
    {
        return;
    }
    for key in ["title", "value", "description"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() && !names.iter().any(|existing| existing == text) {
                names.push(text.to_owned());
                if names.len() >= limit {
                    return;
                }
            }
        }
    }
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            collect_native_accessible_names(child, names, limit);
            if names.len() >= limit {
                return;
            }
        }
    }
}

fn compact_task_outputs(tasks: &mut [Task]) -> bool {
    let mut changed = false;
    for task in tasks {
        if task.capability == "ui.native-inspect"
            && let Some(output) = task.output.as_ref()
        {
            let compact = retained_task_output(&task.capability, output);
            if &compact != output {
                task.output = Some(compact);
                changed = true;
            }
        }
    }
    changed
}

fn compact_persisted_evidence(state: &mut WorkbenchState) -> bool {
    let mut changed = compact_task_outputs(&mut state.tasks);
    for task in &mut state.tasks {
        changed |= compact_value(&mut task.input);
        if let Some(output) = &mut task.output {
            changed |= compact_value(output);
        }
        if let Some(error) = &mut task.error {
            changed |= compact_value(&mut error.details);
        }
    }
    for transaction in &mut state.transactions {
        for entry in &mut transaction.journal {
            changed |= compact_value(&mut entry.details);
        }
        if let Some(error) = &mut transaction.error {
            changed |= compact_value(&mut error.details);
        }
    }
    for generation in &mut state.generations {
        for evidence in [
            &mut generation.validation,
            &mut generation.finalization,
            &mut generation.smoke,
        ]
        .into_iter()
        .flatten()
        {
            changed |= compact_value(&mut evidence.details);
        }
        if let Some(error) = &mut generation.failure {
            changed |= compact_value(&mut error.details);
        }
    }
    for handoff in &mut state.handoffs {
        for evidence in &mut handoff.evidence {
            changed |= compact_value(evidence);
        }
    }
    for agent in &mut state.agents {
        changed |= compact_value(&mut agent.metadata);
    }
    changed
}

fn compact_value(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(object) => {
            if let Some(evaluations) = object.remove("nativeEvaluations") {
                let count = evaluations.as_array().map_or(0, Vec::len);
                object.insert("nativeObservationCount".to_owned(), json!(count));
                changed = true;
            }
            for child in object.values_mut() {
                changed |= compact_value(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                changed |= compact_value(child);
            }
        }
        _ => {}
    }
    changed
}

fn schema_error(path: &str, message: &str) -> RpcError {
    RpcError::new("SCHEMA_VALIDATION_FAILED", format!("{path} {message}"))
}

fn artifact_from_result(
    capability: &str,
    input: &Value,
    result: &Value,
    workspace_session_id: &str,
    executor_id: &str,
    task_id: &str,
) -> Option<Artifact> {
    let value = match capability {
        "artifact.build" => result.get("artifact")?,
        "artifact.describe" | "artifact.transfer" => result,
        _ => return None,
    };
    let digest = value.get("digest")?.as_str()?.to_owned();
    let size = value.get("size").and_then(Value::as_u64).unwrap_or(0);
    let path = value
        .get("path")
        .or_else(|| value.get("destination"))?
        .as_str()?
        .to_owned();
    let artifact_type = input
        .get("artifactType")
        .and_then(Value::as_str)
        .unwrap_or("generic")
        .to_owned();
    let source_digests = input
        .get("digest")
        .and_then(Value::as_str)
        .map(|digest| vec![digest.to_owned()])
        .unwrap_or_default();
    Some(Artifact {
        digest,
        artifact_type,
        schema: "workbench.dev/artifact/v1".to_owned(),
        size,
        locations: vec![ArtifactLocation::File {
            executor_id: executor_id.to_owned(),
            path,
        }],
        provenance: Provenance {
            workspace_session_id: workspace_session_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            source_digests,
            attributes: Default::default(),
        },
        created_at: now_ms(),
    })
}

fn generation_transition_allowed(from: &GenerationState, to: &GenerationState) -> bool {
    from == to
        || matches!(
            (from, to),
            (GenerationState::Reserved, GenerationState::Materializing)
                | (GenerationState::Reserved, GenerationState::Failed)
                | (
                    GenerationState::Materializing,
                    GenerationState::Materialized
                )
                | (GenerationState::Materializing, GenerationState::Failed)
                | (GenerationState::Materialized, GenerationState::Validated)
                | (GenerationState::Materialized, GenerationState::Failed)
                | (GenerationState::Validated, GenerationState::Finalized)
                | (GenerationState::Validated, GenerationState::Failed)
                | (GenerationState::Finalized, GenerationState::SmokePassed)
                | (GenerationState::Finalized, GenerationState::Failed)
                | (GenerationState::SmokePassed, GenerationState::Active)
                | (GenerationState::SmokePassed, GenerationState::Failed)
                | (GenerationState::Active, GenerationState::Superseded)
        )
}

fn upsert_by<T>(items: &mut Vec<T>, value: T, predicate: impl Fn(&T) -> bool) {
    if let Some(existing) = items.iter_mut().find(|item| predicate(item)) {
        *existing = value;
    } else {
        items.push(value);
    }
}

fn render_lock(template: &str, input: &Value) -> Result<String, RpcError> {
    let mut rendered = template.to_owned();
    while let Some(start) = rendered.find("${") {
        let relative_end = rendered[start + 2..]
            .find('}')
            .ok_or_else(|| RpcError::new("INVALID_LOCK_TEMPLATE", template))?;
        let end = start + 2 + relative_end;
        let key = &rendered[start + 2..end];
        let value = input.get(key).ok_or_else(|| {
            RpcError::new(
                "INVALID_LOCK_INPUT",
                format!("lock requires input field {key}"),
            )
        })?;
        let value = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        rendered.replace_range(start..=end, &value);
    }
    Ok(rendered)
}

fn command_resources(input: &Value) -> Result<Vec<String>, RpcError> {
    let Some(values) = input.get("resources") else {
        return Ok(Vec::new());
    };
    let values = values
        .as_array()
        .ok_or_else(|| RpcError::new("INVALID_PARAMS", "command resources must be an array"))?;
    if values.len() > 32 {
        return Err(RpcError::new(
            "INVALID_PARAMS",
            "command resources may contain at most 32 entries",
        ));
    }
    values
        .iter()
        .map(|value| {
            let resource = value.as_str().ok_or_else(|| {
                RpcError::new("INVALID_PARAMS", "command resources must be strings")
            })?;
            if resource.is_empty() || resource.len() > 512 || resource.chars().any(char::is_control)
            {
                return Err(RpcError::new(
                    "INVALID_PARAMS",
                    "command resource is empty, too long, or contains control characters",
                ));
            }
            Ok(format!("command:{resource}"))
        })
        .collect()
}

fn wait_for_process_readiness(
    endpoint: &ExecutorEndpoint,
    input: &Value,
    result: Value,
) -> Result<Value, RpcError> {
    let process_id = required_str(input, "processId")?;
    let timeout_ms = input
        .get("readinessTimeoutMs")
        .and_then(Value::as_u64)
        .or_else(|| {
            input
                .get("readiness")
                .and_then(|value| value.get("timeoutMs"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(180_000);
    let readiness = result
        .get("readiness")
        .and_then(|value| value.get("state"))
        .and_then(Value::as_str);
    if readiness == Some("ready") {
        return Ok(result);
    }
    // Readiness progression and layered failure detection are owned by the
    // Executor. The Controller sends one subscription-style wait request; it
    // never loops process.get across the peer route.
    let waited = call_executor(
        endpoint,
        &traced_request(
            "readiness.wait",
            json!({"processId": process_id, "timeoutMs": timeout_ms}),
        ),
    )
    .map_err(|error| {
        let mut value = RpcError::new("EXECUTOR_DISCONNECTED", error.to_string());
        value.retryable = true;
        value
    })?;
    if !waited.ok {
        return Err(waited.error.unwrap_or_else(|| {
            RpcError::new(
                "INVALID_EXECUTOR_RESPONSE",
                "readiness.wait returned no result",
            )
        }));
    }
    waited.result.ok_or_else(|| {
        RpcError::new(
            "INVALID_EXECUTOR_RESPONSE",
            "readiness.wait returned an empty result",
        )
    })
}

fn wait_for_tunnel_readiness(
    endpoint: &ExecutorEndpoint,
    input: &Value,
    result: Value,
) -> Result<Value, RpcError> {
    if result.get("observedState").and_then(Value::as_str) == Some("ready") {
        return Ok(result);
    }
    let tunnel_id = required_str(input, "tunnelId")?;
    let process_id = format!("tunnel-{tunnel_id}");
    let timeout_ms = input
        .get("readinessTimeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(8_000)
        .min(8_000);
    let waited = call_executor(
        endpoint,
        &traced_request(
            "readiness.wait",
            json!({"processId": process_id, "timeoutMs": timeout_ms}),
        ),
    );
    let current = call_executor(
        endpoint,
        &traced_request("tunnel.get", json!({"tunnelId": tunnel_id})),
    )
    .map_err(|error| RpcError::new("EXECUTOR_DISCONNECTED", error.to_string()))?;
    if current.ok
        && let Some(mut value) = current.result
    {
        if value.get("observedState").and_then(Value::as_str) == Some("ready") {
            return Ok(value);
        }
        if waited.as_ref().is_ok_and(|response| response.ok)
            && value.get("state").and_then(Value::as_str) == Some("running")
        {
            value["observedState"] = json!("ready");
            return Ok(value);
        }
        if let Ok(response) = &waited
            && !response.ok
            && let Some(error) = &response.error
            && error.retryable
        {
            value["observedState"] = json!("pending");
            value["pendingReason"] = json!(error.message);
            value["retryable"] = json!(true);
            return Ok(value);
        }
    }
    match waited {
        Ok(response) if !response.ok => Err(response
            .error
            .unwrap_or_else(|| RpcError::new("TUNNEL_NOT_READY", "tunnel readiness failed"))),
        Ok(_) => Err(RpcError::new(
            "TUNNEL_NOT_READY",
            "tunnel did not become ready",
        )),
        Err(error) => {
            let mut value = RpcError::new("EXECUTOR_DISCONNECTED", error.to_string());
            value.retryable = true;
            Err(value)
        }
    }
}

fn command_requires_approval(input: &Value) -> bool {
    input
        .get("argv")
        .and_then(Value::as_array)
        .and_then(|argv| argv.first())
        .and_then(Value::as_str)
        .and_then(|path| std::path::Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "rm" | "sudo" | "su" | "dd" | "mkfs"))
}

fn command_digest(input: &Value) -> Result<String, RpcError> {
    let canonical = json!({
        "cwd": input.get("cwd").ok_or_else(|| RpcError::new("INVALID_PARAMS", "cwd is required"))?,
        "argv": input.get("argv").ok_or_else(|| RpcError::new("INVALID_PARAMS", "argv is required"))?,
    });
    serde_json::to_vec(&canonical)
        .map(|bytes| sha256_bytes(&bytes))
        .map_err(|error| RpcError::new("INVALID_PARAMS", error.to_string()))
}

fn release_acquired(leases: &mut LeaseTable, acquired: &[workbench_schema::Lease]) {
    for lease in acquired.iter().rev() {
        let _ = leases.release(&lease.resource, &lease.owner, &lease.token);
    }
}

fn lease_order_rank(resource: &str) -> u8 {
    if resource.starts_with("workspace:") {
        1
    } else if resource.starts_with("runtime:") {
        2
    } else if resource.starts_with("acceptance:") {
        3
    } else {
        0
    }
}

fn protocol_features() -> [&'static str; 4] {
    [
        "capability-authority-v2",
        "driver-draining-v1",
        "read-grant-v1",
        "acceptance-handoff-v1",
    ]
}

fn render_capability_authority_resource(
    template: &str,
    executor_id: &str,
    input: &Value,
) -> Result<String, RpcError> {
    let expanded = template.replace("${executorId}", executor_id);
    render_lock(&expanded, input)
}

fn map_lease_error(error: LeaseError) -> RpcError {
    let code = match error {
        LeaseError::Active { .. } => "LEASE_ACTIVE",
        LeaseError::NotFound => "LEASE_NOT_FOUND",
        LeaseError::OwnedByOther => "LEASE_OWNED_BY_OTHER",
        LeaseError::InvalidHandoff => "INVALID_HANDOFF",
    };
    RpcError::new(code, error.to_string())
}

fn executor_execution_fence(lease_fence: u64) -> u64 {
    // Executor resources can be shared by many workspace sessions. A
    // session-local epoch is therefore not a global ordering and can make a
    // newer writer look stale after another session used the same resource.
    // Anchor the fence in wall-clock milliseconds and retain the low lease
    // sequence bits for multiple acquisitions in the same millisecond.
    now_ms()
        .saturating_mul(1_u64 << 16)
        .saturating_add(lease_fence.min(u16::MAX as u64))
}

#[cfg(unix)]
fn refresh_local_endpoint_health(controllers: &mut [ControllerPeer], executors: &mut [Executor]) {
    for controller in controllers {
        if let ExecutorEndpoint::Local { socket } = &controller.endpoint {
            controller.health = if std::path::Path::new(socket).exists() {
                HealthStatus::Ready
            } else {
                HealthStatus::Offline
            };
        }
    }
    for executor in executors {
        if let ExecutorEndpoint::Local { socket } = &executor.endpoint {
            executor.health = if std::path::Path::new(socket).exists() {
                HealthStatus::Ready
            } else {
                HealthStatus::Offline
            };
        }
    }
}

#[cfg(not(unix))]
fn refresh_local_endpoint_health(_controllers: &mut [ControllerPeer], _executors: &mut [Executor]) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RpcServer;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use workbench_protocol::Request;

    #[test]
    fn capability_start_log_fields_are_useful_without_recording_input_values() {
        let fields = safe_request_fields(
            "capability.invoke",
            &json!({
                "executorId":"node-a",
                "capability":"artifact.relay",
                "workspaceSessionId":"session-a",
                "input":{"source":"/secret/path","token":"do-not-log"}
            }),
        );
        assert_eq!(fields["executorId"], "node-a");
        assert_eq!(fields["capability"], "artifact.relay");
        assert_eq!(fields["inputSummary"]["keys"].as_array().unwrap().len(), 2);
        assert!(!fields.to_string().contains("do-not-log"));
        assert!(!fields.to_string().contains("/secret/path"));
    }

    #[test]
    fn unregister_removes_persisted_peer_registrations_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let store = JsonStore::new(directory.path().join("controller.json"));
        let controller = Controller::open_with_id(store.clone(), Some("local".to_owned())).unwrap();
        let now = now_ms();
        controller
            .state
            .lock()
            .unwrap()
            .controllers
            .push(ControllerPeer {
                api_version: "workbench.dev/v1".to_owned(),
                metadata: Metadata {
                    id: "stale".to_owned(),
                    labels: Default::default(),
                    created_at: now,
                    updated_at: now,
                },
                endpoint: ExecutorEndpoint::Local {
                    socket: "missing-controller.sock".to_owned(),
                },
                health: HealthStatus::Offline,
            });
        controller.state.lock().unwrap().executors.push(Executor {
            api_version: "workbench.dev/v1".to_owned(),
            metadata: Metadata {
                id: "stale-rust".to_owned(),
                labels: Default::default(),
                created_at: now,
                updated_at: now,
            },
            endpoint: ExecutorEndpoint::Local {
                socket: "missing-executor.sock".to_owned(),
            },
            capabilities: Vec::new(),
            allowed_roots: Vec::new(),
            health: HealthStatus::Offline,
        });
        controller.persist().unwrap();
        assert!(
            controller
                .handle(Request::new(
                    "controller.unregister",
                    json!({"controllerId": "stale"})
                ))
                .ok
        );
        assert!(
            controller
                .handle(Request::new(
                    "executor.unregister",
                    json!({"executorId": "stale-rust"})
                ))
                .ok
        );
        let reopened = Controller::open_with_id(store, Some("local".to_owned())).unwrap();
        assert!(reopened.state.lock().unwrap().controllers.is_empty());
        assert!(reopened.state.lock().unwrap().executors.is_empty());
        assert_eq!(
            reopened
                .handle(Request::new(
                    "controller.unregister",
                    json!({"controllerId": "stale"})
                ))
                .result
                .unwrap()["removed"],
            false
        );
    }

    #[cfg(unix)]
    #[test]
    fn status_marks_missing_peer_socket_offline_without_forgetting_registration() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("controller.json"))).unwrap();
        controller.state.lock().unwrap().executors.push(Executor {
            api_version: "workbench.dev/v1".to_owned(),
            metadata: Metadata {
                id: "peer-rust".to_owned(),
                labels: Default::default(),
                created_at: 1,
                updated_at: 1,
            },
            endpoint: ExecutorEndpoint::Local {
                socket: directory
                    .path()
                    .join("disconnected-peer.sock")
                    .to_string_lossy()
                    .into_owned(),
            },
            capabilities: Vec::new(),
            allowed_roots: Vec::new(),
            health: HealthStatus::Ready,
        });
        let status = controller.handle(Request::new("status", Value::Null));
        assert!(status.ok, "{:?}", status.error);
        assert_eq!(status.result.unwrap()["executors"][0]["health"], "offline");
        assert_eq!(controller.state.lock().unwrap().executors.len(), 1);
    }

    #[test]
    fn readiness_wait_delegates_one_wait_to_the_executor() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("readiness.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(move |request| {
                    server_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(request.action, "readiness.wait");
                    Response::success(
                        request.request_id,
                        json!({
                            "id": "process-1", "state": "running",
                            "readiness": {"state": "ready", "attempts": 2}
                        }),
                    )
                })
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let endpoint: ExecutorEndpoint =
            serde_json::from_value(json!({"transport": "local", "socket": socket})).unwrap();
        let ready = wait_for_process_readiness(
            &endpoint,
            &json!({"processId": "process-1", "readinessTimeoutMs": 2_000}),
            json!({"id": "process-1", "state": "running", "readiness": {"state": "starting", "attempts": 0}}),
        ).unwrap();
        assert_eq!(ready["readiness"]["state"], "ready");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tunnel_wait_returns_visible_pending_state_after_short_retryable_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("tunnel-readiness.sock");
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(move |request| match request.action.as_str() {
                    "readiness.wait" => {
                        let mut error = RpcError::new("BUILD_STALLED", "tunnel probe timed out");
                        error.retryable = true;
                        Response::failure(request.request_id, error)
                    }
                    "tunnel.get" => Response::success(
                        request.request_id,
                        json!({"tunnelId": "signal", "observedState": "starting"}),
                    ),
                    other => panic!("unexpected action: {other}"),
                })
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let endpoint: ExecutorEndpoint =
            serde_json::from_value(json!({"transport": "local", "socket": socket})).unwrap();
        let pending = wait_for_tunnel_readiness(
            &endpoint,
            &json!({"tunnelId": "signal", "readinessTimeoutMs": 25}),
            json!({"tunnelId": "signal", "observedState": "starting"}),
        )
        .unwrap();
        assert_eq!(pending["observedState"], "pending");
        assert_eq!(pending["pendingReason"], "tunnel probe timed out");
        assert_eq!(pending["retryable"], true);
    }

    #[test]
    fn tunnel_wait_trusts_successful_readiness_over_a_transient_second_probe() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("tunnel-ready.sock");
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(move |request| match request.action.as_str() {
                    "readiness.wait" => Response::success(
                        request.request_id,
                        json!({"id": "tunnel-signal", "readiness": {"state": "ready"}}),
                    ),
                    "tunnel.get" => Response::success(
                        request.request_id,
                        json!({
                            "tunnelId": "signal",
                            "state": "running",
                            "observedState": "starting"
                        }),
                    ),
                    other => panic!("unexpected action: {other}"),
                })
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let endpoint: ExecutorEndpoint =
            serde_json::from_value(json!({"transport": "local", "socket": socket})).unwrap();
        let ready = wait_for_tunnel_readiness(
            &endpoint,
            &json!({"tunnelId": "signal", "readinessTimeoutMs": 2_000}),
            json!({"tunnelId": "signal", "state": "running", "observedState": "starting"}),
        )
        .unwrap();
        assert_eq!(ready["observedState"], "ready");
    }

    #[test]
    fn driver_ttl_is_not_silently_capped_at_five_minutes() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("controller.json"))).unwrap();
        let acquired = controller.handle(Request::new(
            "driver.take",
            json!({"resource": "workspace:test", "owner": "agent", "ttlMs": 900_000}),
        ));
        assert!(acquired.ok, "{:?}", acquired.error);
        let lease = acquired.result.unwrap();
        assert_eq!(
            lease["expiresAt"].as_u64().unwrap() - lease["acquiredAt"].as_u64().unwrap(),
            900_000
        );
        let renewed = controller.handle(Request::new(
            "driver.renew",
            json!({"resource": "workspace:test", "owner": "agent", "token": lease["token"], "ttlMs": 900_000}),
        ));
        let lease = renewed.result.unwrap();
        assert_eq!(
            lease["expiresAt"].as_u64().unwrap() - lease["updatedAt"].as_u64().unwrap(),
            900_000
        );
    }

    #[test]
    fn controller_relays_artifacts_between_executors_without_hostnames() {
        let directory = tempfile::tempdir().unwrap();
        let source_root = directory.path().join("source");
        let destination_root = directory.path().join("destination");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&destination_root).unwrap();
        std::fs::write(
            source_root.join("chunk.js"),
            vec![7_u8; 2 * 1024 * 1024 + 17],
        )
        .unwrap();
        // Keep the executor-owned relay scratch directory outside the artifact
        // itself, as it is in production where state lives beside the socket.
        let source_runtime = Arc::new(
            crate::ExecutorRuntime::open(
                "source",
                vec![directory.path().to_path_buf()],
                directory.path().join("source-fences.json"),
            )
            .unwrap(),
        );
        let destination_runtime = Arc::new(
            crate::ExecutorRuntime::open(
                "destination",
                vec![directory.path().to_path_buf()],
                directory.path().join("destination-fences.json"),
            )
            .unwrap(),
        );
        let source_socket = directory.path().join("source.sock");
        let destination_socket = directory.path().join("destination.sock");
        for (runtime, socket) in [
            (source_runtime, source_socket.clone()),
            (destination_runtime, destination_socket.clone()),
        ] {
            std::thread::spawn(move || {
                RpcServer::new(socket)
                    .serve(move |request| runtime.handle(request))
                    .unwrap()
            });
        }
        for _ in 0..100 {
            if source_socket.exists() && destination_socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let controller =
            Controller::open(JsonStore::new(directory.path().join("controller.json"))).unwrap();
        for (id, socket) in [
            ("source", source_socket),
            ("destination", destination_socket),
        ] {
            assert!(controller.handle(Request::new("executor.register", json!({"executorId": id, "endpoint": {"transport": "local", "socket": socket}}))).ok);
        }
        let driver = controller
            .handle(Request::new(
                "driver.acquire",
                json!({"resource": "workspace:relay-test", "owner": "relay-agent", "ttlMs": 60_000}),
            ))
            .result
            .unwrap();
        let driver_token = driver["token"].as_str().unwrap();
        let transferred = controller.handle(Request::new(
            "artifact.transfer",
            json!({
                "source": {"executorId": "source", "path": source_root},
                "destination": {"executorId": "destination", "path": destination_root.join("copy")},
                "mode": "mirror",
                "workspaceSessionId": "relay-test", "owner": "relay-agent", "driverToken": driver_token
            }),
        ));
        assert!(transferred.ok, "{:?}", transferred.error);
        let transferred = transferred.result.unwrap();
        assert_eq!(transferred["transport"], "archive");
        assert_eq!(transferred["compression"], "gzip");
        assert_eq!(
            transferred["transfer"]["transferredBytes"],
            transferred["archiveSize"]
        );
        assert!(transferred["transfer"]["chunks"].as_u64().unwrap() >= 1);
        assert!(
            transferred["transfer"]["encodedBytes"].as_u64().unwrap()
                >= transferred["archiveSize"].as_u64().unwrap()
        );
        assert!(transferred["transfer"].get("relayDurationMs").is_some());
        assert!(transferred["transfer"].get("commitDurationMs").is_some());
        assert!(transferred["transfer"].get("totalDurationMs").is_some());
        assert!(
            transferred["archiveSize"].as_u64().unwrap() < transferred["size"].as_u64().unwrap()
        );
        assert_eq!(
            std::fs::metadata(destination_root.join("copy/chunk.js"))
                .unwrap()
                .len(),
            2 * 1024 * 1024 + 17
        );
        let reverse = controller.handle(Request::new(
            "artifact.transfer",
            json!({
                "source": {"executorId": "destination", "path": destination_root.join("copy")},
                "destination": {"executorId": "source", "path": source_root.join("roundtrip")},
                "mode": "mirror",
                "workspaceSessionId": "relay-test", "owner": "relay-agent", "driverToken": driver_token
            }),
        ));
        assert!(reverse.ok, "{:?}", reverse.error);
        assert_eq!(
            std::fs::metadata(source_root.join("roundtrip/chunk.js"))
                .unwrap()
                .len(),
            2 * 1024 * 1024 + 17
        );
    }

    #[test]
    fn registers_and_calls_a_peer_controller_independently_of_transport_role() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("peer-controller.sock");
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(|request| {
                    Response::success(
                        request.request_id,
                        json!({"peer": "ready", "controller": {"id": "peer-b", "status": "ready"}}),
                    )
                })
                .unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let controller = Arc::new(
            Controller::open(JsonStore::new(directory.path().join("controller.json"))).unwrap(),
        );
        let mismatched = controller.handle(Request::new(
            "controller.register",
            json!({
                "controllerId": "wrong-peer",
                "endpoint": {"transport": "local", "socket": socket}
            }),
        ));
        assert_eq!(
            mismatched.error.unwrap().code,
            "CONTROLLER_IDENTITY_MISMATCH"
        );
        let registered = controller.handle(Request::new(
            "controller.register",
            json!({
                "controllerId": "peer-b",
                "endpoint": {"transport": "local", "socket": socket}
            }),
        ));
        assert!(registered.ok, "{:?}", registered.error);
        let called = controller.handle(Request::new(
            "controller.call",
            json!({"controllerId": "peer-b", "action": "ping", "params": {}}),
        ));
        assert!(called.ok, "{:?}", called.error);
        assert_eq!(called.result.unwrap()["peer"], "ready");
    }

    #[test]
    fn executor_registration_rejects_an_endpoint_with_another_identity() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("peer-executor.sock");
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(|request| {
                    Response::success(
                        request.request_id,
                        json!({
                            "executorId": "executor-b",
                            "status": "ready",
                            "capabilities": [],
                            "allowedRoots": []
                        }),
                    )
                })
                .unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let controller =
            Controller::open(JsonStore::new(directory.path().join("controller.json"))).unwrap();
        let mismatched = controller.handle(Request::new(
            "executor.register",
            json!({
                "executorId": "wrong-executor",
                "endpoint": {"transport": "local", "socket": socket}
            }),
        ));
        assert_eq!(mismatched.error.unwrap().code, "EXECUTOR_IDENTITY_MISMATCH");
        let registered = controller.handle(Request::new(
            "executor.register",
            json!({
                "executorId": "executor-b",
                "endpoint": {"transport": "local", "socket": socket}
            }),
        ));
        assert!(registered.ok, "{:?}", registered.error);
    }

    #[test]
    fn controller_identity_persists_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.json");
        let first =
            Controller::open_with_id(JsonStore::new(&path), Some("node-a".to_owned())).unwrap();
        assert_eq!(first.id, "node-a");
        drop(first);
        let reopened = Controller::open(JsonStore::new(path)).unwrap();
        assert_eq!(reopened.id, "node-a");
    }

    #[test]
    fn session_handoff_moves_authority_and_routes_through_old_home() {
        let directory = tempfile::tempdir().unwrap();
        let controller_b = Arc::new(
            Controller::open_with_id(
                JsonStore::new(directory.path().join("b.json")),
                Some("node-b".to_owned()),
            )
            .unwrap(),
        );
        let socket_b = directory.path().join("b.sock");
        let server_b = Arc::clone(&controller_b);
        let listen_b = socket_b.clone();
        std::thread::spawn(move || {
            RpcServer::new(listen_b)
                .serve(move |request| server_b.handle(request))
                .unwrap();
        });
        for _ in 0..100 {
            if socket_b.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let controller_a = Controller::open_with_id(
            JsonStore::new(directory.path().join("a.json")),
            Some("node-a".to_owned()),
        )
        .unwrap();
        assert!(
            controller_a
                .handle(Request::new(
                    "controller.register",
                    json!({"controllerId": "node-b", "endpoint": {"transport": "local", "socket": socket_b}}),
                ))
                .ok
        );
        let session = controller_a.handle(Request::new(
            "session.put",
            json!({
                "apiVersion": "workbench.dev/v1",
                "metadata": {"id": "session-1", "labels": {}, "createdAt": 1, "updatedAt": 1},
                "objective": "test",
                "state": "active"
            }),
        ));
        assert_eq!(
            session.result.unwrap()["authority"]["controllerId"],
            "node-a"
        );
        let submitted = controller_a.handle(Request::new(
            "task.submit",
            json!({
                "workspaceSessionId": "session-1",
                "executorId": "executor-1",
                "capability": "test",
                "input": {},
                "idempotencyKey": "handoff-task"
            }),
        ));
        let task_id = submitted.result.unwrap()["task"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        for state in ["running", "succeeded"] {
            assert!(
                controller_a
                    .handle(Request::new(
                        "task.transition",
                        json!({"taskId": task_id, "state": state})
                    ))
                    .ok
            );
        }
        let moved = controller_a.handle(Request::new(
            "session.handoff",
            json!({"sessionId": "session-1", "targetControllerId": "node-b"}),
        ));
        assert!(moved.ok, "{:?}", moved.error);
        assert_eq!(moved.result.unwrap()["authority"]["epoch"], 2);
        assert_eq!(
            controller_b
                .handle(Request::new("task.get", json!({"taskId": task_id})))
                .result
                .unwrap()["state"],
            "succeeded"
        );
        let transitioned = controller_a.handle(Request::new(
            "session.transition",
            json!({"sessionId": "session-1", "state": "completed"}),
        ));
        assert!(transitioned.ok, "{:?}", transitioned.error);
        assert_eq!(transitioned.result.unwrap()["state"], "completed");
        assert_eq!(
            controller_b
                .handle(Request::new(
                    "session.get",
                    json!({"sessionId": "session-1"})
                ))
                .result
                .unwrap()["state"],
            "completed"
        );
    }

    #[test]
    fn session_gate_prevents_mutation_from_crossing_a_handoff() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("target.sock");
        let server_socket = socket.clone();
        let (accept_started_tx, accept_started_rx) = mpsc::channel();
        let (release_accept_tx, release_accept_rx) = mpsc::channel();
        let release_accept_rx = Arc::new(Mutex::new(release_accept_rx));
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(move |request| match request.action.as_str() {
                    "session.accept-handoff" => {
                        let _ = accept_started_tx.send(());
                        release_accept_rx
                            .lock()
                            .expect("release receiver")
                            .recv()
                            .expect("handoff release");
                        Response::success(request.request_id, json!({"accepted": true}))
                    }
                    "session.transition" => Response::success(
                        request.request_id,
                        json!({"id": "session-gated", "state": "completed"}),
                    ),
                    _ => Response::success(
                        request.request_id,
                        json!({"controller": {"id": "node-b", "status": "ready"}}),
                    ),
                })
                .unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let controller = Arc::new(
            Controller::open_with_id(
                JsonStore::new(directory.path().join("source.json")),
                Some("node-a".to_owned()),
            )
            .unwrap(),
        );
        assert!(
            controller
                .handle(Request::new(
                    "controller.register",
                    json!({"controllerId": "node-b", "endpoint": {"transport": "local", "socket": socket}}),
                ))
                .ok
        );
        assert!(
            controller
                .handle(Request::new(
                    "session.put",
                    json!({
                        "apiVersion": "workbench.dev/v1",
                        "metadata": {"id": "session-gated", "labels": {}, "createdAt": 1, "updatedAt": 1},
                        "objective": "test gate",
                        "state": "active"
                    }),
                ))
                .ok
        );
        let handoff_controller = Arc::clone(&controller);
        let handoff = std::thread::spawn(move || {
            handoff_controller.handle(Request::new(
                "session.handoff",
                json!({"sessionId": "session-gated", "targetControllerId": "node-b"}),
            ))
        });
        accept_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handoff reached target");
        let transition_controller = Arc::clone(&controller);
        let (transition_tx, transition_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let response = transition_controller.handle(Request::new(
                "session.transition",
                json!({"sessionId": "session-gated", "state": "completed"}),
            ));
            transition_tx.send(response).unwrap();
        });
        assert!(
            transition_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        release_accept_tx.send(()).unwrap();
        assert!(handoff.join().unwrap().ok);
        let transitioned = transition_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("transition unblocked after handoff");
        assert!(transitioned.ok, "{:?}", transitioned.error);
    }

    #[test]
    fn automatic_driver_handoff_drains_hooks_and_rotates_the_fence() {
        let directory = tempfile::tempdir().unwrap();
        let controller = Controller::open_with_id(
            JsonStore::new(directory.path().join("controller.json")),
            Some("node-a".into()),
        )
        .unwrap();
        assert!(controller.handle(Request::new("session.put", json!({
            "apiVersion":"workbench.dev/v1", "metadata":{"id":"session-a","labels":{},"createdAt":1,"updatedAt":1},
            "objective":"handoff", "state":"active"
        }))).ok);
        let old = controller
            .handle(Request::new(
                "driver.acquire",
                json!({
                    "resource":"workspace:session-a", "owner":"old-agent", "ttlMs":60000
                }),
            ))
            .result
            .unwrap();
        assert!(controller.handle(Request::new("agent.mutation.start", json!({
            "workspaceSessionId":"session-a", "agentId":"old-agent", "operationId":"patch-1", "tool":"apply_patch"
        }))).ok);
        let requested = controller
            .handle(Request::new(
                "driver.handoff.request",
                json!({
                    "workspaceSessionId":"session-a", "owner":"new-agent", "ttlMs":60000
                }),
            ))
            .result
            .unwrap();
        assert_eq!(requested["state"], "draining");
        let blocked = controller.handle(Request::new("agent.mutation.start", json!({
            "workspaceSessionId":"session-a", "agentId":"old-agent", "operationId":"patch-2", "tool":"exec_command"
        })));
        assert_eq!(blocked.error.unwrap().code, "DRIVER_DRAINING");
        assert!(controller.handle(Request::new("status", json!({}))).ok);
        assert!(controller.handle(Request::new("agent.mutation.finish", json!({
            "workspaceSessionId":"session-a", "agentId":"old-agent", "operationId":"patch-1", "tool":"apply_patch"
        }))).ok);
        let next = controller
            .handle(Request::new(
                "driver.handoff.await",
                json!({
                    "workspaceSessionId":"session-a", "owner":"new-agent", "ttlMs":60000
                }),
            ))
            .result
            .unwrap();
        assert_eq!(next["owner"], "new-agent");
        assert!(next["fence"].as_u64().unwrap() > old["fence"].as_u64().unwrap());
        let stale = controller.handle(Request::new(
            "driver.release",
            json!({
                "resource":"workspace:session-a", "owner":"old-agent", "token":old["token"]
            }),
        ));
        assert_eq!(stale.error.unwrap().code, "LEASE_OWNED_BY_OTHER");
    }

    #[test]
    fn lease_order_requires_release_between_publish_phases() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("controller.json"))).unwrap();
        assert!(
            controller
                .handle(Request::new(
                    "driver.acquire",
                    json!({
                        "resource":"workspace:s", "owner":"publisher", "ttlMs":60000
                    })
                ))
                .ok
        );
        let runtime = controller.handle(Request::new(
            "lease.acquire",
            json!({
                "resource":"runtime:mac:doubao", "owner":"publisher", "ttlMs":60000
            }),
        ));
        assert_eq!(runtime.error.unwrap().code, "LEASE_ORDER_VIOLATION");
    }

    #[test]
    fn handoff_requires_acknowledgement_before_completion() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let created = controller.handle(Request::new(
            "handoff.create",
            json!({
                "id": "handoff-1",
                "workspaceSessionId": "session-1",
                "objective": "accept",
                "from": {"role": "coding"},
                "to": {"role": "gui-acceptance"},
                "createdAt": 0
            }),
        ));
        assert!(created.ok);
        let premature = controller.handle(Request::new(
            "handoff.complete",
            json!({"handoffId": "handoff-1"}),
        ));
        assert_eq!(premature.error.unwrap().code, "HANDOFF_NOT_ACKNOWLEDGED");
        assert!(
            controller
                .handle(Request::new(
                    "handoff.acknowledge",
                    json!({"handoffId": "handoff-1"}),
                ))
                .ok
        );
        assert!(
            controller
                .handle(Request::new(
                    "handoff.complete",
                    json!({"handoffId": "handoff-1"}),
                ))
                .ok
        );
    }

    #[test]
    fn transaction_journal_is_idempotent_and_fenced() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let begin = json!({
            "workspaceSessionId": "session-1",
            "idempotencyKey": "publish-1",
            "target": "client/current",
            "generationId": "generation-1",
            "leaseFence": 7
        });
        let first = controller.handle(Request::new("transaction.begin", begin.clone()));
        assert!(first.ok);
        let transaction_id = first.result.unwrap()["transaction"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let reused = controller.handle(Request::new("transaction.begin", begin));
        assert!(reused.result.unwrap()["reused"].as_bool().unwrap());
        let stale = controller.handle(Request::new(
            "transaction.record",
            json!({
                "transactionId": transaction_id,
                "step": "materialize",
                "state": "started",
                "fence": 6
            }),
        ));
        assert_eq!(stale.error.unwrap().code, "STALE_FENCING_TOKEN");
        let recorded = controller.handle(Request::new(
            "transaction.record",
            json!({
                "transactionId": transaction_id,
                "step": "materialize",
                "state": "succeeded",
                "fence": 7,
                "outputDigest": "sha256:ok"
            }),
        ));
        assert!(recorded.ok);
        let activated = controller.handle(Request::new(
            "transaction.record",
            json!({
                "transactionId": transaction_id,
                "step": "activate",
                "state": "succeeded",
                "fence": 7,
                "previousGenerationId": "generation-0"
            }),
        ));
        assert_eq!(
            activated.result.unwrap()["previousGenerationId"],
            "generation-0"
        );
    }

    #[test]
    fn approvals_are_exact_expiring_objects() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let requested = controller.handle(Request::new(
            "approval.request",
            json!({"digest": "sha256:exact", "owner": "agent", "reason": "test"}),
        ));
        assert!(requested.ok);
        let id = requested.result.unwrap()["id"].as_str().unwrap().to_owned();
        let approved = controller.handle(Request::new(
            "approval.approve",
            json!({"approvalId": id, "ttlMs": 1000}),
        ));
        assert_eq!(approved.result.unwrap()["state"], "approved");
        let duplicate =
            controller.handle(Request::new("approval.approve", json!({"approvalId": id})));
        assert_eq!(duplicate.error.unwrap().code, "APPROVAL_NOT_PENDING");
    }

    #[test]
    fn capability_schema_validation_checks_required_types_and_bounds() {
        let schema = json!({
            "type": "object",
            "required": ["port", "args"],
            "properties": {
                "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                "args": {"type": "array", "items": {"type": "string"}}
            },
            "additionalProperties": false
        });
        assert!(
            validate_schema(&schema, &json!({"port": 9222, "args": ["--safe"]}), "input").is_ok()
        );
        assert_eq!(
            validate_schema(&schema, &json!({"port": 0, "args": ["--safe"]}), "input")
                .unwrap_err()
                .code,
            "SCHEMA_VALIDATION_FAILED"
        );
        assert!(validate_schema(&schema, &json!({"port": 9222}), "input").is_err());
        assert!(validate_schema(&schema, &json!({"port": 9222, "args": [7]}), "input").is_err());
    }

    #[test]
    fn generation_identity_and_state_transitions_are_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let generation = json!({
            "id": "g1", "workspaceSessionId": "s1", "applicationType": "application",
            "root": "/state/g1", "state": "materializing",
            "baseline": {"digest": "sha256:base", "source": "/baseline"},
            "appliedArtifacts": [], "digest": "sha256:base", "createdAt": 1
        });
        assert!(
            controller
                .handle(Request::new("generation.put", generation.clone()))
                .ok
        );
        let mut active = generation;
        active["state"] = Value::String("active".to_owned());
        let invalid = controller.handle(Request::new("generation.put", active));
        assert_eq!(invalid.error.unwrap().code, "INVALID_GENERATION_STATE");
    }

    #[test]
    fn native_inspection_task_output_retains_only_a_bounded_summary() {
        let output = json!({
            "applicationPath": "/state/App.app",
            "pids": [42],
            "inspection": {
                "accessibilityTrusted": true,
                "processes": [{"pid": 42, "windows": [{
                    "role": "AXWindow",
                    "title": "",
                    "children": [
                        {"role": "AXStaticText", "title": "document.docx"},
                        {"role": "AXMenu", "children": [{"title": "Passwords…"}]}
                    ]
                }]}]
            },
            "inspectedAt": 123
        });
        let compact = retained_task_output("ui.native-inspect", &output);
        assert_eq!(compact["inspection"]["accessibilityTrusted"], true);
        assert_eq!(compact["inspection"]["processCount"], 1);
        assert_eq!(compact["inspection"]["processes"][0]["pid"], 42);
        assert_eq!(
            compact["inspection"]["processes"][0]["windows"][0]["accessibleNames"],
            json!(["document.docx"])
        );
        assert_eq!(retained_task_output("ui.evaluate", &output), output);
    }

    #[test]
    fn long_capabilities_do_not_hold_the_session_control_gate() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        assert!(!controller.requires_session_gate("capability.invoke"));
        assert!(!controller.requires_session_gate("driver.renew"));
        assert!(!controller.requires_session_gate("task.wait"));
        assert!(controller.requires_session_gate("session.handoff"));
        assert!(controller.requires_session_gate("transaction.record"));
    }

    #[test]
    fn status_is_bounded_by_default_and_verbose_on_request() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let compact = controller
            .handle(Request::new("status", json!({})))
            .result
            .unwrap();
        assert_eq!(compact["tasks"]["count"], 0);
        assert!(compact["tasks"].get(0).is_none());
        let verbose = controller
            .handle(Request::new("status", json!({"verbose": true})))
            .result
            .unwrap();
        assert!(verbose["tasks"].is_array());
    }

    #[test]
    fn run_lifecycle_records_scoped_rpc_observations() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let started = controller.handle(Request::new(
            "run.start",
            json!({"targetSummary":"build dashboard","agentSessionId":"agent-1"}),
        ));
        assert!(started.ok);
        let run_id = started.result.unwrap()["runId"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut status = Request::new("status", json!({}));
        status.run_id = Some(run_id.clone());
        status.span_id = Some("span-1".into());
        assert!(controller.handle(status).ok);
        let queried = controller
            .handle(Request::new("observability.query", json!({"runId":run_id})))
            .result
            .unwrap();
        assert!(
            queried["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["name"] == "status" && event["spanId"] == "span-1")
        );
        assert!(
            queried["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "run" && event["name"] == "run.start")
        );
        assert!(
            controller
                .handle(Request::new(
                    "run.finish",
                    json!({"runId":run_id,"status":"completed"})
                ))
                .ok
        );
        let queried = controller
            .handle(Request::new("observability.query", json!({"runId":run_id})))
            .result
            .unwrap();
        assert!(
            queried["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "run" && event["name"] == "run.finish")
        );
    }

    #[test]
    fn dashboard_snapshot_never_exposes_driver_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        assert!(
            controller
                .handle(Request::new(
                    "driver.take",
                    json!({"resource":"workspace:test","owner":"agent"})
                ))
                .ok
        );
        let snapshot = controller
            .handle(Request::new("dashboard.snapshot", json!({})))
            .result
            .unwrap()
            .to_string();
        assert!(!snapshot.contains("token"));
        assert!(!snapshot.contains("driverLeaseToken"));
    }

    #[test]
    fn force_cancel_requires_matching_single_use_operator_nonce() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let submitted=controller.handle(Request::new("task.submit",json!({"workspaceSessionId":"s","executorId":"e","capability":"process.start","input":{"processId":"p"},"idempotencyKey":"one"}))).result.unwrap();
        let task_id = submitted["task"]["id"].as_str().unwrap();
        assert_eq!(
            controller
                .handle(Request::new("task.cancel", json!({"taskId":task_id})))
                .error
                .unwrap()
                .code,
            "OPERATOR_REQUIRED"
        );
        let grant = controller
            .handle(Request::new(
                "operator.grant",
                json!({"operatorId":"human"}),
            ))
            .result
            .unwrap();
        let nonce=controller.handle(Request::new("operator.nonce",json!({"grantToken":grant["grantToken"],"action":"cancel-task","target":task_id,"reason":"stop damage","confirmed":true}))).result.unwrap();
        let action =
            json!({"actionNonce":nonce["actionNonce"],"action":"cancel-task","target":task_id});
        assert_eq!(
            controller
                .handle(Request::new("operator.action", action.clone()))
                .result
                .unwrap()["state"],
            "cancelled"
        );
        assert_eq!(
            controller
                .handle(Request::new("operator.action", action))
                .error
                .unwrap()
                .code,
            "ACTION_NONCE_INVALID"
        );
    }

    #[test]
    fn normal_operator_actions_use_grants_and_never_accept_force_without_nonce() {
        let directory = tempfile::tempdir().unwrap();
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        let submitted=controller.handle(Request::new("task.submit",json!({"workspaceSessionId":"s","executorId":"e","capability":"test","input":{},"idempotencyKey":"normal"}))).result.unwrap();
        let task_id = submitted["task"]["id"].as_str().unwrap();
        assert!(
            controller
                .handle(Request::new(
                    "task.transition",
                    json!({"taskId":task_id,"state":"running"})
                ))
                .ok
        );
        controller.handle(Request::new("task.transition",json!({"taskId":task_id,"state":"failed","error":{"code":"X","message":"retry","retryable":true,"details":null}})));
        let grant = controller
            .handle(Request::new(
                "operator.grant",
                json!({"operatorId":"dashboard","ttlMs":60000}),
            ))
            .result
            .unwrap();
        let retried = controller
            .handle(Request::new(
                "operator.action",
                json!({"grantToken":grant["grantToken"],"action":"retry-task","target":task_id}),
            ))
            .result
            .unwrap();
        assert_eq!(retried["state"], "queued");
        let forced = controller
            .handle(Request::new(
                "operator.action",
                json!({"grantToken":grant["grantToken"],"action":"cancel-task","target":task_id}),
            ))
            .error
            .unwrap();
        assert_eq!(forced.code, "INVALID_PARAMS");
        let unknown=controller.handle(Request::new("operator.action",json!({"grantToken":grant["grantToken"],"action":"arbitrary-call","target":task_id}))).error.unwrap();
        assert_eq!(unknown.code, "OPERATOR_ACTION_NOT_ALLOWED");
    }

    #[test]
    fn running_task_cancel_propagates_process_stop_to_executor() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("executor.sock");
        let stopped = Arc::new(AtomicBool::new(false));
        let server_stopped = Arc::clone(&stopped);
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket).serve(move|request|match request.action.as_str(){"status"=>Response::success(request.request_id,json!({"executorId":"executor","status":"ready","allowedRoots":[],"capabilities":[]})),"process.stop"=>{server_stopped.store(true,Ordering::SeqCst);Response::success(request.request_id,json!({"id":"process","state":"stopped"}))},_=>Response::failure(request.request_id,RpcError::new("UNEXPECTED","unexpected"))}).unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let controller =
            Controller::open(JsonStore::new(directory.path().join("state.json"))).unwrap();
        assert!(controller.handle(Request::new("executor.register",json!({"executorId":"executor","endpoint":{"transport":"local","socket":socket}}))).ok);
        let submitted=controller.handle(Request::new("task.submit",json!({"workspaceSessionId":"s","executorId":"executor","capability":"process.start","input":{"processId":"process"},"idempotencyKey":"run"}))).result.unwrap();
        let task_id = submitted["task"]["id"].as_str().unwrap().to_owned();
        assert!(
            controller
                .handle(Request::new(
                    "task.transition",
                    json!({"taskId":task_id,"state":"running"})
                ))
                .ok
        );
        let grant = controller
            .handle(Request::new(
                "operator.grant",
                json!({"operatorId":"human"}),
            ))
            .result
            .unwrap();
        let nonce=controller.handle(Request::new("operator.nonce",json!({"grantToken":grant["grantToken"],"action":"cancel-task","target":task_id,"reason":"stop damage","confirmed":true}))).result.unwrap();
        let cancelled = controller
            .handle(Request::new(
                "operator.action",
                json!({"actionNonce":nonce["actionNonce"],"action":"cancel-task","target":task_id}),
            ))
            .result
            .unwrap();
        assert_eq!(cancelled["state"], "cancelled");
        assert!(stopped.load(Ordering::SeqCst));
        let events = cancelled["events"].as_array().unwrap();
        assert!(
            events
                .iter()
                .any(|event| event["eventType"] == "task.cancel-requested")
        );
        assert!(
            events
                .iter()
                .any(|event| event["eventType"] == "task.cancelling")
        );
    }

    #[test]
    fn mutating_capability_releases_driver_lock_before_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("executor-mutation.sock");
        let runtime = Arc::new(
            crate::ExecutorRuntime::new("executor", vec![directory.path().to_path_buf()]).unwrap(),
        );
        let server_runtime = Arc::clone(&runtime);
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(move |request| server_runtime.handle(request))
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let controller = Arc::new(
            Controller::open_with_id(
                JsonStore::new(directory.path().join("controller-mutation.json")),
                Some("controller".to_owned()),
            )
            .unwrap(),
        );
        assert!(
            controller
                .handle(Request::new(
                    "executor.register",
                    json!({
                        "executorId":"executor",
                        "endpoint":{"transport":"local","socket":socket}
                    })
                ))
                .ok
        );
        assert!(controller.handle(Request::new("session.put", json!({
            "apiVersion":"workbench.dev/v1",
            "metadata":{"id":"workspace","labels":{},"createdAt":now_ms(),"updatedAt":now_ms()},
            "objective":"test mutating capability",
            "state":"active"
        }))).ok);
        let lease = controller
            .handle(Request::new(
                "driver.acquire",
                json!({
                    "resource":"workspace:workspace","owner":"agent","ttlMs":60000
                }),
            ))
            .result
            .unwrap();
        let token = lease["token"].as_str().unwrap().to_owned();
        let target = directory.path().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let controller_for_command = Arc::clone(&controller);
        let command_token = token.clone();
        std::thread::spawn(move || {
            let response = controller_for_command.handle(Request::new(
                "capability.invoke",
                json!({
                    "executorId":"executor",
                "capability":"command.run",
                    "workspaceSessionId":"workspace",
                    "owner":"agent",
                    "driverToken":command_token,
                    "idempotencyKey":"mutating-capability",
                    "executionMode":"sync",
                "input":{"cwd":target,"argv":["pwd"]}
                }),
            ));
            sender.send(response).unwrap();
        });
        let response = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("mutating capability deadlocked before executor dispatch");
        assert!(response.ok, "{:?}", response.error);

        let written = directory.path().join("authority-contract.txt");
        let response = controller.handle(Request::new(
            "capability.invoke",
            json!({
                "executorId":"executor",
                "capability":"filesystem.write",
                "workspaceSessionId":"workspace",
                "owner":"agent",
                "driverToken":token,
                "idempotencyKey":"mutating-capability-with-lock",
                "executionMode":"sync",
                "input":{"path":written,"content":"ok"}
            }),
        ));
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(std::fs::read_to_string(written).unwrap(), "ok");
    }

    #[test]
    fn observability_query_merges_peer_events_and_marks_offline_nodes_stale() {
        let directory = tempfile::tempdir().unwrap();
        let controller_b = Arc::new(
            Controller::open_with_id(
                JsonStore::new(directory.path().join("b.json")),
                Some("node-b".into()),
            )
            .unwrap(),
        );
        let socket = directory.path().join("b.sock");
        let server = Arc::clone(&controller_b);
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            RpcServer::new(server_socket)
                .serve(move |request| server.handle(request))
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let controller_a = Controller::open_with_id(
            JsonStore::new(directory.path().join("a.json")),
            Some("node-a".into()),
        )
        .unwrap();
        assert!(controller_a.handle(Request::new("controller.register",json!({"controllerId":"node-b","endpoint":{"transport":"local","socket":socket}}))).ok);
        controller_a
            .state
            .lock()
            .unwrap()
            .controllers
            .push(ControllerPeer {
                api_version: "workbench.dev/v1".into(),
                metadata: Metadata {
                    id: "node-offline".into(),
                    labels: Default::default(),
                    created_at: 1,
                    updated_at: 1,
                },
                endpoint: ExecutorEndpoint::Local {
                    socket: directory
                        .path()
                        .join("missing.sock")
                        .to_string_lossy()
                        .into_owned(),
                },
                health: HealthStatus::Offline,
            });
        for (controller, node) in [(&controller_a, "node-a"), (&*controller_b, "node-b")] {
            assert!(controller.handle(Request::new("observation.append",json!({"eventId":0,"runId":"run-shared","timestamp":1,"nodeId":node,"role":"controller","kind":"test","name":"event","status":"completed","attributes":{}}))).ok);
        }
        let result = controller_a
            .handle(Request::new(
                "observability.query",
                json!({"runId":"run-shared","limit":20}),
            ))
            .result
            .unwrap();
        assert_eq!(result["events"].as_array().unwrap().len(), 2);
        assert!(
            result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["nodeId"] == "node-offline" && node["stale"] == true)
        );
        assert!(result["cursors"].get("node-a").is_some());
        assert!(result["cursors"].get("node-b").is_some());
    }
}

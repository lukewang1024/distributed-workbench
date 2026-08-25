import React, { useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import "./style.css";

type Any = Record<string, any>;
type Dashboard = {
  generatedAt?: number;
  nodes?: Any[];
  tunnels?: Any[];
  workspaces?: Any[];
  unscopedEvents?: Any[];
};

const api = async (path: string) => {
  const response = await fetch(path, { cache: "no-store" });
  if (!response.ok) throw Error(await response.text());
  return response.json();
};
const statusCopy: Record<string, string> = {
  failed: "失败",
  blocked: "已阻塞",
  stalled: "疑似停滞",
  active: "推进中",
  idle: "待命",
};
const statusIcon: Record<string, string> = {
  failed: "!",
  blocked: "‖",
  stalled: "…",
  active: "↗",
  idle: "○",
};
const stageCopy: Record<string, string> = {
  prepare: "准备",
  build: "构建",
  component: "组件运行",
  publish: "发布",
  acceptance: "验收",
  activity: "执行",
};
const roleCopy: Record<string, string> = {
  primary: "主 Agent",
  "computer-use": "验收 Agent",
  agent: "Agent",
  controller: "Controller",
  executor: "Executor",
};
const stateRank: Record<string, number> = {
  failed: 0,
  blocked: 1,
  stalled: 2,
  active: 3,
  idle: 4,
};
const terminalRun = (run: Any) =>
  ["completed", "cancelled", "interrupted"].includes(run.status);
const age = (value?: number) => {
  if (!value) return "暂无进展";
  const seconds = Math.max(0, Math.round((Date.now() - value) / 1000));
  if (seconds < 60) return `${seconds} 秒前`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return `${Math.floor(seconds / 86400)} 天前`;
};
const duration = (value?: number) => {
  if (!value) return "—";
  if (value < 1000) return `${value} ms`;
  if (value < 60000)
    return `${(value / 1000).toFixed(value < 10000 ? 1 : 0)} 秒`;
  if (value < 3600000) return `${Math.round(value / 60000)} 分钟`;
  return `${(value / 3600000).toFixed(1)} 小时`;
};
const formatTime = (value?: number) =>
  value
    ? new Date(value).toLocaleTimeString("zh-CN", {
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      })
    : "—";
const uniq = (values: string[]) => [...new Set(values.filter(Boolean))];

function capabilityLabel(value = "") {
  const tail = value.split(".").pop() || value;
  if (value.includes("build")) return "构建产物";
  if (value.includes("validate")) return "校验运行环境";
  if (value.includes("materialize") || value.includes("apply-artifacts"))
    return "组装应用资源";
  if (value.includes("finalize")) return "完成发布准备";
  if (value.includes("launch") || value.includes("open-file"))
    return "启动应用";
  if (value.includes("inspect")) return "执行界面验收";
  if (value.includes("process.start")) return "启动组件";
  if (value.includes("process.stop")) return "停止组件";
  if (value.includes("handoff")) return "交接验收任务";
  return tail.replaceAll("-", " ");
}

function activityCopy(event?: Any) {
  if (!event) return "等待新的任务活动";
  const attrs = event.attributes || {};
  if (event.kind === "blocker")
    return event.status === "resolved"
      ? "阻塞已解除"
      : attrs.blockerReason || "任务已报告阻塞";
  if (event.kind === "agent-tool") return `正在使用 ${attrs.action || "工具"}`;
  if (event.kind === "task-state" || attrs.capability)
    return capabilityLabel(attrs.capability || event.name);
  if (event.kind === "readiness")
    return event.status === "ready" ? "组件已就绪" : "正在等待组件就绪";
  if (event.kind === "handoff") return "正在跨节点交接任务";
  if (event.kind === "artifact" || event.kind === "generation")
    return "正在准备可发布产物";
  if (event.name === "SubagentStart") return "已启动协作 Agent";
  if (event.name === "SubagentStop") return "协作 Agent 已结束";
  return capabilityLabel(event.name || event.kind || "任务活动");
}

function blockerCopy(blocker?: Any) {
  const attrs = blocker?.attributes || {};
  const kinds: Record<string, string> = {
    human: "等待人工处理",
    dependency: "等待依赖",
    node: "节点异常",
    error: "执行错误",
    "no-progress": "长时间无进展",
  };
  return blocker
    ? `${kinds[attrs.blockerKind] || "任务阻塞"}${attrs.blockerReason ? `：${attrs.blockerReason}` : ""}`
    : "";
}

function eventStage(event: Any, workspace: Any) {
  const explicit = event.attributes?.stage;
  if (explicit) return explicit;
  const capability = event.attributes?.capability || event.name;
  const groups = workspace.observability?.capabilityGroups || {};
  return (
    Object.keys(groups).find((stage) =>
      (groups[stage] || []).includes(capability),
    ) ||
    workspace.currentStage ||
    workspace.observability?.stages?.[0]?.id ||
    "activity"
  );
}

function eventNode(event: Any, workspace: Any) {
  return (
    event.nodeId ||
    event.attributes?.targetNode ||
    workspace.activeNodes?.[0] ||
    "local"
  );
}
function eventRole(event: Any) {
  return event.role || "agent";
}

function navigate(id?: string) {
  const path = id ? `/profiles/${encodeURIComponent(id)}` : "/";
  history.pushState({}, "", path);
  window.dispatchEvent(new PopStateEvent("popstate"));
}
function useRoute() {
  const read = () => {
    const match = location.pathname.match(/^\/profiles\/([^/]+)$/);
    return match ? decodeURIComponent(match[1]) : "";
  };
  const [value, setValue] = useState(read);
  useEffect(() => {
    const change = () => setValue(read());
    addEventListener("popstate", change);
    return () => removeEventListener("popstate", change);
  }, []);
  return value;
}

function App() {
  const [data, setData] = useState<Dashboard>({}),
    [error, setError] = useState(""),
    [showRecent, setShowRecent] = useState(false);
  const selectedId = useRoute();
  const load = async () => {
    try {
      setData(await api("/api/workspaces"));
      setError("");
    } catch (value) {
      setError(String(value));
    }
  };
  useEffect(() => {
    load();
    const timer = setInterval(load, 5000);
    const source = new EventSource("/api/events?cursor=0");
    source.addEventListener("observations", load);
    return () => {
      clearInterval(timer);
      source.close();
    };
  }, []);
  const workspaces = useMemo(
    () =>
      [...(data.workspaces || [])].sort(
        (a, b) => (stateRank[a.status] ?? 9) - (stateRank[b.status] ?? 9),
      ),
    [data],
  );
  const selected = workspaces.find((workspace) => workspace.id === selectedId);
  if (selectedId && selected)
    return (
      <Detail
        workspace={selected}
        nodes={data.nodes || []}
        updatedAt={data.generatedAt}
        error={error}
      />
    );
  return (
    <Overview
      data={data}
      workspaces={workspaces}
      error={error}
      showRecent={showRecent}
      setShowRecent={setShowRecent}
    />
  );
}

function AppHeader({
  updatedAt,
  compact = false,
}: {
  updatedAt?: number;
  compact?: boolean;
}) {
  return (
    <header className={compact ? "compact" : ""}>
      <div className="brand">
        <div className="brand-mark">
          <span />
          <span />
          <span />
        </div>
        <div>
          <h1>Workbench</h1>
          <p>Profile 任务驾驶舱 · 只读</p>
        </div>
      </div>
      <div className="live">
        <i />
        实时更新 <span>{formatTime(updatedAt)}</span>
      </div>
    </header>
  );
}

function NodeStrip({ nodes, workspaces }: { nodes: Any[]; workspaces: Any[] }) {
  return (
    <section className="node-strip" aria-label="节点状态">
      <div className="node-strip-title">
        <span>节点</span>
        <small>
          {nodes.filter((node) => node.health === "ready").length}/
          {nodes.length} 在线
        </small>
      </div>
      {nodes.length ? (
        nodes.map((node) => {
          const loads = workspaces.filter((workspace) =>
            workspace.activeNodes?.includes(node.id),
          );
          const state = node.health || "unknown";
          return (
            <div className={`node-chip ${state}`} key={node.id}>
              <i />
              <div>
                <b>{node.id}</b>
                <span>
                  {state === "ready"
                    ? loads.length
                      ? `${loads.length} 个 Profile 正在工作`
                      : "在线 · 待命"
                    : state === "offline"
                      ? "连接中断"
                      : state === "degraded"
                        ? "状态异常"
                        : "状态未知"}
                </span>
              </div>
              {loads.length > 0 && <em>{loads.length}</em>}
            </div>
          );
        })
      ) : (
        <div className="empty-inline">尚未注册节点</div>
      )}
    </section>
  );
}

function endpoint(value?: Any) {
  return value ? `${value.host || "127.0.0.1"}:${value.port || "—"}` : "—";
}
function TunnelPanel({
  tunnels,
  workspaceId,
}: {
  tunnels: Any[];
  workspaceId?: string;
}) {
  const visible = workspaceId
    ? tunnels.filter((tunnel) => tunnel.workspaceSessionId === workspaceId)
    : tunnels;
  const healthy = visible.filter((tunnel) =>
    ["ready", "running"].includes(tunnel.observedState),
  ).length;
  return (
    <section className="tunnel-panel">
      <div className="tunnel-head">
        <div>
          <span className="eyebrow">NODE TUNNELS</span>
          <h2>节点间 Tunnel</h2>
        </div>
        <p>
          {visible.length
            ? `${healthy}/${visible.length} 健康`
            : "当前没有受管 Tunnel"}
        </p>
      </div>
      {visible.length > 0 && (
        <div className="tunnel-table">
          <div className="tunnel-row tunnel-labels">
            <span>状态</span>
            <span>运行节点</span>
            <span>转发路径</span>
            <span>作用域</span>
            <span>最近探测</span>
          </div>
          {visible.map((tunnel) => (
            <div
              className="tunnel-row"
              key={`${tunnel.executorId}-${tunnel.id}`}
            >
              <span>
                <i className={`tunnel-state ${tunnel.observedState}`} />
                <b>{tunnel.observedState || "unknown"}</b>
              </span>
              <span>
                <b>{tunnel.executorId || "—"}</b>
                <small>{tunnel.direction || "—"}</small>
              </span>
              <span className="tunnel-route">
                <b>{endpoint(tunnel.source)}</b>
                <em>→</em>
                <b>
                  {tunnel.sshHost || "remote"} / {endpoint(tunnel.destination)}
                </b>
              </span>
              <span>
                <b>{tunnel.workspaceSessionId || "全局"}</b>
                <small>{tunnel.id}</small>
              </span>
              <span>
                <b>{age(tunnel.lastProbeAt || tunnel.updatedAt)}</b>
                <small>{tunnel.desiredState || "—"}</small>
              </span>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

function Overview({
  data,
  workspaces,
  error,
  showRecent,
  setShowRecent,
}: {
  data: Dashboard;
  workspaces: Any[];
  error: string;
  showRecent: boolean;
  setShowRecent: (value: boolean) => void;
}) {
  const cutoff = Date.now() - 86400000;
  const recent = workspaces.filter(
    (workspace) =>
      workspace.status === "idle" &&
      (workspace.runs || []).some(
        (run: Any) => terminalRun(run) && (run.finishedAt || 0) > cutoff,
      ),
  );
  const visible = workspaces.filter((workspace) => !recent.includes(workspace));
  const counts = {
    attention: workspaces.filter((workspace) =>
      ["failed", "blocked", "stalled"].includes(workspace.status),
    ).length,
    active: workspaces.filter((workspace) => workspace.status === "active")
      .length,
  };
  return (
    <>
      <AppHeader updatedAt={data.generatedAt} />
      <main>
        {error && (
          <div className="error-banner">
            <b>看板暂时无法更新</b>
            <span>{error}</span>
          </div>
        )}
        <NodeStrip nodes={data.nodes || []} workspaces={workspaces} />
        <TunnelPanel tunnels={data.tunnels || []} />
        <section className="hero">
          <div>
            <span className="eyebrow">NOW RUNNING</span>
            <h2>每个 Profile，现在进行到哪里？</h2>
            <p>关注任务推进，而不是消费原始日志。异常会自动排在最前面。</p>
          </div>
          <div className="hero-metrics">
            <div className={counts.attention ? "attention" : ""}>
              <strong>{counts.attention}</strong>
              <span>需要关注</span>
            </div>
            <div>
              <strong>{counts.active}</strong>
              <span>正在推进</span>
            </div>
            <div>
              <strong>{workspaces.length}</strong>
              <span>全部 Profile</span>
            </div>
          </div>
        </section>
        <section className="section-heading">
          <div>
            <span className="eyebrow">PROFILES</span>
            <h2>任务概览</h2>
          </div>
          <p>
            {counts.attention
              ? `有 ${counts.attention} 个任务需要处理`
              : "目前没有需要立即处理的问题"}
          </p>
        </section>
        <div className="profile-grid">
          {visible.map((workspace) => (
            <ProfileCard key={workspace.id} workspace={workspace} />
          ))}
        </div>
        {!visible.length && !recent.length && <EmptyState />}
        {recent.length > 0 && (
          <section className="recent">
            <button
              className="recent-toggle"
              onClick={() => setShowRecent(!showRecent)}
              aria-expanded={showRecent}
            >
              <span>
                <b>最近 24 小时已完成</b>
                <small>{recent.length} 个 Profile</small>
              </span>
              <i>{showRecent ? "−" : "+"}</i>
            </button>
            {showRecent && (
              <div className="profile-grid compact-grid">
                {recent.map((workspace) => (
                  <ProfileCard key={workspace.id} workspace={workspace} />
                ))}
              </div>
            )}
          </section>
        )}
      </main>
      <footer>数据来自本机 Controller · 原始证据仅在详情中按需展示</footer>
    </>
  );
}

function ProfileCard({ workspace }: { workspace: Any }) {
  const status = workspace.status || "idle",
    stages = workspace.observability?.stages || [
      { id: "activity", label: "Activity" },
    ];
  const stageIndex = Math.max(
    0,
    stages.findIndex((stage: Any) => stage.id === workspace.currentStage),
  );
  const activeNode =
    workspace.currentActivity?.nodeId || workspace.activeNodes?.[0];
  return (
    <button
      className={`profile-card ${status}`}
      onClick={() => navigate(workspace.id)}
    >
      <div className="profile-top">
        <span className={`status ${status}`}>
          <i>{statusIcon[status] || "○"}</i>
          {statusCopy[status] || status}
        </span>
        <span className="open-arrow">↗</span>
      </div>
      <h3>{workspace.id}</h3>
      <p className="objective">{workspace.objective || "暂无任务目标"}</p>
      <div
        className="mini-progress"
        aria-label={`当前阶段 ${workspace.currentStage || "activity"}`}
      >
        {stages.map((stage: Any, index: number) => (
          <span
            key={stage.id}
            className={`${index < stageIndex ? "done" : ""} ${index === stageIndex ? "current" : ""}`}
          />
        ))}
      </div>
      <div className="current-action">
        <span className="action-icon">
          {status === "blocked" ? "‖" : status === "stalled" ? "…" : "›"}
        </span>
        <div>
          <small>{status === "blocked" ? "阻塞原因" : "当前动作"}</small>
          <b>
            {status === "blocked"
              ? blockerCopy(workspace.blocker)
              : activityCopy(workspace.currentActivity)}
          </b>
        </div>
      </div>
      <div className="profile-meta">
        <span>
          <small>阶段</small>
          <b>
            {stageCopy[workspace.currentStage] ||
              stages[stageIndex]?.label ||
              "等待开始"}
          </b>
        </span>
        <span>
          <small>节点 / 角色</small>
          <b>
            {activeNode || "未分配"}
            {workspace.activeRoles?.[0]
              ? ` · ${roleCopy[workspace.activeRoles[0]] || workspace.activeRoles[0]}`
              : ""}
          </b>
        </span>
        <span>
          <small>最后进展</small>
          <b>{age(workspace.lastProgressAt)}</b>
        </span>
      </div>
    </button>
  );
}

function Detail({
  workspace,
  nodes,
  updatedAt,
  error,
}: {
  workspace: Any;
  nodes: Any[];
  updatedAt?: number;
  error: string;
}) {
  const [selectedEvidence, setSelectedEvidence] = useState<Any | null>(null);
  const stages = workspace.observability?.stages?.length
    ? workspace.observability.stages
    : [{ id: "activity", label: "Activity" }];
  const currentIndex = Math.max(
    0,
    stages.findIndex((stage: Any) => stage.id === workspace.currentStage),
  );
  const events = workspace.events || [];
  const laneKeys = uniq([
    ...(workspace.agents || []).map(
      (agent: Any) => `${agent.executorId}|${agent.role}`,
    ),
    ...events.map(
      (event: Any) => `${eventNode(event, workspace)}|${eventRole(event)}`,
    ),
  ]);
  const detailNodes = nodes.filter(
    (node) =>
      workspace.activeNodes?.includes(node.id) ||
      laneKeys.some((key) => key.startsWith(`${node.id}|`)),
  );
  return (
    <>
      <AppHeader updatedAt={updatedAt} compact />
      <main className="detail-page">
        {error && (
          <div className="error-banner">
            <b>实时更新已中断</b>
            <span>{error}</span>
          </div>
        )}
        <button className="back" onClick={() => navigate()}>
          ← 返回所有 Profile
        </button>
        <section className={`detail-hero ${workspace.status}`}>
          <div>
            <span className={`status ${workspace.status}`}>
              <i>{statusIcon[workspace.status]}</i>
              {statusCopy[workspace.status]}
            </span>
            <h2>{workspace.id}</h2>
            <p>{workspace.objective || "暂无任务目标"}</p>
          </div>
          <div className="detail-facts">
            <span>
              <small>当前阶段</small>
              <b>
                {stageCopy[workspace.currentStage] ||
                  workspace.currentStage ||
                  "等待开始"}
              </b>
            </span>
            <span>
              <small>最后有效进展</small>
              <b>{age(workspace.lastProgressAt)}</b>
            </span>
            <span>
              <small>活跃节点</small>
              <b>{workspace.activeNodes?.join("、") || "无"}</b>
            </span>
          </div>
        </section>
        {workspace.blocker && (
          <section className="blocker-banner">
            <span>‖</span>
            <div>
              <small>任务被阻塞</small>
              <b>{blockerCopy(workspace.blocker)}</b>
              <p>
                {workspace.blocker.attributes?.waitingOn &&
                  `等待对象：${workspace.blocker.attributes.waitingOn} · `}
                开始于 {age(workspace.blocker.timestamp)}
              </p>
            </div>
            <button onClick={() => setSelectedEvidence(workspace.blocker)}>
              查看证据
            </button>
          </section>
        )}
        {workspace.status === "stalled" && (
          <section className="stalled-banner">
            <span>…</span>
            <div>
              <small>疑似停滞</small>
              <b>超过预期时间没有检测到有效进展</b>
              <p>心跳和重复轮询不会重置停滞计时。</p>
            </div>
          </section>
        )}
        <NodeStrip nodes={detailNodes} workspaces={[workspace]} />
        <section className="workflow">
          <div className="workflow-title">
            <div>
              <span className="eyebrow">WORKFLOW</span>
              <h2>阶段与节点泳道</h2>
            </div>
            <p>
              <i className="legend active" />
              正在工作 <i className="legend done" />
              已完成 <i className="legend wait" />
              等待
            </p>
          </div>
          <div
            className={`stage-track ${workspace.status}`}
            style={{ "--columns": stages.length } as React.CSSProperties}
          >
            {stages.map((stage: Any, index: number) => (
              <div
                key={stage.id}
                className={`stage ${index < currentIndex ? "done" : ""} ${index === currentIndex ? "current" : ""}`}
              >
                <i>{index < currentIndex ? "✓" : index + 1}</i>
                <span>{stageCopy[stage.id] || stage.label}</span>
                {index === currentIndex && (
                  <small>
                    {workspace.status === "blocked"
                      ? "停在这里"
                      : workspace.status === "stalled"
                        ? "等待进展"
                        : "当前阶段"}
                  </small>
                )}
              </div>
            ))}
          </div>
          <div className="lanes">
            {laneKeys.length ? (
              laneKeys.map((key) => {
                const [node, role] = key.split("|");
                const laneEvents = events.filter(
                  (event: Any) =>
                    eventNode(event, workspace) === node &&
                    eventRole(event) === role,
                );
                return (
                  <div className="lane-row" key={key}>
                    <div className="lane-label">
                      <span
                        className={`node-dot ${workspace.activeNodes?.includes(node) ? "working" : ""}`}
                      />
                      <div>
                        <b>{node}</b>
                        <small>{roleCopy[role] || role}</small>
                      </div>
                    </div>
                    <div
                      className="lane-grid"
                      style={
                        { "--columns": stages.length } as React.CSSProperties
                      }
                    >
                      {stages.map((stage: Any) => (
                        <div className="lane-cell" key={stage.id}>
                          {laneEvents
                            .filter(
                              (event: Any) =>
                                eventStage(event, workspace) === stage.id,
                            )
                            .slice(-4)
                            .map((event: Any, index: number) => (
                              <button
                                key={`${event.eventId}-${index}`}
                                className={`event-chip ${event.status === "running" ? "running" : ""} ${event.status === "failed" ? "failed" : ""}`}
                                onClick={() => setSelectedEvidence(event)}
                              >
                                <span>{activityCopy(event)}</span>
                                <small>
                                  {formatTime(event.timestamp)}
                                  {event.durationMs
                                    ? ` · ${duration(event.durationMs)}`
                                    : ""}
                                </small>
                              </button>
                            ))}
                        </div>
                      ))}
                    </div>
                  </div>
                );
              })
            ) : (
              <div className="empty-lanes">
                <span>◇</span>
                <b>等待第一条结构化活动</b>
                <p>Agent 开始工作后，节点和角色会自动出现在这里。</p>
              </div>
            )}
          </div>
        </section>
        <section className="evidence-list">
          <div className="workflow-title">
            <div>
              <span className="eyebrow">EVIDENCE</span>
              <h2>最近的有效进展</h2>
            </div>
            <p>默认隐藏底层协议字段</p>
          </div>
          {events
            .filter((event: Any) => event.name !== "PreToolUse")
            .slice(-8)
            .reverse()
            .map((event: Any) => (
              <button
                key={`${event.nodeId}-${event.eventId}`}
                onClick={() => setSelectedEvidence(event)}
              >
                <time>{formatTime(event.timestamp)}</time>
                <span className={`event-state ${event.status}`} />
                <div>
                  <b>{activityCopy(event)}</b>
                  <small>
                    {event.nodeId} · {roleCopy[event.role] || event.role} ·{" "}
                    {event.kind}
                  </small>
                </div>
                <i>查看证据 →</i>
              </button>
            ))}
        </section>
      </main>
      {selectedEvidence && (
        <EvidenceDrawer
          value={selectedEvidence}
          close={() => setSelectedEvidence(null)}
        />
      )}
    </>
  );
}

function EvidenceDrawer({ value, close }: { value: Any; close: () => void }) {
  const safe = {
    timestamp: value.timestamp,
    nodeId: value.nodeId,
    role: value.role,
    kind: value.kind,
    name: value.name,
    status: value.status,
    durationMs: value.durationMs,
    requestId: value.requestId,
    taskId: value.taskId,
    processId: value.processId,
    attributes: value.attributes,
  };
  return (
    <div className="drawer-backdrop" onClick={close}>
      <aside className="drawer" onClick={(event) => event.stopPropagation()}>
        <div className="drawer-head">
          <div>
            <span className="eyebrow">STRUCTURED EVIDENCE</span>
            <h2>{activityCopy(value)}</h2>
          </div>
          <button onClick={close} aria-label="关闭证据">
            ×
          </button>
        </div>
        <dl>
          <div>
            <dt>节点</dt>
            <dd>{value.nodeId || "—"}</dd>
          </div>
          <div>
            <dt>角色</dt>
            <dd>{roleCopy[value.role] || value.role || "—"}</dd>
          </div>
          <div>
            <dt>状态</dt>
            <dd>{value.status || "—"}</dd>
          </div>
          <div>
            <dt>时间</dt>
            <dd>
              {value.timestamp
                ? new Date(value.timestamp).toLocaleString("zh-CN")
                : "—"}
            </dd>
          </div>
        </dl>
        <h3>安全字段</h3>
        <pre>{JSON.stringify(safe, null, 2)}</pre>
        <p className="drawer-note">
          原始 prompt、token、完整命令和未授权日志不会出现在此处。
        </p>
      </aside>
    </div>
  );
}
function EmptyState() {
  return (
    <section className="empty-state">
      <span>◇</span>
      <h3>还没有可观测的 Profile</h3>
      <p>创建 Workspace Session 并启动 Agent 后，这里会自动出现任务卡片。</p>
    </section>
  );
}
createRoot(document.getElementById("root")!).render(<App />);

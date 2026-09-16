# Computer Use through the upstream Pi extension

Workbench runs the **unmodified** `@injaneity/pi-computer-use` 0.5.1 package with
Pi's official extension loader (SDK 0.85.1). `computer-use/package-lock.json`
pins the complete dependency tree and npm integrity hashes. No platform source
is vendored, translated or maintained here. Upstream MIT notices ship in the
npm package. An official Node 24.14.0 runtime is pinned by SHA-256.

The adapter owns only transport, lifecycle, schema validation and the Executor
desktop queue. Screenshots, accessibility trees, input, stale observations, UI refs,
verification and platform fallbacks belong to the plugin. No model/API key or
Pi login is needed. Only the configured extension is loaded; workbench sessions
use an isolated Pi state directory, never the user's personal Pi configuration.

## Install

Desktop release archives contain `computer-use/` with Node and locked packages.
For a checkout or explicit separate installation, with Node >=24 available:

```sh
node scripts/install-computer-use.mjs /absolute/release/computer-use
```

The installer verifies official Node archives and runs `npm ci --ignore-scripts`.
Native helper installation remains upstream-owned on first use. Helper data lives
under `$XDG_DATA_HOME/distributed-workbench/computer-use` on POSIX and
`%LOCALAPPDATA%/distributed-workbench/computer-use` on Windows. macOS keeps the
upstream socket in `~/Library/Caches/pi-computer-use` for its LaunchServices flow.
Release packaging must run on the target OS/architecture; npm native dependencies
are platform-specific. Linux needs glibc for the bundled Node/native helper even
though the workbench control-plane binary itself uses musl. Android is unsupported.

`WORKBENCH_COMPUTER_USE_ROOT` is an administrator override for the host directory.
It is never accepted as tool input. Production nodes should use the immutable
installer-recorded runtime path and be upgraded together through fabric bootstrap.
Installers copy the plugin out of temporary release staging into versioned data
and record `computer-use/runtime-root` in Executor state. An unpacked archive also
works through the release-relative fallback.

## Use

All commands address the local Controller, which routes to the chosen Executor.

```sh
workbench computer-use tools --executor <executor-id>
workbench computer-use open --executor <executor-id> --owner <unique-session> --request-key <stable-request-key>
workbench computer-use status --executor <executor-id> --owner <unique-session> --token <returned-token>
workbench computer-use call --executor <executor-id> --owner <unique-session> --token <returned-token> find_roots '{}'
workbench computer-use call --executor <executor-id> --owner <unique-session> --token <returned-token> observe_ui '{"root":"@r1","mode":"fused"}'
workbench computer-use call --executor <executor-id> --owner <unique-session> --token <returned-token> act_ui '{"stateId":"<observed-state>","actions":[{"action":"press","ref":"@e3"}]}'
workbench computer-use close --executor <executor-id> --owner <unique-session> --token <returned-token>
```

Use the `tools` response as the authoritative original schemas. Long JSON can be
passed on stdin with `-`. `open` submits to the **target Executor's durable FIFO**.
Keep its token and poll `status` until `state=active` before calling tools. A lost
submit response can be recovered by repeating the same owner/request-key; use a
new key for a new session. `queue` lists sessions without exposing tokens. `cancel`
removes queued work or drains active work; `close` finishes the whole session.
An active session expires after 15 minutes by default; renew before expiry.
Queued time does not consume its TTL. TTL must be 1 second to 1 hour.

All Controllers, including CloudIDE submitters, route to the same target queue.
The queue owns an entire acceptance session, not individual clicks. Activation,
observations, actions, screenshots and cleanup belong inside that session. Every
new session gets a unique identity and increasing epoch, invalidating prior refs.
Different Executor desktops run independently. One Executor per interactive
desktop is required; multiple Executor services on the same desktop are unsupported.

Legacy `application.*`, `ui.*`, and `clipboard.write` calls share the execution
gate. During a session they must carry `_desktop: {owner, token}` in their Executor
input, **in addition to** their existing runtime/acceptance authority. Without a
session these compatibility calls are individually serialized; callers needing a
multi-step guarantee must submit a session first. Schema discovery is also gated
because initializing upstream helpers can affect the desktop; discover before
submitting, or pass session credentials using `executor.call`.

Expiry runs in the Executor even if submitters disconnect. A successor starts only
after in-flight work returns and the upstream host acknowledges cleanup. Calls
validated before waiting on the execution gate are revalidated inside it. Tool
transport failures and Executor restart with an outstanding session quarantine the
desktop. No GUI action is automatically replayed. After an operator has stopped old
native helpers and reset/checked the desktop, use the local Controller:

```sh
workbench call desktop.recover '{"executorId":"<executor-id>","confirmDesktopReset":true}'
```

The queue is stored beside Executor state in `desktop-queue.json`; deleting it
loses fencing history and is not a recovery procedure. Completed sessions retain
submission deduplication; the queue rejects submissions at 10,000 retained entries
rather than silently discarding that history. Scheduling is within the trusted
fabric boundary and does not exclude a human using the keyboard or an unrelated
OS automation process.

## Platform requirements

| Platform | Upstream backend | Requirements/limits |
| --- | --- | --- |
| macOS | AX + native capture helper | macOS 14+, Accessibility and Screen Recording grants for the upstream helper app |
| Windows | UI Automation + native input/capture | Native interactive user desktop; workbench launches the host into the active session. Locked/secure desktops or integrity-level mismatches can prevent physical input. |
| Linux X11 | AT-SPI2 + XComposite/XTEST | CU host must use the target user's DISPLAY, XAUTHORITY and accessibility/session bus environment (see environment.json below) |
| Linux Wayland | AT-SPI2 | Semantic operations only; upstream does not implement portal capture/input |

`COMPUTER_USE_UNAVAILABLE` means runtime/transport readiness failed.
`COMPUTER_USE_TOOL_FAILED` preserves the upstream permission, stale-ref or native
failure. Fix the stated prerequisite; do not replace the plugin with injected
PowerShell or platform-specific scripts.

## Verification

```sh
npm test --prefix computer-use
cargo test -p workbench-runtime computer_use --lib
cargo build -p workbench-cli --bin workbench
python3 scripts/test-computer-use.py
python3 scripts/test-desktop-queue.py
```

The smoke test uses isolated temporary Controller/Executor instances and the real
upstream extension, proving tool discovery, lease exclusion, stale-state rejection,
shutdown and stale-token rejection. It does not claim GUI acceptance on any OS.
Live acceptance must separately observe an actual window, perform an authorized
interaction, check the successor state and save its screenshot.

Reference: https://github.com/injaneity/pi-computer-use

## Per-desktop CU environment

Administrators may write `environment.json` in the Executor's `computer-use`
state directory (beside `runtime-root`). The dedicated CU host reads it before
loading the upstream SDK on every new host. It changes only the CU process and
its helpers, never the Executor or its clipboard backend. Finish existing CU
sessions before changing it; no Executor restart is needed. Missing files inherit
the service environment; invalid files fail closed. Configuration is local
administrator state, never accepted from a remote tool call.

For an Xfce desktop independent of the clipboard Xvfb:

```json
{
  "DISPLAY": ":1",
  "XAUTHORITY": "/home/wangyuanlv/.Xauthority",
  "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1001/bus"
}
```

Allowed keys are `DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`,
`AT_SPI_BUS_ADDRESS`, `XDG_RUNTIME_DIR`, and `PI_COMPUTER_USE_HEADLESS`; values
are strings. The latter accepts only `"true"` or `"false"`. Keep this file readable
only by the service/interactive user and administrators.

On Windows the default path is
`C:/ProgramData/distributed-workbench/computer-use/environment.json`.
`{"PI_COMPUTER_USE_HEADLESS":"false"}` enables upstream foreground input policy
and overrides the upstream `.pi/computer-use.json` headless setting. Alternatively
keep using the upstream config file directly; omit the environment override if
that file should remain authoritative. Physical keys (for example Hyper CapsLock
or F18) must use the original plugin's discovered `act_ui` keyboard schema, with
a fresh observation and foreground delivery permitted. Text insertion is not raw
keyboard input. An unlocked interactive desktop and appropriate integrity level
are still required; configuration alone does not prove a global hotkey fired.

## Linux helper with an isolated C runtime

On older Linux hosts, the upstream prebuilt helper may require a newer glibc.
Provision a verified, separate glibc runtime in an administrator-owned data directory,
then write `linux-runtime.json` beside `environment.json` in CU state:

```json
{
  "loader": "/absolute/compat/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
  "libraryPath": ["/absolute/compat/usr/lib/x86_64-linux-gnu"]
}
```

The CU host redirects only `spawn` of the exact managed `linux-bridge` path to
`loader --library-path <paths> <original-helper> <args>`. It preserves argv, stdio,
signals/child handles, and the graphical-session environment. The original Pi
installer continues to verify/copy the unchanged helper; no wrapper occupies its
install path, and no UI behavior is forked. Other child commands, the Executor,
clipboard, and system libc are unaffected. No shell evaluation or global
LD_LIBRARY_PATH is used. Runtime paths must be absolute and available before use;
invalid configuration fails closed. Non-Linux hosts ignore this file. Finish the
old CU session before changing it; each new host reads it again.

The runtime is node-local administration, not remote tool input. Pin and verify
its distribution package digest, retain its license notices, and maintain security
updates independently. Debian 10 / glibc 2.28 has been tested to start the original
0.5.1 helper with the Ubuntu 2.39-0ubuntu8.9 libc6 runtime; desktop acceptance also
requires valid X11/AT-SPI access. Do not replace system libc or place a shell wrapper
at the helper install path: the original plugin installer overwrites that path.

## Independently released host

`cu-host-vVERSION` tags publish a platform-neutral JSON bundle and SHA-256 sidecar.
The bundle contains only host modules and a release manifest (about 15 KB at 0.1.0).
`vVERSION` tags continue publishing the full compatible platform runtime.

Install with the **managed runtime's Node**, never an arbitrary system Node:

```sh
node scripts/install-computer-use-host.mjs HOST.json SHA256 CU_STATE DATA_ROOT
```

The installer checks the artifact digest, every file, protocol 1, exact Node version
and dependency-lock digest before atomically selecting `CU_STATE/host-root`. It
records `host-previous`; rollback uses `--rollback - CU_STATE DATA_ROOT`. Concurrent
installs fail on an exclusive lock. A failed validation leaves the selection intact.
No symlink/junction privileges or npm installation are required. Existing sessions
retain their process; the next host start reads the new selection. Executor status
reports the actual running host identity separately from installed state.

For selected Fabric nodes, use `python3 scripts/deploy-computer-use-host.py --file
FABRIC --artifact HOST.json --sha256 SHA256`. It transfers only the bundle and the
installer, reuses the installed runtime, and does not restart services. Upgrade all
cores to a host-selection-aware release before using this entrypoint. The existing
full runtime remains the fallback when no host selection exists.

Desktop maintenance uses the same durable queue authority:
`desktop.maintenance {executorId, owner, enabled:true}` pauses new grants while the
current session may finish. New submissions remain FIFO-queued; bare GUI writes are
rejected. `desktop.list.safePoint` is true only while maintenance is held, no owner or
action is active, and the desktop is not quarantined. Only the maintenance owner may
release it. The gate survives an Executor restart. Host deployment acquires this gate,
waits up to two minutes for cleanup, switches the host, then releases the gate. Timeout
changes no host selection. This is a desktop gate, not a claim that arbitrary long-lived
product processes have stopped; whole-node upgrades still require product/task draining.

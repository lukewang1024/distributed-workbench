# Computer Use through the upstream Pi extension

Workbench runs the **unmodified** `@injaneity/pi-computer-use` 0.5.1 package with
Pi's official extension loader (SDK 0.85.1). `computer-use/package-lock.json`
pins the complete dependency tree and npm integrity hashes. No platform source
is vendored, translated or maintained here. Upstream MIT notices ship in the
npm package. An official Node 24.14.0 runtime is pinned by SHA-256.

The adapter owns only transport, lifecycle, schema validation and the Executor
lease. Screenshots, accessibility trees, input, stale observations, UI refs,
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
workbench computer-use open --executor <executor-id> --owner <unique-session>
workbench computer-use call --executor <executor-id> --owner <unique-session> --token <returned-token> find_roots '{}'
workbench computer-use call --executor <executor-id> --owner <unique-session> --token <returned-token> observe_ui '{"root":"@r1","mode":"fused"}'
workbench computer-use call --executor <executor-id> --owner <unique-session> --token <returned-token> act_ui '{"stateId":"<observed-state>","actions":[{"action":"press","ref":"@e3"}]}'
workbench computer-use close --executor <executor-id> --owner <unique-session> --token <returned-token>
```

Use the `tools` response as the authoritative original schemas. Long JSON can be
passed on stdin with `-`. A session holds one exclusive `computer-use:<executor>`
lease (15 minutes by default); renew before expiry. Do not mix this session with
legacy `ui.input` / `ui.automate` clients on the same desktop. Release workspace
and runtime ownership before acquiring desktop control.

All plugin tool calls, including observations, require this lease because native
helper initialization may affect the desktop. Schema listing does not. A new
session destroys old observation state. The executor serializes requests and
never retries an uncertain input delivery. After a disconnect, re-observe before
acting; do not replay a click or submit operation. `close` ends the host and
native helper children. Parent disconnect or 15 minutes of inactivity also ends
the host. macOS's upstream LaunchServices helper may remain available for reuse.

The original tool result includes text, structured details, and image content
blocks (`type=image`, base64 `data`, `mimeType`). Preserve screenshots directly;
do not render a synthetic replacement. A successful transport is not proof of
UI success: inspect the plugin's reported action outcome and successor state.

## Platform requirements

| Platform | Upstream backend | Requirements/limits |
| --- | --- | --- |
| macOS | AX + native capture helper | macOS 14+, Accessibility and Screen Recording grants for the upstream helper app |
| Windows | UI Automation + native input/capture | Native interactive user desktop; workbench launches the host into the active session. Locked/secure desktops or integrity-level mismatches can prevent physical input. |
| Linux X11 | AT-SPI2 + XComposite/XTEST | Executor must inherit the target user's DISPLAY, XAUTHORITY and accessibility/session bus environment |
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
```

The smoke test uses isolated temporary Controller/Executor instances and the real
upstream extension, proving tool discovery, lease exclusion, stale-state rejection,
shutdown and stale-token rejection. It does not claim GUI acceptance on any OS.
Live acceptance must separately observe an actual window, perform an authorized
interaction, check the successor state and save its screenshot.

Reference: https://github.com/injaneity/pi-computer-use

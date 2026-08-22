#!/bin/sh
set -eu

usage() {
  printf '%s\n' \
    'usage: scripts/bootstrap-fabric.sh [--version VERSION] [--local-id ID] [--skip-release-install] [--verify-only] HOST|windows:HOST ...' \
    '' \
    'Install or verify distributed-workbench on selected SSH hosts, then register' \
    'their executors with the laptop Controller. Prefix native Windows nodes' \
    'with windows:. Unprefixed HOST entries are POSIX nodes.'
}

version=latest
local_id=$(hostname -s)
local_id_explicit=false
verify_only=false
skip_release_install=false
while [ "$#" -gt 0 ]; do
  case $1 in
    --version)
      test "$#" -ge 2 || { usage >&2; exit 2; }
      version=$2
      shift 2
      ;;
    --local-id)
      test "$#" -ge 2 || { usage >&2; exit 2; }
      local_id=$2
      local_id_explicit=true
      shift 2
      ;;
    --verify-only)
      verify_only=true
      shift
      ;;
    --skip-release-install)
      skip_release_install=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --*)
      printf 'bootstrap-fabric: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
    *) break ;;
  esac
done

test "$#" -gt 0 || { usage >&2; exit 2; }
nodes=
windows_nodes=
for spec in "$@"; do
  case $spec in
    windows:*) host=${spec#windows:}; windows_nodes="$windows_nodes $host" ;;
    *) host=$spec ;;
  esac
  case $host in
    *[!0-9A-Za-z._-]*|'')
      printf 'bootstrap-fabric: HOST must be an SSH config alias: %s\n' "$host" >&2
      exit 2
      ;;
  esac
  nodes="$nodes $host"
done
# Host aliases are restricted to safe, whitespace-free characters above.
# shellcheck disable=SC2086
set -- $nodes
case $version in
  v*) version=${version#v} ;;
esac
if [ "$version" = latest ]; then
  repository=${DISTRIBUTED_WORKBENCH_REPOSITORY:-lukewang1024/distributed-workbench}
  version=$(curl -fsSL "https://api.github.com/repos/$repository/releases/latest" |
    sed -n 's/.*"tag_name":[[:space:]]*"v\([^"]*\)".*/\1/p' | head -1)
fi
case $local_id in
  *[!0-9A-Za-z._-]*|'')
    printf 'bootstrap-fabric: invalid local id: %s\n' "$local_id" >&2
    exit 2
    ;;
esac
case $version in
  *[!0-9A-Za-z._-]*|'')
    printf 'bootstrap-fabric: invalid version: %s\n' "$version" >&2
    exit 2
    ;;
esac

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
installer=$script_dir/install-from-release.sh
test -f "$installer" || { printf 'bootstrap-fabric: missing %s\n' "$installer" >&2; exit 2; }

state_home=${XDG_STATE_HOME:-"$HOME/.local/state"}
controller_socket=$state_home/distributed-workbench/controller.sock
executor_socket=$state_home/distributed-workbench/executor.sock
peer_root=$state_home/distributed-workbench/peers
launch_agents=$HOME/Library/LaunchAgents
peer_template=$script_dir/../packaging/dev.distributed-workbench.peer.plist.in
remote_peer_template=$script_dir/../packaging/distributed-workbench-peer.service.in
app_binary=${XDG_DATA_HOME:-"$HOME/.local/share"}/distributed-workbench/Agent\ Workbench.app/Contents/MacOS/workbench-macos-agent
if [ -n "${DISTRIBUTED_WORKBENCH_BINARY:-}" ]; then
  workbench=$DISTRIBUTED_WORKBENCH_BINARY
elif [ -x "$app_binary" ]; then
  workbench=$app_binary
elif [ -x "$HOME/.local/bin/workbench" ]; then
  workbench=$HOME/.local/bin/workbench
else
  printf '%s\n' 'bootstrap-fabric: install distributed-workbench on this laptop first' >&2
  exit 1
fi
test -x "$workbench" || { printf 'bootstrap-fabric: executable not found: %s\n' "$workbench" >&2; exit 1; }
test -f "$peer_template" || { printf 'bootstrap-fabric: missing %s\n' "$peer_template" >&2; exit 2; }
test -f "$remote_peer_template" || { printf 'bootstrap-fabric: missing %s\n' "$remote_peer_template" >&2; exit 2; }

release_cache=$(mktemp -d "${TMPDIR:-/tmp}/distributed-workbench-fabric.XXXXXX")
trap 'rm -rf "$release_cache"' EXIT HUP INT TERM

install_linux_release() {
  install_host=$1
  install_node_id=$2
  install_executor_id=$3
  repository=${DISTRIBUTED_WORKBENCH_REPOSITORY:-lukewang1024/distributed-workbench}
  target=x86_64-unknown-linux-musl
  archive=distributed-workbench-$version-$target.tar.gz
  base=https://github.com/$repository/releases/download/v$version
  if [ ! -f "$release_cache/$archive" ]; then
    curl -fsSL "$base/$archive" -o "$release_cache/$archive"
    curl -fsSL "$base/SHA256SUMS" -o "$release_cache/SHA256SUMS"
    expected=$(awk -v name="$archive" '$2 == name {print $1}' "$release_cache/SHA256SUMS")
    test -n "$expected" || { printf '%s\n' 'bootstrap-fabric: checksum missing for Linux release' >&2; return 1; }
    actual=$(shasum -a 256 "$release_cache/$archive" | awk '{print $1}')
    test "$actual" = "$expected" || { printf '%s\n' 'bootstrap-fabric: Linux release checksum mismatch' >&2; return 1; }
  fi
  remote_archive=/tmp/$archive.$$
  scp -q "$release_cache/$archive" "$install_host:$remote_archive"
  ssh -o BatchMode=yes -o ClearAllForwardings=yes "$install_host" \
    "set -eu; temporary=\$(mktemp -d /tmp/distributed-workbench-install.XXXXXX); trap 'rm -rf \"\$temporary\" \"$remote_archive\"' EXIT HUP INT TERM; tar -C \"\$temporary\" -xzf \"$remote_archive\"; root=\"\$temporary/distributed-workbench-$version-$target\"; cd \"\$root\"; DISTRIBUTED_WORKBENCH_CONTROLLER_ID='$install_node_id' scripts/install-linux-user.sh bin/workbench '$install_executor_id' \"\$HOME/Code\" \"\$HOME/Workspace\" \"\${XDG_STATE_HOME:-\$HOME/.local/state}\"" \
    >/dev/null
}

installed_version=$("$workbench" --version | awk 'NR == 1 {print $2}')
if [ "$installed_version" != "$version" ]; then
  if [ "$verify_only" = true ] || [ "$skip_release_install" = true ]; then
    printf 'bootstrap-fabric: laptop version is %s, expected %s\n' "$installed_version" "$version" >&2
    exit 1
  fi
  printf 'bootstrap-fabric: laptop: installing %s\n' "$version"
  DISTRIBUTED_WORKBENCH_NODE_ID=$local_id "$installer" "$version" >/dev/null
  workbench=$app_binary
fi

local_status=$("$workbench" --socket "$controller_socket" status)
local_executor_status=$("$workbench" --socket "$executor_socket" status)
local_executor_id=$(printf '%s\n' "$local_executor_status" |
  sed -n 's/.*"executorId":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
test -n "$local_executor_id" || {
  printf '%s\n' 'bootstrap-fabric: local Executor did not report an identity' >&2
  exit 1
}
actual_controller_id=$(printf '%s\n' "$local_status" |
  sed -n '/"controller":[[:space:]]*{/,/}/{s/.*"id":[[:space:]]*"\([^"]*\)".*/\1/p;}' | head -1)
test -n "$actual_controller_id" || {
  printf '%s\n' 'bootstrap-fabric: local Controller did not report an identity' >&2
  exit 1
}
if [ "$local_id_explicit" = false ]; then
  local_id=$actual_controller_id
elif [ "$actual_controller_id" != "$local_id" ]; then
  printf 'bootstrap-fabric: local id %s does not match Controller identity %s\n' "$local_id" "$actual_controller_id" >&2
  exit 1
fi

escape_sed() {
  printf '%s' "$1" | sed 's/[\\&|]/\\&/g'
}

escape_json() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

install_peer_service() {
  peer_host=$1
  remote_home=$2
  remote_platform=$3
  peer_dir=$peer_root/$peer_host
  expose_controller=$peer_dir/controller.sock
  expose_executor=$peer_dir/executor.sock
  peer_state=$peer_dir/status.json
  peer_log=$peer_dir/peer.log
  if [ "$remote_platform" = windows ]; then
    remote_state_root='C:\ProgramData\distributed-workbench'
    remote_executable='C:\Program Files\distributed-workbench\workbench.exe'
  else
    remote_state_root=$remote_home/.local/state/distributed-workbench
    remote_executable=.local/bin/workbench
  fi
  plist=$launch_agents/dev.distributed-workbench.peer.$peer_host.plist
  mkdir -p "$peer_dir" "$launch_agents"
  plist_temporary=$(mktemp "$plist.tmp.XXXXXX")
  sed \
    -e "s|@EXECUTABLE@|$(escape_sed "$workbench")|g" \
    -e "s|@PEER_ID@|$(escape_sed "$peer_host")|g" \
    -e "s|@LOCAL_ID@|$(escape_sed "$local_id")|g" \
    -e "s|@HOST@|$(escape_sed "$peer_host")|g" \
    -e "s|@EXPOSE_CONTROLLER_SOCKET@|$(escape_sed "$expose_controller")|g" \
    -e "s|@EXPOSE_EXECUTOR_SOCKET@|$(escape_sed "$expose_executor")|g" \
    -e "s|@REMOTE_STATE_ROOT@|$(escape_sed "$remote_state_root")|g" \
    -e "s|@REMOTE_EXECUTABLE@|$(escape_sed "$remote_executable")|g" \
    -e "s|@REMOTE_PLATFORM@|$remote_platform|g" \
    -e "s|@STATE_PATH@|$(escape_sed "$peer_state")|g" \
    -e "s|@LOG_PATH@|$(escape_sed "$peer_log")|g" \
    "$peer_template" >"$plist_temporary"
  plutil -lint "$plist_temporary" >/dev/null
  mv "$plist_temporary" "$plist"
  if [ -f "$peer_state" ]; then mv "$peer_state" "$peer_state.previous"; fi
  domain=gui/$(id -u)
  launchctl bootout "$domain/dev.distributed-workbench.peer.$peer_host" 2>/dev/null || true
  launchctl bootstrap "$domain" "$plist"
}

reconcile_local_peer_services() {
  domain=gui/$(id -u)
  for stale_plist in "$launch_agents"/dev.distributed-workbench.peer.*.plist; do
    test -f "$stale_plist" || continue
    stale_name=${stale_plist##*/dev.distributed-workbench.peer.}
    stale_peer=${stale_name%.plist}
    case " $nodes " in
      *" $stale_peer "*) continue ;;
    esac
    case $stale_peer in
      *[!0-9A-Za-z._-]*|'')
        printf 'bootstrap-fabric: refusing invalid managed peer name: %s\n' "$stale_peer" >&2
        return 1
        ;;
    esac
    launchctl bootout "$domain/dev.distributed-workbench.peer.$stale_peer" 2>/dev/null || true
    rm -f "$stale_plist"
    stale_dir=$peer_root/$stale_peer
    if [ -d "$stale_dir" ]; then find "$stale_dir" -depth -delete; fi
    "$workbench" --socket "$controller_socket" call controller.unregister \
      "{\"controllerId\":\"$stale_peer\"}" >/dev/null
    "$workbench" --socket "$controller_socket" call executor.unregister \
      "{\"executorId\":\"$stale_peer-rust\"}" >/dev/null
    "$workbench" --socket "$controller_socket" call executor.unregister \
      "{\"executorId\":\"$stale_peer-native\"}" >/dev/null
    printf 'bootstrap-fabric: removed unselected local peer service: %s\n' "$stale_peer"
  done
}

reconcile_remote_posix_peer_services() {
  reconcile_host=$1
  selected_peers="$local_id$nodes"
  ssh -o BatchMode=yes -o ClearAllForwardings=yes "$reconcile_host" \
    "set -eu; selected=' $selected_peers '; root=\"\${XDG_STATE_HOME:-\$HOME/.local/state}/distributed-workbench\"; state=\"\$root/peers\"; unit_root=\"\$HOME/.config/systemd/user\"; for unit_path in \$(find \"\$unit_root\" -maxdepth 1 -type f -name 'distributed-workbench-peer-*.service' -print); do unit=\${unit_path##*/}; peer=\${unit#distributed-workbench-peer-}; peer=\${peer%.service}; case \"\$peer\" in *[!0-9A-Za-z._-]*|'') echo \"invalid managed peer name: \$peer\" >&2; exit 1;; esac; case \"\$selected\" in *\" \$peer \"*) continue;; esac; systemctl --user disable --now \"\$unit\" >/dev/null 2>&1 || true; rm -f \"\$unit_path\"; rm -rf \"\$state/\$peer\"; \"\$HOME/.local/bin/workbench\" --socket \"\$root/controller.sock\" call controller.unregister \"{\\\"controllerId\\\":\\\"\$peer\\\"}\" >/dev/null; \"\$HOME/.local/bin/workbench\" --socket \"\$root/controller.sock\" call executor.unregister \"{\\\"executorId\\\":\\\"\$peer-rust\\\"}\" >/dev/null; \"\$HOME/.local/bin/workbench\" --socket \"\$root/controller.sock\" call executor.unregister \"{\\\"executorId\\\":\\\"\$peer-native\\\"}\" >/dev/null; echo \"bootstrap-fabric: removed unselected peer service on $reconcile_host: \$peer\"; done; systemctl --user daemon-reload"
  ssh -o BatchMode=yes -o ClearAllForwardings=yes "$reconcile_host" \
    "set -eu; selected=' $selected_peers '; root=\"\${XDG_STATE_HOME:-\$HOME/.local/state}/distributed-workbench\"; wb=\"\$HOME/.local/bin/workbench\"; controllers=\$(\"\$wb\" --socket \"\$root/controller.sock\" call controller.list); for peer in \$(printf '%s\\n' \"\$controllers\" | sed -n 's/.*\"id\":[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p'); do case \"\$selected\" in *\" \$peer \"*) continue;; esac; \"\$wb\" --socket \"\$root/controller.sock\" call controller.unregister \"{\\\"controllerId\\\":\\\"\$peer\\\"}\" >/dev/null; echo \"bootstrap-fabric: removed unselected Controller registration on $reconcile_host: \$peer\"; done; executors=\$(\"\$wb\" --socket \"\$root/controller.sock\" call executor.list); for executor in \$(printf '%s\\n' \"\$executors\" | sed -n 's/.*\"id\":[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p'); do case \"\$executor\" in *-rust) peer=\${executor%-rust};; *-native) peer=\${executor%-native};; *) continue;; esac; case \"\$selected\" in *\" \$peer \"*) continue;; esac; \"\$wb\" --socket \"\$root/controller.sock\" call executor.unregister \"{\\\"executorId\\\":\\\"\$executor\\\"}\" >/dev/null; echo \"bootstrap-fabric: removed unselected Executor registration on $reconcile_host: \$executor\"; done"
}

platform_of() {
  candidate=$1
  for windows_node in $windows_nodes; do
    if [ "$candidate" = "$windows_node" ]; then
      printf '%s\n' windows
      return
    fi
  done
  printf '%s\n' posix
}

windows_call() {
  call_host=$1
  call_socket=$2
  call_action=$3
  call_json=$4
  request=$(printf '{"apiVersion":"workbench.dev/v1","requestId":"req_bootstrap_fabric","action":"%s","params":%s}' "$call_action" "$call_json")
  encoded=$(printf '%s' "$request" | base64 | tr -d '\n')
  ssh -o BatchMode=yes -o ClearAllForwardings=yes "$call_host" \
    "powershell.exe -NoProfile -NonInteractive -Command \"\$request=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$encoded')); \$psi=New-Object System.Diagnostics.ProcessStartInfo; \$psi.FileName='C:\\Program Files\\distributed-workbench\\workbench.exe'; \$psi.Arguments='--socket $call_socket call-stdin'; \$psi.UseShellExecute=\$false; \$psi.RedirectStandardInput=\$true; \$psi.RedirectStandardOutput=\$true; \$psi.RedirectStandardError=\$true; \$p=[Diagnostics.Process]::Start(\$psi); \$p.StandardInput.WriteLine(\$request); \$p.StandardInput.Close(); \$stdout=\$p.StandardOutput.ReadToEnd(); \$stderr=\$p.StandardError.ReadToEnd(); \$p.WaitForExit(); [Console]::Out.Write(\$stdout); [Console]::Error.Write(\$stderr); if (\$p.ExitCode -ne 0) { exit \$p.ExitCode }\""
}

remote_call() {
  call_host=$1
  call_platform=$2
  call_action=$3
  call_json=$4
  if [ "$call_platform" = windows ]; then
    windows_call "$call_host" 'C:\ProgramData\distributed-workbench\controller.sock' "$call_action" "$call_json"
  else
    ssh -o BatchMode=yes -o ClearAllForwardings=yes "$call_host" \
      "\"\$HOME/.local/bin/workbench\" --socket \"\${XDG_STATE_HOME:-\$HOME/.local/state}/distributed-workbench/controller.sock\" call '$call_action' '$call_json'"
  fi
}

wait_peer_ready() {
  peer_host=$1
  peer_state=$peer_root/$peer_host/status.json
  attempt=0
  while [ "$attempt" -lt 200 ]; do
    if [ -f "$peer_state" ] && "$workbench" peer status --state "$peer_state" 2>/dev/null | grep '"state": "ready"' >/dev/null; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  printf 'bootstrap-fabric: peer did not become ready: %s\n' "$peer_host" >&2
  return 1
}

peer_generation() {
  "$workbench" peer status --state "$peer_root/$1/status.json" 2>/dev/null |
    sed -n 's/.*"generation":[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -1
}

wait_peer_generation() {
  peer_host=$1
  previous_generation=$2
  attempt=0
  while [ "$attempt" -lt 200 ]; do
    current_generation=$(peer_generation "$peer_host")
    if [ -n "$current_generation" ] && [ "$current_generation" -gt "$previous_generation" ] \
      && "$workbench" peer status --state "$peer_root/$peer_host/status.json" 2>/dev/null | grep '"state": "ready"' >/dev/null; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  printf 'bootstrap-fabric: peer did not reconnect with a newer generation: %s\n' "$peer_host" >&2
  return 1
}

install_remote_peer_service() {
  dialer=$1
  peer=$2
  dialer_home=$3
  peer_home=$4
  peer_platform=$5
  remote_state=$dialer_home/.local/state/distributed-workbench
  remote_peer_dir=$remote_state/peers/$peer
  expose_controller=$remote_peer_dir/controller.sock
  expose_executor=$remote_peer_dir/executor.sock
  remote_status_path=$remote_peer_dir/status.json
  if [ "$peer_platform" = windows ]; then
    peer_state_root='C:\ProgramData\distributed-workbench'
    peer_executable='C:\Program Files\distributed-workbench\workbench.exe'
  else
    peer_state_root=$peer_home/.local/state/distributed-workbench
    peer_executable=.local/bin/workbench
  fi
  unit=distributed-workbench-peer-$peer.service
  sed \
    -e "s|@BINARY@|$(escape_sed "$dialer_home/.local/bin/workbench")|g" \
    -e "s|@PEER_ID@|$peer|g" \
    -e "s|@LOCAL_ID@|$dialer|g" \
    -e "s|@HOST@|$peer|g" \
    -e "s|@EXPOSE_CONTROLLER_SOCKET@|$(escape_sed "$expose_controller")|g" \
    -e "s|@EXPOSE_EXECUTOR_SOCKET@|$(escape_sed "$expose_executor")|g" \
    -e "s|@REMOTE_STATE_ROOT@|$(escape_sed "$peer_state_root")|g" \
    -e "s|@REMOTE_EXECUTABLE@|$(escape_sed "$peer_executable")|g" \
    -e "s|@REMOTE_PLATFORM@|$peer_platform|g" \
    -e "s|@STATE_PATH@|$(escape_sed "$remote_status_path")|g" \
    "$remote_peer_template" | ssh -o BatchMode=yes -o ClearAllForwardings=yes "$dialer" \
      "mkdir -p '$remote_peer_dir' \"\$HOME/.config/systemd/user\"; cat >\"\$HOME/.config/systemd/user/$unit\"; if [ -f '$remote_status_path' ]; then mv '$remote_status_path' '$remote_status_path.previous'; fi; systemctl --user daemon-reload; systemctl --user enable --now '$unit'; systemctl --user restart '$unit'"
  attempt=0
  while [ "$attempt" -lt 200 ]; do
    if ssh -o BatchMode=yes -o ClearAllForwardings=yes "$dialer" \
      "\"\$HOME/.local/bin/workbench\" peer status --state '$remote_status_path'" 2>/dev/null | grep '"state": "ready"' >/dev/null; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  printf 'bootstrap-fabric: peer transport %s -> %s did not become ready\n' "$dialer" "$peer" >&2
  return 1
}

install_windows_peer_service() {
  dialer=$1
  peer=$2
  peer_platform=$3
  if [ "$peer_platform" = windows ]; then
    peer_state_root='C:\ProgramData\distributed-workbench'
    peer_executable='C:\Program Files\distributed-workbench\workbench.exe'
  else
    peer_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$peer" 'printf %s "$HOME"')
    peer_state_root=$peer_home/.local/state/distributed-workbench
    peer_executable=.local/bin/workbench
  fi
  scp -q "$script_dir/install-windows-peer.ps1" "$dialer:install-distributed-workbench-peer.ps1"
  ssh -o BatchMode=yes -o ClearAllForwardings=yes "$dialer" \
    "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"& './install-distributed-workbench-peer.ps1' -PeerId '$peer' -LocalId '$dialer' -HostName '$peer' -RemotePlatform '$peer_platform' -RemoteExecutable '$peer_executable' -RemoteStateRoot '$peer_state_root'; Remove-Item './install-distributed-workbench-peer.ps1' -Force -ErrorAction SilentlyContinue\"" \
    >/dev/null
}

wait_remote_peer_ready() {
  dialer=$1
  dialer_platform=$2
  dialer_home=$3
  peer=$4
  attempt=0
  while [ "$attempt" -lt 200 ]; do
    if [ "$dialer_platform" = windows ]; then
      output=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$dialer" \
        "powershell.exe -NoProfile -NonInteractive -Command \"& 'C:\Program Files\distributed-workbench\workbench.exe' peer status --state 'C:\ProgramData\distributed-workbench\peers\$peer\status.json'\"" 2>/dev/null || true)
    else
      output=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$dialer" \
        "\"\$HOME/.local/bin/workbench\" peer status --state '$dialer_home/.local/state/distributed-workbench/peers/$peer/status.json'" 2>/dev/null || true)
    fi
    if printf '%s' "$output" | grep '"state": "ready"' >/dev/null; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  printf 'bootstrap-fabric: peer transport %s -> %s did not become ready\n' "$dialer" "$peer" >&2
  return 1
}

remote_peer_status() {
  status_dialer=$1
  status_platform=$2
  status_home=$3
  status_peer=$4
  if [ "$status_platform" = windows ]; then
    ssh -o BatchMode=yes -o ClearAllForwardings=yes "$status_dialer" \
      "powershell.exe -NoProfile -NonInteractive -Command \"& 'C:\Program Files\distributed-workbench\workbench.exe' peer status --state 'C:\ProgramData\distributed-workbench\peers\$status_peer\status.json'\""
  else
    ssh -o BatchMode=yes -o ClearAllForwardings=yes "$status_dialer" \
      "\"\$HOME/.local/bin/workbench\" peer status --state '$status_home/.local/state/distributed-workbench/peers/$status_peer/status.json'"
  fi
}

wait_remote_peer_generation() {
  generation_dialer=$1
  generation_platform=$2
  generation_home=$3
  generation_peer=$4
  previous_generation=$5
  attempt=0
  while [ "$attempt" -lt 200 ]; do
    output=$(remote_peer_status "$generation_dialer" "$generation_platform" "$generation_home" "$generation_peer" 2>/dev/null || true)
    current_generation=$(printf '%s\n' "$output" |
      sed -n 's/.*"generation":[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -1)
    if [ -n "$current_generation" ] && [ "$current_generation" -gt "$previous_generation" ] \
      && printf '%s\n' "$output" | grep '"state": "ready"' >/dev/null; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  printf 'bootstrap-fabric: peer transport %s -> %s did not reconnect with a newer generation\n' "$generation_dialer" "$generation_peer" >&2
  return 1
}

if [ "$verify_only" = false ]; then
  reconcile_local_peer_services
fi

for host in "$@"; do
  host_platform=$(platform_of "$host")
  if [ "$host_platform" = windows ]; then
    executor_id=$host-native
  else
    executor_id=$host-rust
  fi
  printf 'bootstrap-fabric: %s: checking SSH\n' "$host"
  if [ "$host_platform" = windows ]; then
    ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" \
      'powershell.exe -NoProfile -NonInteractive -Command "Write-Output ready"' >/dev/null
    remote_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" \
      'powershell.exe -NoProfile -NonInteractive -Command "[Environment]::GetFolderPath('"'"'UserProfile'"'"')"' | tr -d '\r')
  else
    ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" 'printf ready' >/dev/null
    remote_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" 'printf %s "$HOME"')
  fi

  if [ "$verify_only" = false ] && [ "$skip_release_install" = false ]; then
    printf 'bootstrap-fabric: %s: installing %s\n' "$host" "$version"
    if [ "$host_platform" = windows ]; then
      scp -q "$script_dir/install-from-release.ps1" "$host:install-distributed-workbench.ps1"
      ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" \
      "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"& './install-distributed-workbench.ps1' -Version '$version' -NodeId '$host'; Remove-Item './install-distributed-workbench.ps1' -Force -ErrorAction SilentlyContinue\"" \
        >/dev/null
    else
      install_linux_release "$host" "$host" "$executor_id"
    fi
  fi

  if [ "$verify_only" = false ] && [ "$host_platform" = posix ]; then
    reconcile_remote_posix_peer_services "$host"
  fi

  if [ "$host_platform" = windows ]; then
    remote_version=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" \
      "powershell.exe -NoProfile -NonInteractive -Command \"& 'C:\\Program Files\\distributed-workbench\\workbench.exe' --version\"" | tr -d '\r' | awk 'NR == 1 {print $2}')
  else
    remote_version=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" \
      '"$HOME/.local/bin/workbench" --version' | awk 'NR == 1 {print $2}')
  fi
  if [ "$remote_version" != "$version" ]; then
    printf 'bootstrap-fabric: %s version is %s, expected %s\n' "$host" "$remote_version" "$version" >&2
    exit 1
  fi

  if [ "$host_platform" = windows ]; then
    ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" \
      "powershell.exe -NoProfile -NonInteractive -Command \"& 'C:\Program Files\distributed-workbench\workbench.exe' --socket 'C:\ProgramData\distributed-workbench\executor.sock' status\"" \
      >/dev/null
  else
    ssh -o BatchMode=yes -o ClearAllForwardings=yes "$host" \
      'mkdir -p "${XDG_STATE_HOME:-$HOME/.local/state}/distributed-workbench/fabric"; "$HOME/.local/bin/workbench" --socket "${XDG_STATE_HOME:-$HOME/.local/state}/distributed-workbench/executor.sock" status' \
      >/dev/null
  fi

  if [ "$verify_only" = false ]; then
    install_peer_service "$host" "$remote_home" "$host_platform"
  fi
  wait_peer_ready "$host"
  peer_executor=$peer_root/$host/executor.sock
  peer_controller=$peer_root/$host/controller.sock
  params=$(printf '{"executorId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$executor_id" "$(escape_json "$peer_executor")")
  if [ "$verify_only" = false ]; then
    "$workbench" --socket "$controller_socket" call executor.register "$params" >/dev/null
    controller_params=$(printf '{"controllerId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$host" "$(escape_json "$peer_controller")")
    "$workbench" --socket "$controller_socket" call controller.register "$controller_params" >/dev/null
    if [ "$host_platform" = windows ]; then
      reverse_executor="C:\ProgramData\distributed-workbench\fabric\\${local_id}-executor.sock"
      reverse_controller="C:\ProgramData\distributed-workbench\fabric\\${local_id}-controller.sock"
    else
      reverse_executor=$remote_home/.local/state/distributed-workbench/fabric/$local_id-executor.sock
      reverse_controller=$remote_home/.local/state/distributed-workbench/fabric/$local_id-controller.sock
    fi
    local_params=$(printf '{"executorId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$local_executor_id" "$(escape_json "$reverse_executor")")
    local_controller_params=$(printf '{"controllerId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$local_id" "$(escape_json "$reverse_controller")")
    remote_call "$host" "$host_platform" executor.register "$local_params" >/dev/null
    remote_call "$host" "$host_platform" controller.register "$local_controller_params" >/dev/null
  fi
  printf 'bootstrap-fabric: %s: ready as %s\n' "$host" "$executor_id"
done

for node_a in "$@"; do
  for node_b in "$@"; do
    if [ "$node_a" = "$node_b" ]; then
      continue
    fi
    platform_a=$(platform_of "$node_a")
    platform_b=$(platform_of "$node_b")
    if [ "$platform_a" != "$platform_b" ]; then
      if [ "$platform_a" = posix ]; then first=$node_a; else first=$node_b; fi
    else
      first=$(printf '%s\n%s\n' "$node_a" "$node_b" | LC_ALL=C sort | head -1)
    fi
    if [ "$node_a" != "$first" ]; then
      continue
    fi
    dialer=$node_a
    peer=$node_b
    dialer_platform=$(platform_of "$dialer")
    peer_platform=$(platform_of "$peer")
    if [ "$dialer_platform" = windows ]; then
      dialer_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$dialer" \
        'powershell.exe -NoProfile -NonInteractive -Command "[Environment]::GetFolderPath('"'"'UserProfile'"'"')"' | tr -d '\r')
      peer_executor="C:\ProgramData\distributed-workbench\peers\$peer\executor.sock"
      peer_controller="C:\ProgramData\distributed-workbench\peers\$peer\controller.sock"
    else
      dialer_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$dialer" 'printf %s "$HOME"')
      peer_executor=$dialer_home/.local/state/distributed-workbench/peers/$peer/executor.sock
      peer_controller=$dialer_home/.local/state/distributed-workbench/peers/$peer/controller.sock
    fi
    if [ "$peer_platform" = windows ]; then
      peer_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$peer" \
        'powershell.exe -NoProfile -NonInteractive -Command "[Environment]::GetFolderPath('"'"'UserProfile'"'"')"' | tr -d '\r')
      reverse_executor="C:\ProgramData\distributed-workbench\fabric\\${dialer}-executor.sock"
      reverse_controller="C:\ProgramData\distributed-workbench\fabric\\${dialer}-controller.sock"
    else
      peer_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$peer" 'printf %s "$HOME"')
      reverse_executor=$peer_home/.local/state/distributed-workbench/fabric/$dialer-executor.sock
      reverse_controller=$peer_home/.local/state/distributed-workbench/fabric/$dialer-controller.sock
    fi
    if [ "$verify_only" = false ]; then
      if [ "$dialer_platform" = windows ]; then
        install_windows_peer_service "$dialer" "$peer" "$peer_platform"
        wait_remote_peer_ready "$dialer" "$dialer_platform" "$dialer_home" "$peer"
      else
        install_remote_peer_service "$dialer" "$peer" "$dialer_home" "$peer_home" "$peer_platform"
      fi
      if [ "$peer_platform" = windows ]; then peer_executor_id=$peer-native; else peer_executor_id=$peer-rust; fi
      if [ "$dialer_platform" = windows ]; then dialer_executor_id=$dialer-native; else dialer_executor_id=$dialer-rust; fi
      peer_executor_params=$(printf '{"executorId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$peer_executor_id" "$(escape_json "$peer_executor")")
      peer_controller_params=$(printf '{"controllerId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$peer" "$(escape_json "$peer_controller")")
      dialer_executor_params=$(printf '{"executorId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$dialer_executor_id" "$(escape_json "$reverse_executor")")
      dialer_controller_params=$(printf '{"controllerId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$dialer" "$(escape_json "$reverse_controller")")
      remote_call "$dialer" "$dialer_platform" executor.register "$peer_executor_params" >/dev/null
      remote_call "$dialer" "$dialer_platform" controller.register "$peer_controller_params" >/dev/null
      remote_call "$peer" "$peer_platform" executor.register "$dialer_executor_params" >/dev/null
      remote_call "$peer" "$peer_platform" controller.register "$dialer_controller_params" >/dev/null
    else
      wait_remote_peer_ready "$dialer" "$dialer_platform" "$dialer_home" "$peer"
    fi
  done
done

status=$("$workbench" --socket "$controller_socket" status)
for host in "$@"; do
  if [ "$(platform_of "$host")" = windows ]; then executor_id=$host-native; else executor_id=$host-rust; fi
  printf '%s\n' "$status" | grep '"id": '"\"$executor_id\"" >/dev/null || {
    printf 'bootstrap-fabric: executor missing after registration: %s\n' "$executor_id" >&2
    exit 1
  }
done
for host in "$@"; do
  host_platform=$(platform_of "$host")
  remote_status=$(remote_call "$host" "$host_platform" status '{}')
  printf '%s\n' "$remote_status" | grep '"id": '"\"$local_executor_id\"" >/dev/null || {
    printf 'bootstrap-fabric: laptop executor missing from %s controller\n' "$host" >&2
    exit 1
  }
  printf '%s\n' "$remote_status" | grep '"id": '"\"$local_id\"" >/dev/null || {
    printf 'bootstrap-fabric: laptop controller missing from %s controller\n' "$host" >&2
    exit 1
  }
  for service_host in "$@"; do
    if [ "$host" = "$service_host" ]; then
      continue
    fi
    if [ "$(platform_of "$service_host")" = windows ]; then service_executor_id=$service_host-native; else service_executor_id=$service_host-rust; fi
    printf '%s\n' "$remote_status" | grep '"id": '"\"$service_executor_id\"" >/dev/null || {
      printf 'bootstrap-fabric: %s executor missing from %s controller\n' "$service_executor_id" "$host" >&2
      exit 1
    }
  done
done

reported_controller_id() {
  sed -n '/"controller":[[:space:]]*{/,/}/{s/.*"id":[[:space:]]*"\([^"]*\)".*/\1/p;}' |
    head -1
}

verify_controller_routes() {
  for route_host in "$@"; do
    route_params=$(printf '{"controllerId":"%s","action":"status","params":{}}' "$route_host")
    route_output=$("$workbench" --socket "$controller_socket" call controller.call "$route_params")
    route_id=$(printf '%s\n' "$route_output" | reported_controller_id)
    test "$route_id" = "$route_host" || {
      printf 'bootstrap-fabric: laptop -> %s Controller route failed\n' "$route_host" >&2
      return 1
    }
    reverse_params=$(printf '{"controllerId":"%s","action":"status","params":{}}' "$local_id")
    reverse_output=$(remote_call "$route_host" "$(platform_of "$route_host")" controller.call "$reverse_params")
    reverse_id=$(printf '%s\n' "$reverse_output" | reported_controller_id)
    test "$reverse_id" = "$local_id" || {
      printf 'bootstrap-fabric: %s -> laptop Controller route failed\n' "$route_host" >&2
      return 1
    }
    for route_peer in "$@"; do
      if [ "$route_host" = "$route_peer" ]; then continue; fi
      peer_params=$(printf '{"controllerId":"%s","action":"status","params":{}}' "$route_peer")
      peer_output=$(remote_call "$route_host" "$(platform_of "$route_host")" controller.call "$peer_params")
      peer_id=$(printf '%s\n' "$peer_output" | reported_controller_id)
      test "$peer_id" = "$route_peer" || {
        printf 'bootstrap-fabric: %s -> %s Controller route failed\n' "$route_host" "$route_peer" >&2
        return 1
      }
    done
  done
}

verify_controller_routes "$@"

if [ "$verify_only" = false ]; then
  for reconnect_host in "$@"; do
    before_generation=$(peer_generation "$reconnect_host")
    test -n "$before_generation" || {
      printf 'bootstrap-fabric: missing peer generation before reconnect: %s\n' "$reconnect_host" >&2
      exit 1
    }
    launchctl kickstart -k "gui/$(id -u)/dev.distributed-workbench.peer.$reconnect_host"
    wait_peer_generation "$reconnect_host" "$before_generation"
  done

  for reconnect_a in "$@"; do
    for reconnect_b in "$@"; do
      if [ "$reconnect_a" = "$reconnect_b" ]; then continue; fi
      platform_a=$(platform_of "$reconnect_a")
      platform_b=$(platform_of "$reconnect_b")
      if [ "$platform_a" != "$platform_b" ]; then
        if [ "$platform_a" = posix ]; then reconnect_first=$reconnect_a; else reconnect_first=$reconnect_b; fi
      else
        reconnect_first=$(printf '%s\n%s\n' "$reconnect_a" "$reconnect_b" | LC_ALL=C sort | head -1)
      fi
      if [ "$reconnect_a" != "$reconnect_first" ]; then continue; fi
      reconnect_platform=$(platform_of "$reconnect_a")
      if [ "$reconnect_platform" = windows ]; then
        reconnect_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$reconnect_a" \
          'powershell.exe -NoProfile -NonInteractive -Command "[Environment]::GetFolderPath('"'"'UserProfile'"'"')"' | tr -d '\r')
      else
        reconnect_home=$(ssh -o BatchMode=yes -o ClearAllForwardings=yes "$reconnect_a" 'printf %s "$HOME"')
      fi
      reconnect_status=$(remote_peer_status "$reconnect_a" "$reconnect_platform" "$reconnect_home" "$reconnect_b")
      before_generation=$(printf '%s\n' "$reconnect_status" |
        sed -n 's/.*"generation":[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -1)
      test -n "$before_generation" || {
        printf 'bootstrap-fabric: missing peer generation before reconnect: %s -> %s\n' "$reconnect_a" "$reconnect_b" >&2
        exit 1
      }
      if [ "$reconnect_platform" = windows ]; then
        ssh -o BatchMode=yes -o ClearAllForwardings=yes "$reconnect_a" \
          "powershell.exe -NoProfile -NonInteractive -Command \"Restart-Service -Name 'DistributedWorkbenchPeer_$reconnect_b' -Force\""
      else
        ssh -o BatchMode=yes -o ClearAllForwardings=yes "$reconnect_a" \
          "systemctl --user restart 'distributed-workbench-peer-$reconnect_b.service'"
      fi
      wait_remote_peer_generation "$reconnect_a" "$reconnect_platform" "$reconnect_home" "$reconnect_b" "$before_generation"
    done
  done
  verify_controller_routes "$@"
fi
printf '%s\n' 'bootstrap-fabric: selected executors are ready'

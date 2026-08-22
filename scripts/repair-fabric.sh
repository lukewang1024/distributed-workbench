#!/bin/sh
set -eu

usage() {
  printf '%s\n' \
    'usage: scripts/repair-fabric.sh [--version VERSION] [--local-id ID] [--timeout-sec SECONDS] HOST|windows:HOST ...' \
    '' \
    'Restart selected node Controllers and managed peers from their native' \
    'supervisors, then delegate registration and route verification to' \
    'bootstrap-fabric.sh without requiring a healthy Controller first.'
}

version=
local_id=$(hostname -s)
timeout_seconds=120
specs=
nodes=
windows_nodes=
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
      shift 2
      ;;
    --timeout-sec)
      test "$#" -ge 2 || { usage >&2; exit 2; }
      timeout_seconds=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --*)
      printf 'repair-fabric: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
    *) break ;;
  esac
done

test "$#" -gt 0 || { usage >&2; exit 2; }
case $local_id in
  *[!0-9A-Za-z._-]*|'')
    printf 'repair-fabric: invalid local id: %s\n' "$local_id" >&2
    exit 2
    ;;
esac
case $timeout_seconds in
  *[!0-9]*|'0'|'')
    printf 'repair-fabric: timeout must be a positive integer: %s\n' "$timeout_seconds" >&2
    exit 2
    ;;
esac

for spec in "$@"; do
  case $spec in
    windows:*)
      host=${spec#windows:}
      windows_nodes="$windows_nodes $host"
      ;;
    *) host=$spec ;;
  esac
  case $host in
    *[!0-9A-Za-z._-]*|'')
      printf 'repair-fabric: HOST must be an SSH config alias: %s\n' "$host" >&2
      exit 2
      ;;
  esac
  specs="$specs $spec"
  nodes="$nodes $host"
done

case $(uname -s) in
  Darwin) local_platform=macos ;;
  Linux) local_platform=posix ;;
  *)
    printf 'repair-fabric: unsupported initiator platform: %s\n' "$(uname -s)" >&2
    exit 2
    ;;
esac

state_home=${XDG_STATE_HOME:-"$HOME/.local/state"}
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
bootstrap=$script_dir/bootstrap-fabric.sh
test -x "$bootstrap" || { printf 'repair-fabric: missing executable: %s\n' "$bootstrap" >&2; exit 2; }

if [ -z "$version" ]; then
  app_binary=${XDG_DATA_HOME:-"$HOME/.local/share"}/distributed-workbench/Agent\ Workbench.app/Contents/MacOS/workbench-macos-agent
  if [ -x "$app_binary" ]; then
    workbench=$app_binary
  elif [ -x "$HOME/.local/bin/workbench" ]; then
    workbench=$HOME/.local/bin/workbench
  else
    printf '%s\n' 'repair-fabric: cannot find the installed workbench executable' >&2
    exit 1
  fi
  version=$($workbench --version | awk 'NR == 1 {print $2}')
fi
case $version in
  v*) version=${version#v} ;;
esac
case $version in
  *[!0-9A-Za-z._-]*|'latest'|'')
    printf 'repair-fabric: invalid pinned version: %s\n' "$version" >&2
    exit 2
    ;;
esac

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

restart_local_controller() {
  case $local_platform in
    macos)
      domain=gui/$(id -u)
      launchctl print "$domain/dev.distributed-workbench.controller" >/dev/null 2>&1 || {
        printf '%s\n' 'repair-fabric: local Controller launchd job is not installed' >&2
        return 1
      }
      launchctl kickstart -k "$domain/dev.distributed-workbench.controller"
      ;;
    posix) systemctl --user restart distributed-workbench-controller.service ;;
  esac
}

restart_local_peer() {
  peer=$1
  case $local_platform in
    macos)
      domain=gui/$(id -u)
      label=dev.distributed-workbench.peer.$peer
      launchctl print "$domain/$label" >/dev/null 2>&1 || {
        printf 'repair-fabric: local peer launchd job is not installed: %s\n' "$peer" >&2
        return 1
      }
      launchctl kickstart -k "$domain/$label"
      ;;
    posix) systemctl --user restart "distributed-workbench-peer-$peer.service" ;;
  esac
}

restart_remote_controller() {
  host=$1
  platform=$(platform_of "$host")
  case $platform in
    windows)
      ssh -o BatchMode=yes -o ClearAllForwardings=yes -o ConnectTimeout=10 "$host" \
        "powershell.exe -NoProfile -NonInteractive -Command \"Restart-Service -Name 'DistributedWorkbenchController' -Force\""
      ;;
    posix)
      ssh -o BatchMode=yes -o ClearAllForwardings=yes -o ConnectTimeout=10 "$host" \
        'systemctl --user restart distributed-workbench-controller.service'
      ;;
  esac
}

restart_remote_peer_services() {
  host=$1
  platform=$(platform_of "$host")
  selected=" $local_id $nodes "
  case $platform in
    windows)
      for peer in $local_id $nodes; do
        ssh -o BatchMode=yes -o ClearAllForwardings=yes -o ConnectTimeout=10 "$host" \
          "powershell.exe -NoProfile -NonInteractive -Command \"if (Get-Service -Name 'DistributedWorkbenchPeer_$peer' -ErrorAction SilentlyContinue) { Restart-Service -Name 'DistributedWorkbenchPeer_$peer' -Force }\"" \
          >/dev/null
      done
      ;;
    posix)
      ssh -o BatchMode=yes -o ClearAllForwardings=yes -o ConnectTimeout=10 "$host" \
        "set -eu; selected='$selected'; unit_root=\"\${XDG_CONFIG_HOME:-\$HOME/.config}/systemd/user\"; find \"\$unit_root\" -maxdepth 1 -type f -name 'distributed-workbench-peer-*.service' -print | while IFS= read -r unit_path; do unit=\${unit_path##*/}; peer=\${unit#distributed-workbench-peer-}; peer=\${peer%.service}; case \"\$selected\" in *\" \$peer \"*) systemctl --user restart \"\$unit\" ;; esac; done"
      ;;
  esac
}

run_with_timeout() {
  seconds=$1
  shift
  if command -v timeout >/dev/null 2>&1; then
    timeout "$seconds" "$@"
  elif command -v perl >/dev/null 2>&1; then
    perl -e 'alarm shift @ARGV; exec @ARGV' "$seconds" "$@"
  else
    printf '%s\n' 'repair-fabric: neither timeout nor perl is available for bounded recovery' >&2
    return 1
  fi
}

printf 'repair-fabric: pinned release %s\n' "$version"
printf 'repair-fabric: local node %s (%s)\n' "$local_id" "$local_platform"
printf '%s\n' 'repair-fabric: restarting selected remote Controllers'
for host in $nodes; do
  printf 'repair-fabric: %s: Controller supervisor restart\n' "$host"
  restart_remote_controller "$host"
done

printf '%s\n' 'repair-fabric: restarting local Controller supervisor'
restart_local_controller

printf '%s\n' 'repair-fabric: restarting selected remote peer supervisors'
for host in $nodes; do
  restart_remote_peer_services "$host"
done

printf '%s\n' 'repair-fabric: restarting local peer supervisors'
set -- $specs
for spec in "$@"; do
  case $spec in
    windows:*) peer=${spec#windows:} ;;
    *) peer=$spec ;;
  esac
  restart_local_peer "$peer"
done

printf '%s\n' 'repair-fabric: verifying recovered registration and routes'
if run_with_timeout "$timeout_seconds" "$bootstrap" \
  --version "$version" \
  --local-id "$local_id" \
  --verify-only \
  "$@"; then
  printf '%s\n' 'repair-fabric: fabric recovery verified without state rotation'
  exit 0
fi

printf '%s\n' 'repair-fabric: verify-only failed; delegating reconciliation'
run_with_timeout "$timeout_seconds" "$bootstrap" \
  --version "$version" \
  --local-id "$local_id" \
  --skip-release-install \
  "$@"

printf '%s\n' 'repair-fabric: fabric recovery verified'

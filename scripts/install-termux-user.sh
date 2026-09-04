#!/bin/sh
set -eu

binary=${1:-target/release/workbench}
executor_id=${2:-$(hostname -s)-termux}
controller_id=${DISTRIBUTED_WORKBENCH_CONTROLLER_ID:-${executor_id%-rust}}
shift_count=0
if [ "$#" -ge 1 ]; then shift_count=1; fi
if [ "$#" -ge 2 ]; then shift_count=2; fi
if [ "$shift_count" -eq 1 ]; then shift; fi
if [ "$shift_count" -eq 2 ]; then shift 2; fi

test -x "$binary" || { echo "install-termux-user: executable not found: $binary" >&2; exit 2; }
test -n "${PREFIX:-}" || { echo 'install-termux-user: PREFIX is unset; run this inside Termux' >&2; exit 2; }
command -v sv >/dev/null 2>&1 || {
  echo 'install-termux-user: install termux-services first: pkg install termux-services' >&2
  exit 2
}
if [ -f "$PREFIX/etc/profile.d/start-services.sh" ]; then
  . "$PREFIX/etc/profile.d/start-services.sh"
fi

state_home=${XDG_STATE_HOME:-"$HOME/.local/state"}
installed_binary=$HOME/.local/bin/workbench
state_root=$state_home/distributed-workbench
service_root=$PREFIX/var/service
controller_socket=$state_root/controller.sock
executor_socket=$state_root/executor.sock
controller_state=$state_root/controller.json

mkdir -p "$HOME/.local/bin" "$state_root" "$service_root"
temporary=$installed_binary.$$.tmp
cp "$binary" "$temporary"
chmod 755 "$temporary"
mv "$temporary" "$installed_binary"
peer_helper=$(dirname "$0")/connect-termux-peer.sh
if [ -f "$peer_helper" ]; then
  installed_peer_helper=$HOME/.local/bin/connect-termux-peer
  peer_temporary=$installed_peer_helper.$$.tmp
  cp "$peer_helper" "$peer_temporary"
  chmod 755 "$peer_temporary"
  mv "$peer_temporary" "$installed_peer_helper"
fi

if [ "$#" -eq 0 ]; then
  set -- "$HOME"
fi
allow_args=
for root in "$@"; do
  case $root in /*) ;; *) echo "install-termux-user: allow-root must be absolute: $root" >&2; exit 2;; esac
  case $root in *"'"*) echo "install-termux-user: allow-root cannot contain a quote: $root" >&2; exit 2;; esac
  allow_args="$allow_args --allow-root '$root'"
done

write_service() {
  service_name=$1
  service_command=$2
  service_dir=$service_root/$service_name
  mkdir -p "$service_dir/log"
  temp_run=$service_dir/run.$$.tmp
  {
    printf '%s\n' '#!/bin/sh' 'exec 2>&1'
    printf 'exec %s\n' "$service_command"
  } >"$temp_run"
  chmod 755 "$temp_run"
  mv "$temp_run" "$service_dir/run"
  log_run=$service_dir/log/run
  {
    printf '%s\n' '#!/bin/sh'
    printf 'exec svlogd -tt %s\n' "'$state_root/log/$service_name'"
  } >"$log_run"
  chmod 755 "$log_run"
  mkdir -p "$state_root/log/$service_name"
}

write_service distributed-workbench-controller \
  "'$installed_binary' --socket '$controller_socket' controller serve --state '$controller_state' --id '$controller_id'"
write_service distributed-workbench-executor \
  "'$installed_binary' --socket '$executor_socket' executor serve --id '$executor_id'$allow_args"

sv up distributed-workbench-controller >/dev/null
sv up distributed-workbench-executor >/dev/null
attempt=0
while [ "$attempt" -lt 100 ]; do
  if "$installed_binary" --socket "$controller_socket" status >/dev/null 2>&1 \
    && "$installed_binary" --socket "$executor_socket" status >/dev/null 2>&1; then
    break
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done
test "$attempt" -lt 100 || { echo 'install-termux-user: services did not become ready' >&2; exit 1; }
params=$(printf '{"executorId":"%s","endpoint":{"transport":"local","socket":"%s"}}' "$executor_id" "$executor_socket")
"$installed_binary" --socket "$controller_socket" call executor.register "$params" >/dev/null
printf '%s\n' "$installed_binary"

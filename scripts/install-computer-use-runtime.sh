#!/bin/sh
# Install locked dependencies with node-local Node; keep old bundled releases readable.
set -eu
binary=${1:?binary required}
state_root=${2:?state root required}
source_root=$(CDPATH='' cd -- "$(dirname -- "$binary")/.." && pwd)/computer-use
[ -f "$source_root/host.mjs" ] || exit 0
expected_node=$(sed -n 's/.*"version": "\([0-9.]*\)".*/\1/p' "$source_root/node-runtimes.json")
node_bin=${WORKBENCH_NODE:-}
if [ -z "$node_bin" ] && command -v nodenv >/dev/null 2>&1; then
  node_bin=$(NODENV_VERSION="$expected_node" nodenv which node 2>/dev/null || true)
fi
if [ -z "$node_bin" ] && [ -n "${NVM_DIR:-}" ] && [ -f "$NVM_DIR/nvm.sh" ]; then
  node_bin=$(set +u; . "$NVM_DIR/nvm.sh" --no-use; nvm which "$expected_node" 2>/dev/null) || node_bin=
fi
if [ -z "$node_bin" ]; then node_bin=$(command -v node || true); fi
# Compatibility with older self-contained releases only.
if [ -x "$source_root/node" ]; then node_bin=$source_root/node; fi
[ -n "$node_bin" ] || { echo 'Install Node with nodenv, nvm or the system package manager; then retry with WORKBENCH_NODE if needed.' >&2; exit 1; }
namespace=${DISTRIBUTED_WORKBENCH_NAMESPACE:-stable}
case $namespace in *[!0-9A-Za-z._-]*|'') exit 2;; esac
if [ "$namespace" = stable ]; then suffix=; else suffix=-$namespace; fi
if command -v sha256sum >/dev/null 2>&1; then hash_command=sha256sum; else hash_command='shasum -a 256'; fi
# Deliberate word splitting for this constant command, never user input.
digest=$({ cat "$source_root/host.mjs" "$source_root/extension-host.mjs" "$source_root/environment.mjs" "$source_root/linux-runtime.mjs" "$source_root/release.mjs" "$source_root/package-lock.json" "$source_root/node-runtimes.json" "$source_root/package.json"; if [ -f "$source_root/host-version.json" ]; then cat "$source_root/host-version.json"; fi; } | $hash_command | cut -d ' ' -f 1)
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
destination=$data_home/distributed-workbench$suffix/computer-use-runtimes/$digest
if [ ! -d "$destination" ]; then
  mkdir -p "$(dirname "$destination")"
  lock=$destination.install-lock
  mkdir "$lock" 2>/dev/null || { echo 'Computer Use installation busy; inspect the existing installer before retrying.' >&2; exit 1; }
  temporary=$destination.$$.tmp
  trap 'rm -rf "$temporary"; rmdir "$lock"'  EXIT HUP INT TERM
  cp -R "$source_root" "$temporary"
  if [ ! -x "$source_root/node" ]; then
    "$node_bin" "$(dirname "$source_root")/scripts/install-computer-use.mjs" "$temporary"
  fi
  mv "$temporary" "$destination"
fi
mkdir -p "$state_root/computer-use"
(umask 077; printf '%s\n' "$destination" > "$state_root/computer-use/runtime-root.$$.tmp")
mv "$state_root/computer-use/runtime-root.$$.tmp" "$state_root/computer-use/runtime-root"

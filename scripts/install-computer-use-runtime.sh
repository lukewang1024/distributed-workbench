#!/bin/sh
# Copy a packaged plugin into immutable XDG data before temporary release cleanup.
set -eu
binary=${1:?binary required}
state_root=${2:?state root required}
source_root=$(CDPATH='' cd -- "$(dirname -- "$binary")/.." && pwd)/computer-use
[ -f "$source_root/host.mjs" ] || exit 0
[ -x "$source_root/node" ] || { echo 'packaged Computer Use Node is missing' >&2; exit 1; }
namespace=${DISTRIBUTED_WORKBENCH_NAMESPACE:-stable}
case $namespace in *[!0-9A-Za-z._-]*|'') exit 2;; esac
if [ "$namespace" = stable ]; then suffix=; else suffix=-$namespace; fi
if command -v sha256sum >/dev/null 2>&1; then hash_command=sha256sum; else hash_command='shasum -a 256'; fi
# Deliberate word splitting for this constant command, never user input.
digest=$(cat "$source_root/host.mjs" "$source_root/extension-host.mjs" "$source_root/package-lock.json" "$source_root/node-runtimes.json" | $hash_command | cut -d ' ' -f 1)
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
destination=$data_home/distributed-workbench$suffix/computer-use-runtimes/$digest
if [ ! -d "$destination" ]; then
  mkdir -p "$(dirname "$destination")"
  temporary=$destination.$$.tmp
  trap 'rm -rf "$temporary"' EXIT HUP INT TERM
  cp -R "$source_root" "$temporary"
  mv "$temporary" "$destination"
fi
mkdir -p "$state_root/computer-use"
(umask 077; printf '%s\n' "$destination" > "$state_root/computer-use/runtime-root.$$.tmp")
mv "$state_root/computer-use/runtime-root.$$.tmp" "$state_root/computer-use/runtime-root"

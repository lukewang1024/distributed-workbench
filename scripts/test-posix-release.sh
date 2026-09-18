#!/bin/sh
# Exercise native POSIX installation with an explicitly provisioned local Node.
set -eu
archive=${1:?release archive required}
node_bin=${WORKBENCH_NODE:?set WORKBENCH_NODE to the locked local Node executable}
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dwb-runtime-test.XXXXXX")
trap 'rm -rf "$temporary"' 0
trap 'exit 1' HUP INT TERM
mkdir "$temporary/package"
tar -xzf "$archive" -C "$temporary/package"
set -- "$temporary"/package/*
package=$1
source_root=$package/computer-use
installer=$package/scripts/install-computer-use-runtime.sh
binary=$package/bin/workbench
state=$temporary/state
export XDG_DATA_HOME=$temporary/data
export npm_config_cache=${npm_config_cache:-$temporary/npm-cache}
/bin/sh "$installer" "$binary" "$state"
selection=$state/computer-use/runtime-root
runtime=$(cat "$selection")
npm_config_offline=true /bin/sh "$installer" "$binary" "$state"
[ "$(cat "$selection")" = "$runtime" ]
# Construct a PATH without Node, even when the test machine has system Node.
mkdir "$temporary/minimal-bin"
for tool in dirname sed; do ln -s "$(command -v "$tool")" "$temporary/minimal-bin/$tool"; done
if env -u WORKBENCH_NODE -u NVM_DIR PATH="$temporary/minimal-bin" /bin/sh "$installer" "$binary" "$state" > "$temporary/missing.log" 2>&1; then
  echo 'Missing Node was accepted' >&2; exit 1
fi
grep -q 'Install Node with' "$temporary/missing.log"
cp "$source_root/node-runtimes.json" "$temporary/node-runtimes.json"
sed 's/"version": "[0-9.]*"/"version": "0.0.0"/' "$temporary/node-runtimes.json" > "$source_root/node-runtimes.json"
if /bin/sh "$installer" "$binary" "$state" > "$temporary/mismatch.log" 2>&1; then
  echo 'Incompatible Node was accepted' >&2; exit 1
fi
grep -q 'Node 0.0.0 is required' "$temporary/mismatch.log"
cp "$temporary/node-runtimes.json" "$source_root/node-runtimes.json"
[ "$(cat "$selection")" = "$runtime" ]
for item in "$XDG_DATA_HOME"/distributed-workbench/computer-use-runtimes/*.tmp "$XDG_DATA_HOME"/distributed-workbench/computer-use-runtimes/*.install-lock; do
  [ ! -e "$item" ] || { echo "Installation left staging or lock: $item" >&2; exit 1; }
done
"$node_bin" --input-type=module - "$source_root" "$runtime" "$temporary/probe" <<'JS'
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, realpathSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const [source, runtime, state] = process.argv.slice(2);
for (const name of readdirSync(source)) {
  assert(!['node', 'node.exe', 'node_modules'].includes(name));
  assert.deepEqual(readFileSync(join(source,name)), readFileSync(join(runtime,name)), name);
}
assert(!readdirSync(runtime).includes('node'));
assert.equal(readFileSync(join(runtime,'node-path'),'utf8').trim(), realpathSync(process.execPath));
const { validateCompatibility } = await import(pathToFileURL(join(runtime,'release.mjs')));
await validateCompatibility(runtime, runtime);
const { loadHost } = await import(pathToFileURL(join(runtime,'extension-host.mjs')));
const host = await loadHost(state, runtime);
assert.equal(host.tools().length, 11);
await host.close();
console.log('PASS: external Node, source equality, host integrity, and all 11 tools.');
JS
printf '%s\n' 'PASS: offline reuse, missing Node, version mismatch, preserved selection, staging and lock cleanup.'

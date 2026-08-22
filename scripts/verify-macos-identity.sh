#!/bin/sh
set -eu

binary=${1:-target/release/workbench-macos-agent}
if [ ! -x "$binary" ]; then
  echo "verify-macos-identity: executable not found: $binary" >&2
  exit 2
fi

plist_tmp=${TMPDIR:-/tmp}/workbench-info-plist.$$.tmp
trap 'rm -f "$plist_tmp"' EXIT HUP INT TERM
/usr/bin/otool -s __TEXT __info_plist "$binary" |
  tail -n +3 |
  awk '{for (i=2; i<=NF; i++) print substr($i,7,2) substr($i,5,2) substr($i,3,2) substr($i,1,2)}' |
  xxd -r -p |
  tr -d '\000' >"$plist_tmp"
name=$(plutil -extract CFBundleDisplayName raw -o - "$plist_tmp")
identifier=$(/usr/bin/codesign -dvv "$binary" 2>&1 | sed -n 's/^Identifier=//p')
test "$name" = "Agent Workbench"
test "$identifier" = "dev.distributed-workbench.macos-agent"
requirement=$(/usr/bin/codesign -d -r- "$binary" 2>&1 | sed -n 's/^designated => //p')
test "$requirement" = 'identifier "dev.distributed-workbench.macos-agent"'
printf 'displayName=%s\nidentifier=%s\n' "$name" "$identifier"
printf 'requirement=%s\n' "$requirement"

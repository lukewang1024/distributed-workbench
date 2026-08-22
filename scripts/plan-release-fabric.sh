#!/bin/sh
set -eu

version=${1:?version is required}
manifest=${2:?Fabric manifest is required}
repository=${DISTRIBUTED_WORKBENCH_REPOSITORY:-lukewang1024/distributed-workbench}
case $(uname -s):$(uname -m) in
  Darwin:arm64) target=aarch64-apple-darwin ;;
  Linux:x86_64) target=x86_64-unknown-linux-musl ;;
  *) printf 'plan-release-fabric: unsupported initiator: %s %s\n' "$(uname -s)" "$(uname -m)" >&2; exit 2 ;;
esac
case $version in v*) version=${version#v} ;; esac
case $version in *[!0-9A-Za-z._-]*|'') printf '%s\n' 'plan-release-fabric: invalid version' >&2; exit 2 ;; esac
test -f "$manifest" || { printf 'plan-release-fabric: missing manifest: %s\n' "$manifest" >&2; exit 2; }

archive=distributed-workbench-$version-$target.tar.gz
base=https://github.com/$repository/releases/download/v$version
temporary=$(mktemp -d "${TMPDIR:-/tmp}/distributed-workbench-plan.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
curl -fsSL "$base/$archive" -o "$temporary/$archive"
curl -fsSL "$base/SHA256SUMS" -o "$temporary/SHA256SUMS"
expected=$(awk -v name="$archive" '$2 == name {print $1}' "$temporary/SHA256SUMS")
test -n "$expected" || { printf '%s\n' 'plan-release-fabric: checksum missing' >&2; exit 1; }
actual=$(shasum -a 256 "$temporary/$archive" | awk '{print $1}')
test "$actual" = "$expected" || { printf '%s\n' 'plan-release-fabric: checksum mismatch' >&2; exit 1; }
tar -C "$temporary" -xzf "$temporary/$archive"
"$temporary/distributed-workbench-$version-$target/bin/workbench" fabric plan --file "$manifest"

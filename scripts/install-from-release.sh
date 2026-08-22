#!/bin/sh
set -eu

version=${1:-latest}
if [ "$#" -gt 0 ]; then
  shift
fi
repository=${DISTRIBUTED_WORKBENCH_REPOSITORY:-lukewang1024/distributed-workbench}
case $(uname -s):$(uname -m) in
  Linux:x86_64) target=x86_64-unknown-linux-musl ;;
  Darwin:arm64) target=aarch64-apple-darwin ;;
  *) echo "install-from-release: unsupported platform: $(uname -s) $(uname -m)" >&2; exit 2 ;;
esac

if [ "$version" = latest ]; then
  version=$(curl -fsSL "https://api.github.com/repos/$repository/releases/latest" |
    sed -n 's/.*"tag_name":[[:space:]]*"v\([^"]*\)".*/\1/p' | head -1)
fi
case $version in v*) version=${version#v};; esac
case $version in *[!0-9A-Za-z._-]*|'') echo "install-from-release: invalid version" >&2; exit 2;; esac

archive=distributed-workbench-$version-$target.tar.gz
base=https://github.com/$repository/releases/download/v$version
temporary=$(mktemp -d "${TMPDIR:-/tmp}/distributed-workbench.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
curl -fsSL "$base/$archive" -o "$temporary/$archive"
curl -fsSL "$base/SHA256SUMS" -o "$temporary/SHA256SUMS"
expected=$(awk -v name="$archive" '$2 == name {print $1}' "$temporary/SHA256SUMS")
test -n "$expected" || { echo "install-from-release: checksum missing" >&2; exit 1; }
actual=$(shasum -a 256 "$temporary/$archive" | awk '{print $1}')
test "$actual" = "$expected" || { echo "install-from-release: checksum mismatch" >&2; exit 1; }
tar -C "$temporary" -xzf "$temporary/$archive"
root=$temporary/distributed-workbench-$version-$target
case $target in
  *-linux-musl)
    executor_id=${DISTRIBUTED_WORKBENCH_EXECUTOR_ID:-$(hostname -s)}
    if [ "$#" -gt 0 ]; then
      (cd "$root" && scripts/install-linux-user.sh "$root/bin/workbench" "$executor_id" "$@")
    else
      (cd "$root" && scripts/install-linux-user.sh "$root/bin/workbench" "$executor_id")
    fi
    ;;
  *-apple-darwin)
    (cd "$root" && scripts/install-macos-app.sh bin/workbench-macos-agent)
    ;;
esac

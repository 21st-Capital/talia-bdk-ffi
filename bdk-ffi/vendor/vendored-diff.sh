#!/usr/bin/env bash
# TALIA FORK: the changes in a vendored crate against the crates.io release it came from, as a
# unified diff.
#
#   vendor/vendored-diff.sh bdk_wallet 3.1.0 1284fb23acc3e3022673712b55f4d5ce7e38aadc2c49bbef830dc3935f0a3289 > vendor/bdk_wallet.diff
#   vendor/vendored-diff.sh --check bdk_wallet 3.1.0 1284fb23acc3e3022673712b55f4d5ce7e38aadc2c49bbef830dc3935f0a3289
#
# Gets <name>-<version>.crate from static.crates.io (or from the file named by CRATE_FILE, for
# example the copy in the local Cargo cache), checks its SHA-256 against the one given (the
# checksum crates.io's index records for that release; Cargo.lock records none for a vendored
# crate, which is a path dependency) and unpacks it in a temporary folder.
#
# Without --check it prints the diff from the published crate (a/) to vendor/<name>/ (b/), file
# by file in byte order of the path, so `patch -p1` applies it inside the unpacked crate.
#
# With --check it applies vendor/<name>.diff to the unpacked crate and fails unless the result
# is vendor/<name>/ byte for byte (every file, both ways). The check does not depend on which
# diff program wrote the file.
set -euo pipefail
export LC_ALL=C

check=0
if [ "${1:-}" = "--check" ]; then
  check=1
  shift
fi
name="$1"
version="$2"
expected_sha="$3"

here="$(cd "$(dirname "$0")" && pwd)"
vendored="$here/$name"
[ -d "$vendored" ] || { echo "no vendor/$name" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

crate="$tmp/$name-$version.crate"
if [ -n "${CRATE_FILE:-}" ]; then
  cp "$CRATE_FILE" "$crate"
else
  curl -sSfL -o "$crate" "https://static.crates.io/crates/$name/$name-$version.crate"
fi
actual_sha="$(shasum -a 256 "$crate" | cut -d' ' -f1)"
if [ "$actual_sha" != "$expected_sha" ]; then
  echo "FAIL: $name-$version.crate has SHA-256 $actual_sha, expected $expected_sha" >&2
  exit 1
fi
mkdir "$tmp/published"
tar -xzf "$crate" -C "$tmp/published" --strip-components=1

files() { (cd "$1" && find . -type f | sed 's#^\./##'); }

if [ "$check" -eq 0 ]; then
  { files "$tmp/published"; files "$vendored"; } | sort -u | while IFS= read -r path; do
    old="$tmp/published/$path"
    new="$vendored/$path"
    if [ -f "$old" ] && [ -f "$new" ] && cmp -s "$old" "$new"; then
      continue
    fi
    old_label="a/$path"
    new_label="b/$path"
    [ -f "$old" ] || { old=/dev/null; old_label=/dev/null; }
    [ -f "$new" ] || { new=/dev/null; new_label=/dev/null; }
    diff -u --label "$old_label" --label "$new_label" "$old" "$new" || [ $? -eq 1 ]
  done
  exit 0
fi

diff_file="$here/$name.diff"
[ -f "$diff_file" ] || { echo "no vendor/$name.diff" >&2; exit 2; }
(cd "$tmp/published" && patch -p1 --quiet --no-backup-if-mismatch < "$diff_file")
manifest() { (cd "$1" && find . -type f -print0 | sort -z | xargs -0 shasum -a 256); }
manifest "$tmp/published" > "$tmp/patched.sha256"
manifest "$vendored" > "$tmp/vendored.sha256"
if cmp -s "$tmp/patched.sha256" "$tmp/vendored.sha256"; then
  echo "$name $version: crate SHA-256 $actual_sha (as expected); vendor/$name.diff applied to it gives vendor/$name/ byte for byte."
  echo "OK"
else
  echo "FAIL: vendor/$name.diff applied to $name-$version.crate does not give vendor/$name/:" >&2
  diff "$tmp/patched.sha256" "$tmp/vendored.sha256" >&2 || true
  exit 1
fi

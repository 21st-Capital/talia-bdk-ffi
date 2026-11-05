#!/usr/bin/env bash
# TALIA FORK: prove a vendored crate is the crates.io release plus only the files Talia patches.
#
#   vendor/check-vendored.sh bdk_wallet       3.1.0  1284fb23acc3e3022673712b55f4d5ce7e38aadc2c49bbef830dc3935f0a3289
#   vendor/check-vendored.sh electrum-client  0.25.0 1970c5d7bd9de6d4041cbfc3e46faa3e85fe7efcea2b0eb06750d3ae1ef577b7
#
# Downloads <name>-<version>.crate from static.crates.io (the SHA-256 must equal the one given,
# which is the checksum crates.io's index records for that release; Cargo.lock records none for
# a vendored crate, which is a path dependency), unpacks it in a temporary folder and compares
# it with vendor/<name>/ file by file (SHA-256 of every file, both ways).
# Every file must be byte-identical except those listed in vendor/<name>.patched (one path per
# line, relative to vendor/<name>/; the list may be empty). It fails on any other difference,
# on a missing or extra file, and on a listed file that is no longer patched (a stale list).
set -euo pipefail

name="$1"
version="$2"
expected_sha="$3"

here="$(cd "$(dirname "$0")" && pwd)"
vendored="$here/$name"
patched_list="$here/$name.patched"
[ -d "$vendored" ] || { echo "no vendor/$name" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

crate="$tmp/$name-$version.crate"
curl -sSfL -o "$crate" "https://static.crates.io/crates/$name/$name-$version.crate"
actual_sha="$(shasum -a 256 "$crate" | cut -d' ' -f1)"
if [ "$actual_sha" != "$expected_sha" ]; then
  echo "FAIL: $name-$version.crate has SHA-256 $actual_sha, expected $expected_sha" >&2
  exit 1
fi
mkdir "$tmp/published"
tar -xzf "$crate" -C "$tmp/published" --strip-components=1

manifest() { (cd "$1" && find . -type f -print0 | sort -z | xargs -0 shasum -a 256); }
manifest "$tmp/published" > "$tmp/published.sha256"
manifest "$vendored" > "$tmp/vendored.sha256"

patched=()
if [ -f "$patched_list" ]; then
  while IFS= read -r line; do
    [ -n "$line" ] && patched+=("./$line")
  done < "$patched_list"
fi

is_patched() {
  local f
  for f in "${patched[@]+"${patched[@]}"}"; do [ "$f" = "$1" ] && return 0; done
  return 1
}

status=0
identical=0
seen_patched=()
while read -r sha path; do
  other="$(awk -v p="$path" '$2 == p {print $1}' "$tmp/vendored.sha256")"
  if [ -z "$other" ]; then
    echo "MISSING in vendor/$name: $path"; status=1
  elif [ "$other" = "$sha" ]; then
    if is_patched "$path"; then
      echo "STALE: $path is listed in $name.patched but is identical to crates.io"; status=1
    else
      identical=$((identical + 1))
    fi
  else
    if is_patched "$path"; then
      echo "patched (expected): $path"; seen_patched+=("$path")
    else
      echo "DIFFERS from crates.io: $path"; status=1
    fi
  fi
done < "$tmp/published.sha256"

while read -r sha path; do
  if ! awk -v p="$path" '$2 == p {found=1} END {exit !found}' "$tmp/published.sha256"; then
    echo "EXTRA in vendor/$name (not in the published crate): $path"; status=1
  fi
done < "$tmp/vendored.sha256"

total="$(wc -l < "$tmp/published.sha256" | tr -d ' ')"
echo "$name $version: crate SHA-256 $actual_sha (as expected); $total files published; $identical byte-identical; ${#seen_patched[@]} patched as listed."
if [ "$status" -eq 0 ]; then echo "OK"; else echo "FAIL"; fi
exit "$status"

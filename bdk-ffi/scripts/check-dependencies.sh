#!/usr/bin/env bash
# TALIA FORK: the dependency rules for every iOS target Talia builds. CI runs it on
# every change (test.yaml); it needs only cargo and the committed Cargo.lock (no iOS toolchain).
#
#   bdk-ffi/scripts/check-dependencies.sh
#
# Fails when the normal or build dependency graph of aarch64-apple-ios, aarch64-apple-ios-sim or
# x86_64-apple-ios contains minreq, rustls 0.21, tokio, bip324, libsqlite3-sys or aws-lc-rs
# (or aws-lc-sys); when bdk_wallet 3.1.0, electrum-client 0.25.0 or bdk_electrum 0.24.1 is not
# the vendored copy under vendor/, or a second copy of one is in the graph; and when cargo warns
# that a [patch.crates-io] entry was not used.
set -euo pipefail

cd "$(dirname "$0")/.."
here="$(pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

forbidden='^(minreq|tokio|bip324|libsqlite3-sys|aws-lc-rs|aws-lc-sys) v|^rustls v0\.21\.'
status=0

for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
  cargo tree --locked --target "$target" -e normal,build --prefix none \
    > "$tmp/tree" 2> "$tmp/stderr"
  sed 's/ (\*)$//' "$tmp/tree" | sort -u > "$tmp/packages"

  if grep -i 'was not used' "$tmp/stderr"; then
    echo "FAIL $target: a [patch.crates-io] entry was not used"; status=1
  fi
  if grep -E "$forbidden" "$tmp/packages"; then
    echo "FAIL $target: forbidden crate(s) above"; status=1
  fi
  for crate in "bdk_wallet v3.1.0" "electrum-client v0.25.0" "bdk_electrum v0.24.1"; do
    name="${crate%% *}"
    copies="$(grep -E "^$name v" "$tmp/packages" || true)"
    if [ "$copies" != "$crate ($here/vendor/$name)" ]; then
      echo "FAIL $target: $name is not exactly the vendored $crate:"; echo "$copies"; status=1
    fi
  done
  echo "$target: $(wc -l < "$tmp/packages" | tr -d ' ') packages checked"
done

if [ "$status" -eq 0 ]; then echo "OK"; else echo "FAIL"; fi
exit "$status"

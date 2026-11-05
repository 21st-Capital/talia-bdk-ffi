# Vendored crates (Talia fork)

Each folder here is a crates.io release, unpacked byte for byte from its `.crate`, plus only
the Talia patches listed in `<name>.patched` (one path per line, relative to the folder).
`[patch.crates-io]` in `../Cargo.toml` points the build at the folder. `<name>.diff` beside
each folder is the whole of the Talia patches as one unified diff against the published crate
(see "The diffs" below), so the changes can be read without the repository's history.

The SHA-256 in the table is the checksum crates.io's index records for the release. Cargo.lock
records none for these three crates: `[patch.crates-io]` makes each of them a path dependency.

| Folder | Release | `.crate` SHA-256 (crates.io index checksum) | Upstream git commit (`.cargo_vcs_info.json`) | Patched files |
|---|---|---|---|---|
| `bdk_wallet/` | bdk_wallet 3.1.0 | `1284fb23acc3e3022673712b55f4d5ce7e38aadc2c49bbef830dc3935f0a3289` | `58fe631177047ef1c49947ff453110f47138adcd` | `bdk_wallet.patched` |
| `electrum-client/` | electrum-client 0.25.0 | `1970c5d7bd9de6d4041cbfc3e46faa3e85fe7efcea2b0eb06750d3ae1ef577b7` | `c7706810c45640505ceb19e9264808dc298b3418` | `electrum-client.patched` |
| `bdk_electrum/` | bdk_electrum 0.24.1 | `97455296f4f798250eaa5f7aa51fc34298a72fb661784b4c52c190d730ad04f7` | `5dc6e959379982a1661e8f57c3ee7af334627ca5` (`crates/electrum`) | `bdk_electrum.patched` |

## Talia patches

bdk_wallet 3.1.0:
- `src/wallet/utils.rs`: `Older::check_older` saturates instead of `checked_add().expect()`, which
  panicked whenever a PSBT spending an unconfirmed coin of a vault with an `older()` branch was
  finalized (upstream issue #557, PR #565). Tests: `src/tests/check_older.rs`.
- `src/wallet/tx_builder.rs`, `src/wallet/mod.rs`: `TxBuilder::replacing(txids)` for Speed up and
  Cancel. `add_utxos` then also accepts a coin the named unconfirmed transactions spend (only
  they, the parent transaction whole and canonical, the script the wallet's own); outputs of the
  replaced transactions and their descendants are never spent, and nothing is accepted while a
  replaced transaction has an unconfirmed descendant that is not named too (the replacement
  would evict it). Tests: `src/tests/replacement.rs`.
- `src/wallet/mod.rs`: `Wallet::finalize_psbt_keeping_metadata`, finalizing the way 0.28's
  watch-only `sign()` did (metadata update, the same two gates, then input by input: final
  fields written, only that input's `partial_sigs` dropped, unfinished inputs and all outputs
  untouched). Upstream's `finalize_psbt` is unchanged. Tests: `src/tests/finalize.rs`.
- `src/wallet/mod.rs`, `src/wallet/signer.rs`: D10, the leaf hazard (decided 2026-10-17; "D10"
  is this fork's name for the rule, used in the patched comments and in
  `src/tests/d10_leaf.rs`). Both finalizers (`finalize_psbt`, which `sign` runs, and
  `finalize_psbt_keeping_metadata`) satisfy each input with miniscript's PSBT input satisfier
  alone: a leaf with `older(N)` or `after(N)` counts only when the transaction carries the lock
  (nSequence and version, nLockTime and an nSequence that enables it). Upstream also passed
  `After`/`Older`, which said yes on the coin's age and the chain tip (or `assume_height`), so
  an everyday spend of an old coin in a vault where one device holds keys on two paths was
  finalized through the timelocked leaf, and the network refuses it. `assume_height` is no longer
  read by the finalizer (documented on `SignOptions`). The same rule as Talia's earlier
  library on bdk 0.28. Tests: `src/tests/d10_leaf.rs` (the hazard tests fail on the unpatched
  finalizer); upstream's own integration tests (`tests/*.rs` of the crate, 191) pass unchanged.
- `src/wallet/mod.rs`: `Wallet::repair_psbt_key_origins`, the combine repair behind
  bdk-ffi's `Wallet.update_psbt_with_descriptor`. A combine lets the device copy's key origins
  win, and upstream's private `update_psbt_with_descriptor` (which `sign` runs, and stays
  private and unchanged) does not undo that for taproot: miniscript only adds a leaf key's
  origin when the key has none. The repair removes the origin of every key the input's or
  output's descriptor contains, then runs miniscript's update, which writes them again from the
  descriptor with their leaf hashes; other keys, signatures and fields stay. Tests:
  `src/tests/finalize.rs` (wpkh, and `tr(key, multi_a(2, A, B, C))` on an input and the change
  output).

electrum-client 0.25.0 (the TLS behaviour of Talia's earlier library, on the new client):
- `src/raw_client.rs`: the ring provider is named per connection (`builder_with_provider` with
  rustls's safe default protocol versions, TLS 1.2 and 1.3) instead of installing a
  process-wide default; with `validate_domain = false` the server's handshake signature is
  still checked against its certificate's key (upstream accepted any signature); the root store
  is webpki-roots 1.0's current Mozilla list, built by the new public `default_root_store()` so a
  test can check the list the client really uses. Tests: `src/tests/electrum_tls.rs` (one of
  them, `--ignored`, connects to a public server).
- `Cargo.toml`: rustls at least 0.23.46, webpki-roots 1.0.9, and the `rustls-ring` feature
  written with `rustls?/...`, so it no longer switches on the `rustls` feature and with it
  aws-lc-rs.

bdk_electrum 0.24.1 (decided 2026-10-25, the new library's first sync of a wallet with a history):
- `src/bdk_electrum_client.rs`: the transactions a sync needs and does not hold yet are asked
  for in batched requests (`prefetch_txs`, at most 100 per request), once per batch of script
  histories (only the histories the scan gets to before its stop gap) and once for the previous
  transactions the fees need; so are the headers `batch_fetch_anchors` fetches again for proofs
  that no longer match a cached header (`prefetch_headers`). Upstream asked for each of them in
  a round trip of its own. The batches only warm the client's caches: the code that follows is
  upstream's, unchanged, and still asks on its own for anything the batch did not bring (a
  refused or missing transaction, one with another txid, a batch that failed as a whole), so it
  succeeds or fails as before, with the same errors. A batch refused in its turn
  (electrum-client's `Error::Protocol`: it gives up on that batch, keeps the connection and does
  not retry) ends only that batch, and the next one still goes; any other failure (a timeout, a
  lost connection, retries used up) ends that warm-up. One outcome can differ, and only with a
  server that refuses a request once and answers the same request when asked again (a passing
  rate limit): the batched code asks twice (in the batch, then on its own) and the sync
  finishes, where upstream asks once and fails with that refusal (test
  `a_request_refused_inside_a_batch_is_asked_again_on_its_own`). With a server that answers a
  request the same way each time, no case is known where one succeeds and the other fails.
  Time, not results: a request the server never answers costs electrum-client's whole retry
  cycle twice, once for the batch and once when it is asked for again on its own, before the
  same error. With `timeout` t and `retry` r that is r + 1 timeouts and the pauses before each
  reconnect (2, 4, ... s) more than upstream. Measured 2026-10-25 with the fake server
  (upstream, then batched): 1.0 s and 2.0 s with t = 1 s, r = 0; 9.0 s and 18.0 s with t = 1 s,
  r = 2; so about 36 s and 72 s with t = 10 s, r = 2.
  A server silent from the connection or the first request on fails as upstream does, at the
  same point. Upstream itself already sends larger batches than these (every merkle proof a
  sync needs in one request), so a server that cannot answer a batch fails upstream's sync too.
  `sync`'s txids and outpoints (which
  bdk_wallet's requests never carry) stay one request each: batching them would consume the
  request's iterators, and so its inspect callbacks, ahead of the fetches. Tests:
  `src/tests/electrum_batch.rs` (a fake Electrum server; each scenario also run on upstream's
  code path, with identical results).

## The diffs

`bdk_wallet.diff`, `electrum-client.diff` and `bdk_electrum.diff` hold every Talia patch of the
folder of the same name, as a unified diff from the published crate (`a/`) to the folder
(`b/`), file by file in byte order of the path. They were made with `vendored-diff.sh`, which
gets the `.crate` (from static.crates.io, or from the file `CRATE_FILE` names, such as the copy
in the local Cargo cache), checks its SHA-256 and diffs the unpacked crate against the folder:

```sh
cd bdk-ffi
vendor/vendored-diff.sh bdk_wallet 3.1.0 1284fb23acc3e3022673712b55f4d5ce7e38aadc2c49bbef830dc3935f0a3289 > vendor/bdk_wallet.diff
vendor/vendored-diff.sh electrum-client 0.25.0 1970c5d7bd9de6d4041cbfc3e46faa3e85fe7efcea2b0eb06750d3ae1ef577b7 > vendor/electrum-client.diff
vendor/vendored-diff.sh bdk_electrum 0.24.1 97455296f4f798250eaa5f7aa51fc34298a72fb661784b4c52c190d730ad04f7 > vendor/bdk_electrum.diff
```

Regenerate a diff in the same change as any edit to its folder. `vendored-diff.sh --check
<name> <version> <sha256>` applies the committed diff to the published crate with `patch -p1`
and fails unless the result is the folder byte for byte; CI runs it for all three, so the check
does not depend on which `diff` program wrote the file. To apply a diff to a fresh copy of its
crate, from the repository root:

```sh
mkdir bdk_wallet-3.1.0 && curl -sSfL https://static.crates.io/crates/bdk_wallet/bdk_wallet-3.1.0.crate \
  | tar -xzf - -C bdk_wallet-3.1.0 --strip-components=1
(cd bdk_wallet-3.1.0 && patch -p1 < ../bdk-ffi/vendor/bdk_wallet.diff)
```

## Checking a folder

```sh
cd bdk-ffi
vendor/check-vendored.sh bdk_wallet 3.1.0 1284fb23acc3e3022673712b55f4d5ce7e38aadc2c49bbef830dc3935f0a3289
vendor/check-vendored.sh electrum-client 0.25.0 1970c5d7bd9de6d4041cbfc3e46faa3e85fe7efcea2b0eb06750d3ae1ef577b7
vendor/check-vendored.sh bdk_electrum 0.24.1 97455296f4f798250eaa5f7aa51fc34298a72fb661784b4c52c190d730ad04f7
```

The script downloads the `.crate` from static.crates.io, checks its SHA-256, unpacks it in a
temporary folder and compares every file both ways by SHA-256. It fails on a file that differs
without being listed in `<name>.patched`, on a missing or extra file, and on a listed file that
no longer differs.

Recorded checks:
- 2026-10-16, bdk_wallet 3.1.0 as vendored: 81 files published, 81 byte-identical, 0 patched.
  The `.crate` downloaded from static.crates.io, the copy in the local Cargo cache and the
  crates.io index all give `1284fb23…3289`.
- 2026-10-16, electrum-client 0.25.0 as vendored: 34 files published, 34 byte-identical,
  0 patched. Download, local Cargo cache and crates.io index all give `1970c5d7…77b7`.
- 2026-10-25, bdk_electrum 0.24.1 as vendored: 10 files published, 10 byte-identical,
  0 patched. Download, local Cargo cache and crates.io index all give `97455296…04f7`.
- 2026-10-25, bdk_electrum 0.24.1 with the batched fetches: 10 files published, 9
  byte-identical, 1 patched as listed (`src/bdk_electrum_client.rs`).

## Notes

- Each published crate's own `.gitignore` lists `Cargo.lock` (bdk_electrum ships no
  `.gitignore`); the vendored `Cargo.lock` is committed anyway (`git add -f`) so the folder
  stays byte-identical. Cargo ignores it for a
  path dependency, and the crate's own `rust-toolchain.toml` (1.95.0) does not apply either:
  rustup's proxies pass the outer toolchain (bdk-ffi's 1.97.1) to everything cargo runs.
- The crate's `.github/` workflows never run: GitHub reads workflows only from the
  repository's top-level `.github/workflows/`.
- Licences: bdk_wallet and electrum-client carry their own licence files. bdk_electrum's
  published crate has none; its `Cargo.toml` says `MIT OR Apache-2.0`, the same licence as this
  repository, whose root `LICENSE-MIT` and `LICENSE-APACHE` cover it. No licence file is added
  inside the folder, which would make it differ from the published crate.

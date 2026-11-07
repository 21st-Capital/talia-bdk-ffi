# bdk-ffi for Talia

This repository, [21st-Capital/talia-bdk-ffi], is 21st Capital's fork of [bdk-ffi], the
[uniffi-rs] wrapper around the [Bitcoin Dev Kit]'s [bdk_wallet]. Talia, 21st Capital's iPhone
wallet ([21st-Capital/talia]), links the static library built from it and the Swift bindings
generated from that library. Only the Rust crate in [`bdk-ffi/`](bdk-ffi) is kept; the upstream
Kotlin and Swift packaging is not.

## This copy

The repository is published as a fresh copy of a reviewed commit of 21st Capital's private
repository: its files are exactly that commit's, without the version history. Each change is
reviewed there and copied here as one new commit before Talia builds the library from it, so
the library the app links is always built from a commit of this repository's `main`.

Issues are welcome. Pull requests from outside 21st Capital are not accepted for now: a change
is made in the private repository and arrives here with the next copy.

## Upstream base

The fork starts from [bitcoindevkit/bdk-ffi] at commit
`1c5f4863c6a53cc9d063d34c4e13c91fea5c58de` ("docs: add changelog entry for 3.1.1 release",
2026-09-29), bdk-ffi 3.1.1. The crate keeps its name and version (`bdk-ffi` 3.1.1, library
`bdkffi`). [CHANGELOG.md](CHANGELOG.md) is upstream's, up to that release; its opening note
points at upstream's repository for the release tags and the development cycle.

## What the fork changes

The changed places in the code carry a `TALIA FORK` note saying what changed and why.

- **Fewer parts.** No Esplora client, no Kyoto compact-filter node and no SQLite persister: Talia
  syncs over Electrum only and keeps no wallet database, so `Persister::new_in_memory` is a
  plain in-memory changeset. The Kotlin (`bdk-android`) and Swift (`bdk-swift`) packaging, its
  tests and upstream's release and documentation tooling are removed.
- **Build settings.** Cargo's feature resolver 2, and uniffi's `cli` and `cargo-metadata`
  features only for the `uniffi-bindgen` tool, so no bindgen code reaches the static library.
  The compiler is pinned to rustc 1.97.1 (`rust-toolchain.toml`), every iOS object is built for
  iOS 15.0 (`.cargo/config.toml`), and the `release-smaller` profile unwinds on a panic instead
  of aborting, so uniffi hands Swift an error it can catch.
- **No call can take the app down.** Every `Wallet` method returns a `Result`, also those
  upstream declares infallible: a wallet whose lock an earlier panic poisoned answers with an
  error, an address index above the BIP-32 maximum is an error, and the JSON encoders and PSBT
  input lookups no longer unwrap.
- **Errors.** Errors that could quote a descriptor carry a fixed text instead of quoting it; a
  spending-policy error (which quotes only a policy node id) is passed on; a failed coin
  selection comes back as `InsufficientFunds`, with the needed and available amounts as
  numbers, where upstream turns it into a `CoinSelection` error that holds only a message.
- **Three more Electrum calls.** `ElectrumClient.script_get_history` and
  `ElectrumClient.script_list_unspent` pass on one script's history and its unspent coins as
  the server reports them (`blockchain.scripthash.get_history` and
  `blockchain.scripthash.listunspent`), and `ElectrumClient.batch_transaction_get` fetches
  several transactions in one request, in the order asked, all through the same client and TLS
  settings as a sync, so Talia can look up an address outside the wallet in three round trips.
- **Vendored crates.** bdk_wallet 3.1.0, electrum-client 0.25.0 and bdk_electrum 0.24.1 are kept
  in [`bdk-ffi/vendor/`](bdk-ffi/vendor) as the crates.io releases plus a few patches, each
  shown as a diff against the published crate ([vendor/README.md](bdk-ffi/vendor/README.md)):
  - `check_older` saturates instead of panicking on a coin that is not confirmed yet;
  - `TxBuilder::replacing` lets Speed up and Cancel spend the coins of the transaction they
    replace;
  - `finalize_psbt_keeping_metadata` finalizes input by input and keeps the PSBT's metadata,
    as bdk 0.28's watch-only `sign()` did;
  - a timelocked leaf counts as satisfied only when the transaction itself carries the lock,
    never because of the coin's age or the chain tip;
  - `repair_psbt_key_origins` puts the wallet's own key origins back after a PSBT combine;
  - electrum-client checks the server's TLS handshake signature even without domain
    validation, names its crypto provider per connection and uses webpki-roots 1.0's current
    Mozilla root list;
  - bdk_electrum fetches the transactions and headers a sync needs in batches.
- **Checks.** `bdk-ffi/scripts/check-dependencies.sh` (dependency rules for the three iOS
  targets), `bdk-ffi/vendor/check-vendored.sh` and `bdk-ffi/vendor/vendored-diff.sh` (each
  vendored folder is its crates.io release plus exactly the listed patches), and
  `bdk-ffi/scripts/symbol-overlap-gate.py`, which Talia's build runs against the other Rust
  libraries the app links. CI ([.github/workflows](.github/workflows)) runs `cargo test`, the
  dependency rules, both vendored-crate checks, the vendored bdk_wallet's own test suites and a
  RustSec audit. It does not run the symbol-overlap gate; Talia's build does.

## What Talia builds

Talia's build script builds the library from `bdk-ffi/`, under an environment cleared down to
the basics plus two variables of its own (below), with

```sh
cargo build --locked --package bdk-ffi --lib --profile release-smaller --target aarch64-apple-ios
cargo build --locked --package bdk-ffi --lib --profile release-smaller --target aarch64-apple-ios-sim
cargo build --locked --package bdk-ffi --lib --profile release-smaller --target x86_64-apple-ios
```

(no features; the two simulator libraries are joined with `lipo`), and generates the Swift
bindings with this crate's own `uniffi-bindgen` (`cargo run --locked --features
uniffi/cli,uniffi/cargo-metadata --bin uniffi-bindgen -- generate <library> --language swift`).

The two variables keep the build machine's folders out of the library (decided 2026-11-06).
`CARGO_ENCODED_RUSTFLAGS` holds `--remap-path-prefix=<Cargo home>=/cargo` and
`--remap-path-prefix=<checkout>=/talia-bdk-ffi` (the two separated by the byte 0x1f), for the
paths rustc writes into panic messages. `CFLAGS` holds the same two as `-ffile-prefix-map`
flags and a third, `-ffile-prefix-map=<Xcode folder>=/xcode`, for clang: ring builds its C and
assembly with debug information on Apple targets, which records their source folders and the
iOS SDK's and clang's include folders. The Cargo home is `$HOME/.cargo`, the checkout is the
repository root (the folder holding `bdk-ffi/`) by its physical path (`pwd -P`), and the Xcode
folder is the one `xcode-select -p` names; a folder reached through a link is mapped by its
physical path as well, and the paths must hold no spaces and no `=`. So the library's bytes
depend on the commit, the Rust compiler (1.97.1), the version of the Xcode whose clang compiles
the C code inside (ring, secp256k1) and those two variables with exactly these names, and not
on where the checkout, the home folder or Xcode sits.

Run the tests with `cargo test --locked` inside `bdk-ffi/`.

## Authorship and licence

The code up to upstream commit `1c5f4863` is the Bitcoin Dev Kit developers' work; its
authorship is in upstream's history at [bitcoindevkit/bdk-ffi]. Later changes are 21st
Capital's. The vendored crates keep their own authors' notices. Everything is licensed as
upstream is, under Apache 2.0 or MIT at your option ([LICENSE](LICENSE),
[LICENSE-APACHE](LICENSE-APACHE), [LICENSE-MIT](LICENSE-MIT)). A copy of this repository
without its history has no version control history to point to, so this section stands in for
the pointer in `LICENSE`.

## Security

Report security issues in this fork privately, through GitHub's **Report a vulnerability** on
this repository, as [SECURITY.md](SECURITY.md) says. Issues in bdk-ffi, bdk_wallet or uniffi
themselves belong to their upstream projects.

[21st-Capital/talia-bdk-ffi]: https://github.com/21st-Capital/talia-bdk-ffi
[21st-Capital/talia]: https://github.com/21st-Capital/talia

---

The rest of this file is upstream's README, without its badges and the parts about the removed
Kotlin and Swift packages and upstream's release signing.

## Language Bindings for BDK

The code in this repository creates a library ready for export to other languages using [uniffi-rs] for the Rust-based [bdk_wallet] library from the [Bitcoin Dev Kit] project.

The Rust code in this project is in the bdk-ffi directory and is a wrapper around the [bdk_wallet] library to expose its APIs in a uniform way using the [mozilla/uniffi-rs] bindings generator for each supported target language.

## Other supported languages maintained in separate repositories

The `bdk-ffi` codebase in this repository can be used to produce language bindings for more than Swift and Kotlin. Some of these require the use of uniffi 3rd party plugins and some not. Below are some of the libraries that use the bdk-ffi API, but are maintained separately. Refer to their individual READMEs for information on their state of production-readiness.

| Language | Platform              | Published Package         | Repository   | API Docs              |
| -------- |-----------------------|---------------------------|--------------|-----------------------|
| Kotlin   | JVM                   | [bdk-jvm (Maven Central)] | [bdk-jvm]    |                       |
| Python   | Linux, macOS, Windows | [bdk-python (PyPI)]       | [bdk-python] |                       |
| Dart     | Android, iOS, macOS, Linux, Windows |                   | [bdk-dart]   |                       |
| TypeScript | React Native (Android, iOS) |                   | [bdk-rn]     |                       |

## Contributing

To add new structs and functions, see the [UniFFI User Guide](https://mozilla.github.io/uniffi-rs/) and the [uniffi-examples](https://thunderbiscuit.github.io/uniffi-examples/) repository.

## Goals

1. Language bindings should feel idiomatic in target languages/platforms
2. Adding new targets should be easy
3. Getting up and running should be easy
4. Contributing should be easy
5. Get it right, then automate

## Thanks

This project is made possible thanks to the wonderful work by the [mozilla/uniffi-rs] team.

[bdk-jvm]: https://github.com/bitcoindevkit/bdk-jvm
[bdk-jvm (Maven Central)]: https://central.sonatype.com/artifact/org.bitcoindevkit/bdk-jvm/
[bdk-python]: https://github.com/bitcoindevkit/bdk-python
[bdk-python (PyPI)]: https://pypi.org/project/bdk-python
[bdk-dart]: https://github.com/bitcoindevkit/bdk-dart
[bdk-rn]: https://github.com/bitcoindevkit/bdk-rn
[mozilla/uniffi-rs]: https://github.com/mozilla/uniffi-rs
[bdk_wallet]: https://github.com/bitcoindevkit/bdk_wallet
[Bitcoin Dev Kit]: https://github.com/bitcoindevkit
[uniffi-rs]: https://github.com/mozilla/uniffi-rs
[bdk-ffi]: https://github.com/bitcoindevkit/bdk-ffi
[bitcoindevkit/bdk-ffi]: https://github.com/bitcoindevkit/bdk-ffi

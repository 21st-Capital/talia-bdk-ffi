# Security Policy

This policy covers [21st-Capital/talia-bdk-ffi](https://github.com/21st-Capital/talia-bdk-ffi),
21st Capital's fork of bdk-ffi: the changes this repository makes to upstream bdk-ffi 3.1.1
(listed in [README.md](README.md)) and the patches to the vendored crates in
[`bdk-ffi/vendor/`](bdk-ffi/vendor). The library built from it is part of the Talia wallet app;
it is not published as a crate or package of its own.

## Reporting a vulnerability

Please report a vulnerability privately, never in a public issue or pull request. Use GitHub's
private vulnerability reporting: on this repository's **Security** tab, choose **Report a
vulnerability**, or open
<https://github.com/21st-Capital/talia-bdk-ffi/security/advisories/new>. The report stays
private between you and the repository's maintainers.

Say which commit you looked at, what an attacker can do, and how to reproduce it.

## Upstream code

A problem in code this fork does not change belongs to the upstream project, which has its
own security policy:

- bdk-ffi: [bitcoindevkit/bdk-ffi](https://github.com/bitcoindevkit/bdk-ffi/blob/master/SECURITY.md)
- bdk_wallet, bdk_electrum, bdk_chain: [Bitcoin Dev Kit](https://github.com/bitcoindevkit/bdk_wallet/blob/master/SECURITY.md)
- electrum-client: [bitcoindevkit/rust-electrum-client](https://github.com/bitcoindevkit/rust-electrum-client)
- uniffi: [mozilla/uniffi-rs](https://github.com/mozilla/uniffi-rs)

If you are not sure whether a problem is upstream's or this fork's, report it to us and we will
pass it on.

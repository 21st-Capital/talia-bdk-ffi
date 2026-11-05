//! TALIA FORK: the build settings that make the shipped library safe, checked where `cargo test`
//! cannot see them otherwise.
//!
//! Cargo builds tests with the test profile, which ignores `[profile.release-smaller]`'s `panic`
//! setting, so the panic tests in `wallet.rs` pass even if the library Talia ships would abort on
//! a panic. Likewise nothing in a test build depends on the iOS deployment target or on which
//! compiler the toolchain file names. These tests read the files themselves.

const CARGO_TOML: &str = include_str!("../../Cargo.toml");
const CARGO_CONFIG: &str = include_str!("../../../.cargo/config.toml");
const RUST_TOOLCHAIN: &str = include_str!("../../../rust-toolchain.toml");

/// The `key = value` lines of `[section]` in a TOML file, comments and blank lines dropped and
/// spaces removed (enough for these flat sections; no value here contains `#` or a space).
fn section(file: &str, name: &str) -> Vec<String> {
    let header = format!("[{}]", name);
    let mut lines = file.lines().map(str::trim);
    assert!(
        lines.any(|line| line == header),
        "no {} section in the file",
        header
    );
    lines
        .take_while(|line| !line.starts_with('['))
        .map(|line| line.split('#').next().unwrap().replace(' ', ""))
        .filter(|line| !line.is_empty())
        .collect()
}

#[test]
fn test_release_smaller_unwinds_on_panic() {
    let profile = section(CARGO_TOML, "profile.release-smaller");
    let panic: Vec<_> = profile
        .iter()
        .filter(|line| line.starts_with("panic="))
        .collect();
    assert_eq!(panic, ["panic=\"unwind\""], "{:?}", profile);
    assert!(profile.contains(&"inherits=\"release\"".to_string()));
    // `release` itself must not say abort either (release-smaller inherits it).
    assert!(!CARGO_TOML.contains("[profile.release]"));
}

#[test]
fn test_every_ios_object_is_built_for_ios_15() {
    let env = section(CARGO_CONFIG, "env");
    assert_eq!(
        env,
        ["IPHONEOS_DEPLOYMENT_TARGET={value=\"15.0\",force=true}"],
        "{:?}",
        env
    );
}

#[test]
fn test_toolchain_is_rust_1_97_1_with_the_ios_targets_and_no_components() {
    let toolchain = section(RUST_TOOLCHAIN, "toolchain");
    assert_eq!(
        toolchain,
        [
            "channel=\"1.97.1\"",
            "targets=[\"aarch64-apple-ios\",\"aarch64-apple-ios-sim\",\"x86_64-apple-ios\"]",
        ],
        "{:?}",
        toolchain
    );
}

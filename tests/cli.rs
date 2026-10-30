//! The compiled binary's command line. Cargo builds the binary for integration tests and tells us where it is.

use std::process::Command;

#[test]
fn version_flag_prints_the_baked_in_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_marginalia")).arg("--version").output().expect("run marginalia");
    assert!(out.status.success(), "{out:?}");
    // clap prints `<name> <version>`; release builds bake MARGINALIA_VERSION in, local builds report Cargo's.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("marginalia {}", marginalia::VERSION));
}

#[test]
fn version_falls_back_to_the_cargo_version() {
    if option_env!("MARGINALIA_VERSION").is_none() {
        assert_eq!(marginalia::VERSION, env!("CARGO_PKG_VERSION"));
    }
}

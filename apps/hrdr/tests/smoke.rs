//! Binary smoke tests: the CLI launches and its arg surface is wired.

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hrdr")
}

#[test]
fn prints_version() {
    let out = Command::new(bin()).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("hrdr"));
}

#[test]
fn prints_help() {
    let out = Command::new(bin()).arg("--help").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("harness"));
    assert!(s.contains("run"));
}

#[test]
fn run_requires_a_prompt() {
    // `run` with no prompt is a usage error (clap: required trailing arg).
    let out = Command::new(bin()).arg("run").output().unwrap();
    assert!(!out.status.success());
}

//! The sandbox only reaches a test binary that LINKS it, and rustc links nothing that
//! is never referenced — a dev-dependency alone is dropped, ctor and all (verified: an
//! unreferenced `hrdr-test-support` dev-dep does not run its ctor; one line of
//! `extern crate` does).
//!
//! So the line is load-bearing, and a new crate — or a new `tests/*.rs`, which is its own
//! binary and gets none of the library's `#[cfg(test)]` code — can silently lack it. This
//! test walks the workspace and fails, by name, on any crate root or integration test
//! that does. It is cheap, it runs in every `cargo test`, and it is what makes the
//! isolation structural instead of remembered.

extern crate hrdr_test_support;

use std::path::{Path, PathBuf};

/// The link line, in both spellings. A crate root gates it on `cfg(test)`; a `tests/*.rs`
/// binary is only ever built for tests, so it does not.
const LINK: &str = "extern crate hrdr_test_support;";

#[test]
fn every_crate_root_links_the_sandbox_ctor() {
    let mut missing = Vec::new();
    for krate in workspace_crates() {
        // The crate that DEFINES the ctor cannot `extern crate` itself.
        if krate.file_name().is_some_and(|n| n == "hrdr-test-support") {
            continue;
        }
        let roots = ["src/lib.rs", "src/main.rs"];
        let present: Vec<PathBuf> = roots
            .iter()
            .map(|r| krate.join(r))
            .filter(|p| p.is_file())
            .collect();
        for root in present {
            let src = std::fs::read_to_string(&root).unwrap();
            if !src.contains(LINK) {
                missing.push(root);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these crate roots do not link the sandbox ctor, so their unit tests run against \
         the developer's real $HOME:\n{}\n\nAdd to the crate root:\n    #[cfg(test)]\n    \
         {LINK}\nand to [dev-dependencies]:\n    hrdr-test-support.workspace = true",
        list(&missing)
    );
}

#[test]
fn every_integration_test_binary_links_the_sandbox_ctor() {
    let mut missing = Vec::new();
    for krate in workspace_crates() {
        let tests = krate.join("tests");
        let Ok(entries) = std::fs::read_dir(&tests) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "rs")
                && !std::fs::read_to_string(&p).unwrap().contains(LINK)
            {
                missing.push(p);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these integration tests are their own binaries and do not link the sandbox ctor, \
         so they run against the developer's real $HOME:\n{}\n\nAdd at the top of the \
         file:\n    {LINK}",
        list(&missing)
    );
}

/// Every crate directory in the workspace: `crates/*` and `apps/*`.
fn workspace_crates() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<crate> sits two levels under the workspace root");
    let mut out = Vec::new();
    for group in ["crates", "apps"] {
        let Ok(entries) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().join("Cargo.toml").is_file() {
                out.push(e.path());
            }
        }
    }
    assert!(!out.is_empty(), "the workspace has crates");
    out.sort();
    out
}

fn list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

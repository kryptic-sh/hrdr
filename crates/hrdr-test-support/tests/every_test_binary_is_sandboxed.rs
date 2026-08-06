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

use std::path::PathBuf;

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

/// Every crate directory listed in the root manifest's `[workspace] members`.
///
/// Members only, deliberately. A crate outside the workspace (`[workspace] exclude`) is
/// never compiled or run by the workspace `cargo test`, so it has no test binary that
/// could reach the developer's real $HOME through this harness, and the link line would
/// buy nothing there. The root manifest currently has no `exclude` key at all, so today
/// this is the whole workspace.
///
/// The flip side: a crate that is missing from `members` is not scanned. That is the same
/// condition as not being tested at all, so it costs no coverage — but it does mean adding
/// a crate to the workspace is what enrolls it here.
fn workspace_crates() -> Vec<PathBuf> {
    let root = hrdr_test_support::workspace_root();
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("the root manifest is readable");
    let mut out: Vec<PathBuf> = workspace_members(&manifest)
        .into_iter()
        .map(|m| root.join(m))
        .filter(|dir| dir.join("Cargo.toml").is_file())
        .collect();
    assert!(!out.is_empty(), "the workspace has member crates");
    out.sort();
    out
}

/// The `members = [...]` paths from the workspace manifest.
///
/// A deliberately small parser instead of a toml dependency: the array is a flat list of
/// quoted relative paths, so every odd field of a split on `"` is one entry. Globs would
/// slip through unexpanded — the workspace does not use them, and a stray `*` in a path
/// simply fails the `Cargo.toml` existence filter above rather than scanning the wrong dir.
fn workspace_members(manifest: &str) -> Vec<String> {
    let after = manifest
        .split_once("members = [")
        .expect("the root manifest declares [workspace] members")
        .1;
    let list = after
        .split_once(']')
        .expect("the members array is closed")
        .0;
    list.split('"')
        .skip(1)
        .step_by(2)
        .map(String::from)
        .collect()
}

fn list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

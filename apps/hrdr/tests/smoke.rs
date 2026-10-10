//! Binary smoke tests: the CLI launches and its arg surface is wired.

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hrdr")
}

fn build_info(command: &mut Command) -> Output {
    command.arg("--build-info").output().unwrap()
}

fn assert_build_info(out: Output) -> String {
    assert!(out.status.success());
    assert!(out.stderr.is_empty());

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0], format!("version: {}", env!("CARGO_PKG_VERSION")));

    let commit = lines[1].strip_prefix("commit: ").unwrap();
    assert!(
        commit == "unknown"
            || ((commit.len() == 40 || commit.len() == 64)
                && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
    );

    let target = lines[2].strip_prefix("target: ").unwrap();
    assert!(!target.is_empty());

    let source = lines[3].strip_prefix("source: ").unwrap();
    assert!(matches!(source, "clean" | "dirty" | "unknown"));
    stdout
}

fn git(directory: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap()
}

fn assert_git(directory: &Path, args: &[&str]) {
    let out = git(directory, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn isolated_build_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"),
        project.path().join("build.rs"),
    )
    .unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"build-identity-regression\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/main.rs"),
        "fn main() {\n    println!(\"commit: {}\", env!(\"HRDR_BUILD_COMMIT\"));\n    println!(\"source: {}\", env!(\"HRDR_BUILD_SOURCE_STATE\"));\n    println!(\"injection: {}\", option_env!(\"HRDR_INJECTION\").unwrap_or(\"none\"));\n}\n",
    )
    .unwrap();
    fs::write(project.path().join(".gitignore"), "target/\n").unwrap();
    assert_git(project.path(), &["init", "--quiet"]);
    let lockfile = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        lockfile.status.success(),
        "cargo generate-lockfile failed: {}",
        String::from_utf8_lossy(&lockfile.stderr)
    );
    assert_git(project.path(), &["add", "."]);
    assert_git(
        project.path(),
        &[
            "-c",
            "user.name=Build Test",
            "-c",
            "user.email=build-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    project
}

fn isolated_build(project: &Path) {
    let out = Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn instrument_build_script(project: &Path, counter: &Path) {
    let build_script = project.join("build.rs");
    let source = fs::read_to_string(&build_script).unwrap();
    let instrumented_main = format!(
        "fn main() {{\n    let count = fs::read_to_string({counter:?})\n        .ok()\n        .and_then(|count| count.trim().parse::<usize>().ok())\n        .unwrap_or(0);\n    fs::write({counter:?}, (count + 1).to_string()).unwrap();"
    );
    assert!(source.contains("fn main() {"));
    fs::write(
        build_script,
        source.replacen("fn main() {", &instrumented_main, 1),
    )
    .unwrap();
}

fn isolated_build_info(project: &Path) -> String {
    let out = Command::new("cargo")
        .args(["run", "--quiet"])
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn prints_build_info() {
    assert_build_info(build_info(&mut Command::new(bin())));
}

#[test]
fn build_info_exits_before_reading_invalid_config() {
    let baseline = assert_build_info(build_info(&mut Command::new(bin())));

    let invalid_root = tempfile::NamedTempFile::new().unwrap();
    let mut root_command = Command::new(bin());
    root_command.env("XDG_CONFIG_HOME", invalid_root.path());
    assert_eq!(assert_build_info(build_info(&mut root_command)), baseline);

    let config_root = tempfile::tempdir().unwrap();
    fs::create_dir(config_root.path().join("hrdr")).unwrap();
    fs::write(config_root.path().join("hrdr/config.toml"), "not = [valid").unwrap();
    let mut data_command = Command::new(bin());
    data_command.env("XDG_CONFIG_HOME", config_root.path());
    assert_eq!(assert_build_info(build_info(&mut data_command)), baseline);
}

#[test]
fn update_check_exits_before_reading_invalid_config() {
    let config_root = tempfile::tempdir().unwrap();
    fs::create_dir(config_root.path().join("hrdr")).unwrap();
    fs::write(config_root.path().join("hrdr/config.toml"), "not = [valid").unwrap();
    let out = Command::new(bin())
        .args(["update-check", "--json"])
        .env("XDG_CONFIG_HOME", config_root.path())
        .output()
        .unwrap();
    assert!(matches!(out.status.code(), Some(1 | 2)));
    assert!(out.stderr.is_empty(), "stderr must not expose Git output");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["eligible"], false);
    assert_eq!(value["restart_available"], false);
    assert_eq!(value["ci_status"], "not_checked");
    assert!(matches!(
        value["outcome"].as_str(),
        Some("ineligible" | "unavailable")
    ));
}

#[test]
fn update_check_requires_json() {
    let out = Command::new(bin()).arg("update-check").output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--json"));
}

#[test]
fn unchanged_builds_do_not_rerun_the_build_script() {
    let project = isolated_build_project();
    let counter_dir = tempfile::tempdir().unwrap();
    let counter = counter_dir.path().join("build-script-runs");
    instrument_build_script(project.path(), &counter);
    assert_git(project.path(), &["add", "build.rs"]);
    assert_git(
        project.path(),
        &[
            "-c",
            "user.name=Build Test",
            "-c",
            "user.email=build-test@example.invalid",
            "commit",
            "--quiet",
            "--amend",
            "--no-edit",
        ],
    );

    isolated_build(project.path());
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    isolated_build(project.path());
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
}

#[cfg(unix)]
#[test]
fn tracked_newline_filename_cannot_emit_a_cargo_directive() {
    let project = isolated_build_project();
    let filename = "tracked\ncargo:rustc-env=HRDR_INJECTION=bad";
    fs::write(project.path().join(filename), "// tracked\n").unwrap();
    assert_git(project.path(), &["add", "--", filename]);

    assert!(
        isolated_build_info(project.path())
            .lines()
            .any(|line| line == "injection: none"),
        "a control-character filename must not inject a Cargo directive"
    );
}

#[test]
fn build_identity_refreshes_when_a_packed_ref_becomes_loose() {
    let project = isolated_build_project();
    let root = project.path();
    assert_git(
        root,
        &[
            "-c",
            "user.name=Build Test",
            "-c",
            "user.email=build-test@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "next",
        ],
    );
    let next = String::from_utf8(git(root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let initial = String::from_utf8(git(root, &["rev-parse", "HEAD^"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let branch = String::from_utf8(git(root, &["symbolic-ref", "--short", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let reference = format!("refs/heads/{branch}");
    assert_git(root, &["update-ref", &reference, &initial]);
    assert_git(root, &["pack-refs", "--all", "--prune"]);

    assert!(
        !root.join(".git").join(&reference).exists(),
        "the active ref is packed before the first build"
    );
    let expected_initial = format!("commit: {initial}");
    assert_eq!(
        isolated_build_info(root).lines().next(),
        Some(expected_initial.as_str())
    );

    assert_git(root, &["update-ref", &reference, &next]);
    let expected_next = format!("commit: {next}");
    assert_eq!(
        isolated_build_info(root).lines().next(),
        Some(expected_next.as_str())
    );
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

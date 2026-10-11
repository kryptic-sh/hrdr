//! Validation-only contract for the hidden self-update coordinator.
//!
//! This module deliberately does not build, replace, restart, or recover anything.

use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const SCHEMA_VERSION: u32 = 1;
const MAX_RETRIES: u8 = 3;
const MAX_DEADLINE_SECS: u64 = 24 * 60 * 60;
const STAGING_DIR: &str = ".hrdr/update-staging";

#[derive(Debug)]
struct Request {
    session_id: String,
    checkout_root: PathBuf,
    expected_commit: String,
    candidate_binary: PathBuf,
    intended_cargo_bin: PathBuf,
    expected_target: String,
    operation_id: String,
    retry_limit: u8,
    deadline_secs: u64,
    checksum: String,
}

/// Dispatch the hidden argument before clap, configuration, sessions, or the TUI.
pub fn run_from_args() -> Option<anyhow::Result<()>> {
    let mut args = std::env::args_os();
    let _program = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("__update-coordinator")) {
        return None;
    }

    let result = match (args.next(), args.next()) {
        (Some(path), None) => run(PathBuf::from(path)),
        _ => Status::Invalid,
    };
    println!(
        "{{\"schema_version\":1,\"status\":\"{}\"}}",
        result.as_str()
    );
    Some(Ok(()))
}

#[derive(Clone, Copy)]
enum Status {
    Invalid,
    Reused,
    Stale,
    Validated,
    #[cfg(not(windows))]
    Unsupported,
}

impl Status {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Reused => "reused",
            Self::Stale => "stale",
            Self::Validated => "validated",
            #[cfg(not(windows))]
            Self::Unsupported => "unsupported",
        }
    }
}

fn run(request_path: PathBuf) -> Status {
    let Ok(in_progress) = claim(&request_path) else {
        return Status::Reused;
    };
    let Ok(bytes) = fs::read(&in_progress) else {
        return Status::Invalid;
    };
    let Ok(request) = parse_request(&bytes) else {
        return Status::Invalid;
    };
    let Ok(checkout_root) = validate_request_path(&in_progress, &request) else {
        return Status::Invalid;
    };
    if head_commit(&checkout_root).as_deref() != Some(&request.expected_commit) {
        return Status::Stale;
    }

    #[cfg(windows)]
    {
        let _ = request;
        Status::Validated
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Status::Unsupported
    }
}

fn claim(path: &Path) -> Result<PathBuf, ()> {
    if !path.is_absolute() || contains_control_path(path) {
        return Err(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(());
    }
    let parent = path.parent().ok_or(())?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| ())?;
    let canonical_path = fs::canonicalize(path).map_err(|_| ())?;
    if canonical_path.parent() != Some(canonical_parent.as_path()) {
        return Err(());
    }
    let in_progress = canonical_path.with_extension("in-progress");
    fs::rename(canonical_path, &in_progress).map_err(|_| ())?;
    Ok(in_progress)
}

fn validate_request_path(request_path: &Path, request: &Request) -> Result<PathBuf, ()> {
    if !valid_text(&request.session_id, 128)
        || !valid_text(&request.operation_id, 128)
        || !valid_text(&request.expected_target, 256)
        || request.retry_limit > MAX_RETRIES
        || request.deadline_secs > MAX_DEADLINE_SECS
        || !is_full_commit(&request.expected_commit)
        || !is_checksum(&request.checksum)
        || request.expected_target != env!("HRDR_BUILD_TARGET")
    {
        return Err(());
    }

    let checkout_root = canonical_absolute(&request.checkout_root)?;
    let staging_root = fs::canonicalize(checkout_root.join(STAGING_DIR)).map_err(|_| ())?;
    if request_path.parent() != Some(staging_root.as_path()) {
        return Err(());
    }
    let candidate = canonical_absolute(&request.candidate_binary)?;
    if !candidate.is_file() || !candidate.starts_with(&staging_root) {
        return Err(());
    }
    if !request.intended_cargo_bin.is_absolute()
        || contains_control_path(&request.intended_cargo_bin)
    {
        return Err(());
    }
    Ok(checkout_root)
}

fn canonical_absolute(path: &Path) -> Result<PathBuf, ()> {
    if !path.is_absolute() || contains_control_path(path) {
        return Err(());
    }
    fs::canonicalize(path).map_err(|_| ())
}

fn contains_control_path(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .chars()
        .any(char::is_control)
}

fn valid_text(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && !value.chars().any(char::is_control)
}

fn is_full_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_checksum(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn head_commit(checkout_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            checkout_root.to_str()?,
            "rev-parse",
            "--verify",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 41 || !output.stderr.is_empty() {
        return None;
    }
    let commit = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    is_full_commit(commit).then(|| commit.to_owned())
}

fn parse_request(bytes: &[u8]) -> Result<Request, ()> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    let expected: BTreeSet<_> = [
        "schema_version",
        "session_id",
        "checkout_root",
        "expected_commit",
        "candidate_binary",
        "intended_cargo_bin",
        "expected_target",
        "operation_id",
        "retry_limit",
        "deadline_secs",
        "checksum",
    ]
    .into_iter()
    .collect();
    let keys: BTreeSet<_> = object.keys().map(String::as_str).collect();
    if keys != expected {
        return Err(());
    }
    if object
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(SCHEMA_VERSION.into())
    {
        return Err(());
    }

    let string = |key| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or(())
    };
    let request = Request {
        session_id: string("session_id")?,
        checkout_root: PathBuf::from(string("checkout_root")?),
        expected_commit: string("expected_commit")?,
        candidate_binary: PathBuf::from(string("candidate_binary")?),
        intended_cargo_bin: PathBuf::from(string("intended_cargo_bin")?),
        expected_target: string("expected_target")?,
        operation_id: string("operation_id")?,
        retry_limit: object
            .get("retry_limit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u8::try_from(v).ok())
            .ok_or(())?,
        deadline_secs: object
            .get("deadline_secs")
            .and_then(serde_json::Value::as_u64)
            .ok_or(())?,
        checksum: string("checksum")?,
    };
    let checksum = sha256_hex(canonical_body(&request).as_bytes());
    (checksum == request.checksum).then_some(request).ok_or(())
}

fn canonical_body(request: &Request) -> String {
    let quote = |value: &str| serde_json::to_string(value).expect("strings serialize");
    format!(
        "{{\"schema_version\":{SCHEMA_VERSION},\"session_id\":{},\"checkout_root\":{},\"expected_commit\":{},\"candidate_binary\":{},\"intended_cargo_bin\":{},\"expected_target\":{},\"operation_id\":{},\"retry_limit\":{},\"deadline_secs\":{}}}",
        quote(&request.session_id),
        quote(&request.checkout_root.to_string_lossy()),
        quote(&request.expected_commit),
        quote(&request.candidate_binary.to_string_lossy()),
        quote(&request.intended_cargo_bin.to_string_lossy()),
        quote(&request.expected_target),
        quote(&request.operation_id),
        request.retry_limit,
        request.deadline_secs,
    )
}

fn sha256_hex(input: &[u8]) -> String {
    Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_value() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn write_request(path: &Path, request: &mut Request) {
        request.checksum = sha256_hex(canonical_body(request).as_bytes());
        let body = format!(
            "{{{}}}",
            canonical_body(request)
                .trim_start_matches('{')
                .trim_end_matches('}')
        );
        let body = body.trim_end_matches('}');
        fs::write(
            path,
            format!("{body},\"checksum\":\"{}\"}}", request.checksum),
        )
        .unwrap();
    }

    fn request(root: &Path, candidate: PathBuf) -> Request {
        Request {
            session_id: "session".into(),
            checkout_root: root.to_path_buf(),
            expected_commit: "0".repeat(40),
            candidate_binary: candidate,
            intended_cargo_bin: root.join("bin/hrdr"),
            expected_target: env!("HRDR_BUILD_TARGET").into(),
            operation_id: "operation".into(),
            retry_limit: 1,
            deadline_secs: 60,
            checksum: String::new(),
        }
    }

    #[test]
    fn malformed_tampered_and_reused_requests_are_not_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let malformed = temp.path().join("malformed.json");
        fs::write(&malformed, b"not json").unwrap();
        assert!(matches!(run(malformed.clone()), Status::Invalid));
        assert!(matches!(run(malformed), Status::Reused));

        let tampered = temp.path().join("tampered.json");
        let mut signed = request(temp.path(), temp.path().join("candidate"));
        write_request(&tampered, &mut signed);
        let body = fs::read_to_string(&tampered)
            .unwrap()
            .replace("session", "other");
        fs::write(&tampered, body).unwrap();
        assert!(matches!(run(tampered), Status::Invalid));
    }

    #[test]
    fn offroot_candidate_is_rejected_before_git() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join(STAGING_DIR);
        fs::create_dir_all(&staging).unwrap();
        let outside = temp.path().join("candidate");
        fs::write(&outside, b"candidate").unwrap();
        let path = staging.join("request.json");
        let mut signed = request(temp.path(), outside);
        write_request(&path, &mut signed);
        assert!(matches!(run(path), Status::Invalid));
    }

    #[test]
    fn stale_commit_is_rejected_after_validation() {
        let temp = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "--quiet", temp.path().to_str().unwrap()])
            .status()
            .unwrap();
        assert!(init.success());
        let staging = temp.path().join(STAGING_DIR);
        fs::create_dir_all(&staging).unwrap();
        let candidate = staging.join("candidate");
        fs::write(&candidate, b"candidate").unwrap();
        let path = staging.join("request.json");
        let mut signed = request(temp.path(), candidate);
        write_request(&path, &mut signed);
        assert!(matches!(run(path), Status::Stale));
    }
}

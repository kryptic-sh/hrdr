//! One-shot hidden self-update coordinator.
//!
//! The request is claimed by rename before it is parsed.  Windows then waits for
//! the exact old process instance to exit, installs the validated checkout, and
//! starts the installed binary's handoff recovery exactly once.

use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const SCHEMA_VERSION: u32 = 2;
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
    source_pid: u32,
    source_created: u64,
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
        "{{\"schema_version\":{SCHEMA_VERSION},\"status\":\"{}\"}}",
        result.as_str()
    );
    Some(Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Invalid,
    Reused,
    Stale,
    WaitTimeout,
    SourceUnknown,
    InstallFailed,
    IdentityFailed,
    LaunchFailed,
    Launched,
    #[cfg(not(windows))]
    Unsupported,
}

impl Status {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Reused => "reused",
            Self::Stale => "stale",
            Self::WaitTimeout => "wait_timeout",
            Self::SourceUnknown => "source_unknown",
            Self::InstallFailed => "install_failed",
            Self::IdentityFailed => "identity_failed",
            Self::LaunchFailed => "launch_failed",
            Self::Launched => "launched",
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

    #[cfg(windows)]
    {
        run_windows(&request, &checkout_root)
    }
    #[cfg(not(windows))]
    {
        let _ = (request, checkout_root);
        Status::Unsupported
    }
}

#[cfg(windows)]
fn run_windows(request: &Request, checkout_root: &Path) -> Status {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(request.deadline_secs);
    let lease = hrdr_tools::ProcessLease {
        pid: request.source_pid,
        created: request.source_created,
    };
    loop {
        match hrdr_tools::process_lease_liveness(lease) {
            hrdr_tools::ProcessLiveness::Dead | hrdr_tools::ProcessLiveness::Replaced => break,
            hrdr_tools::ProcessLiveness::Unknown => return Status::SourceUnknown,
            hrdr_tools::ProcessLiveness::Live if std::time::Instant::now() >= deadline => {
                return Status::WaitTimeout;
            }
            hrdr_tools::ProcessLiveness::Live => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
        }
    }

    if !clean_head(checkout_root, &request.expected_commit, deadline) {
        return Status::Stale;
    }

    let mut installed = false;
    for _ in 0..=request.retry_limit {
        if run_command(
            Command::new(cargo_program())
                .current_dir(checkout_root)
                .args(["install", "--path", "apps/hrdr", "--locked"]),
            deadline,
            false,
        )
        .is_ok()
        {
            installed = true;
            break;
        }
    }
    if !installed {
        return Status::InstallFailed;
    }

    if !clean_head(checkout_root, &request.expected_commit, deadline) {
        return Status::Stale;
    }
    if !installed_identity(&request.intended_cargo_bin, request, deadline) {
        return Status::IdentityFailed;
    }
    if launch(&request.intended_cargo_bin, &request.session_id) {
        Status::Launched
    } else {
        Status::LaunchFailed
    }
}

#[cfg(windows)]
fn cargo_program() -> PathBuf {
    #[cfg(test)]
    if let Some(program) = std::env::var_os("HRDR_TEST_CARGO") {
        return PathBuf::from(program);
    }
    PathBuf::from("cargo")
}

#[cfg(windows)]
fn clean_head(checkout_root: &Path, expected_commit: &str, deadline: std::time::Instant) -> bool {
    let Ok(head) = run_command(
        Command::new("git")
            .current_dir(checkout_root)
            .args(["rev-parse", "--verify", "HEAD"]),
        deadline,
        true,
    ) else {
        return false;
    };
    let Ok(status) = run_command(
        Command::new("git").current_dir(checkout_root).args([
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ]),
        deadline,
        true,
    ) else {
        return false;
    };
    std::str::from_utf8(&head)
        .ok()
        .map(str::trim)
        .is_some_and(|head| head == expected_commit)
        && status.is_empty()
}

#[cfg(windows)]
fn installed_identity(binary: &Path, request: &Request, deadline: std::time::Instant) -> bool {
    let Ok(output) = run_command(Command::new(binary).arg("--build-info"), deadline, true) else {
        return false;
    };
    let Ok(output) = std::str::from_utf8(&output) else {
        return false;
    };
    let expected = format!(
        "version: {}\ncommit: {}\ntarget: {}\nsource: clean\n",
        env!("CARGO_PKG_VERSION"),
        request.expected_commit,
        request.expected_target,
    );
    output.replace("\r\n", "\n") == expected
}

#[cfg(windows)]
fn launch(binary: &Path, session_id: &str) -> bool {
    Command::new(binary)
        .args(["__recover-handoff", session_id])
        .spawn()
        .is_ok()
}

/// Runs an argv command without a shell. Output is capped and every wait shares
/// the request deadline; a timed-out child is killed before this returns.
#[cfg(windows)]
fn run_command(
    command: &mut Command,
    deadline: std::time::Instant,
    capture: bool,
) -> Result<Vec<u8>, ()> {
    use std::io::Read;
    use std::process::Stdio;

    command.stderr(Stdio::null());
    if capture {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    let mut child = command.spawn().map_err(|_| ())?;
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let count = stdout.read(&mut chunk).unwrap_or(0);
                if count == 0 {
                    break;
                }
                if bytes.len().saturating_add(count) > 4096 {
                    return None;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Some(bytes)
        })
    });
    loop {
        if child.try_wait().map_err(|_| ())?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(reader) = reader {
                let _ = reader.join();
            }
            return Err(());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    if !child.wait().map_err(|_| ())?.success() {
        return Err(());
    }
    match reader {
        Some(reader) => reader.join().map_err(|_| ())?.ok_or(()),
        None => Ok(Vec::new()),
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
        || request.source_pid == 0
        || request.source_created == 0
        || request.retry_limit > MAX_RETRIES
        || request.deadline_secs == 0
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
        "source_pid",
        "source_created",
        "retry_limit",
        "deadline_secs",
        "checksum",
    ]
    .into_iter()
    .collect();
    let keys: BTreeSet<_> = object.keys().map(String::as_str).collect();
    if keys != expected
        || object
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
    let number = |key| {
        object
            .get(key)
            .and_then(serde_json::Value::as_u64)
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
        source_pid: u32::try_from(number("source_pid")?).map_err(|_| ())?,
        source_created: number("source_created")?,
        retry_limit: u8::try_from(number("retry_limit")?).map_err(|_| ())?,
        deadline_secs: number("deadline_secs")?,
        checksum: string("checksum")?,
    };
    let checksum = sha256_hex(canonical_body(&request).as_bytes());
    (checksum == request.checksum).then_some(request).ok_or(())
}

fn canonical_body(request: &Request) -> String {
    let quote = |value: &str| serde_json::to_string(value).expect("strings serialize");
    format!(
        "{{\"schema_version\":{SCHEMA_VERSION},\"session_id\":{},\"checkout_root\":{},\"expected_commit\":{},\"candidate_binary\":{},\"intended_cargo_bin\":{},\"expected_target\":{},\"operation_id\":{},\"source_pid\":{},\"source_created\":{},\"retry_limit\":{},\"deadline_secs\":{}}}",
        quote(&request.session_id),
        quote(&request.checkout_root.to_string_lossy()),
        quote(&request.expected_commit),
        quote(&request.candidate_binary.to_string_lossy()),
        quote(&request.intended_cargo_bin.to_string_lossy()),
        quote(&request.expected_target),
        quote(&request.operation_id),
        request.source_pid,
        request.source_created,
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
        let body = canonical_body(request).trim_end_matches('}').to_owned();
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
            source_pid: 1,
            source_created: 1,
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
        assert_eq!(run(malformed.clone()), Status::Invalid);
        assert_eq!(run(malformed), Status::Reused);

        let tampered = temp.path().join("tampered.json");
        let mut signed = request(temp.path(), temp.path().join("candidate"));
        write_request(&tampered, &mut signed);
        let body = fs::read_to_string(&tampered)
            .unwrap()
            .replace("session", "other");
        fs::write(&tampered, body).unwrap();
        assert_eq!(run(tampered), Status::Invalid);
    }

    #[test]
    fn missing_process_instance_fields_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("request.json");
        let mut signed = request(temp.path(), temp.path().join("candidate"));
        signed.source_created = 0;
        write_request(&path, &mut signed);
        assert_eq!(run(path), Status::Invalid);
    }

    #[cfg(windows)]
    #[test]
    fn launch_passes_one_exact_handoff_argv() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("launches.txt");
        let script = temp.path().join("installed.cmd");
        fs::write(
            &script,
            format!("@echo off\r\necho %1^|%2>>\"{}\"\r\n", marker.display()),
        )
        .unwrap();

        assert!(launch(&script, "saved-session"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            "__recover-handoff|saved-session\r\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn fake_cargo_installs_then_launches_once() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(
            root.join(".gitignore"),
            ".hrdr/\nfake-bin/\ncargo-home/\ncargo-args.txt\nlaunches.txt\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["add", ".gitignore"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "test"
                ])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        let commit = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let staging = root.join(STAGING_DIR);
        fs::create_dir_all(&staging).unwrap();
        let candidate = staging.join("candidate.exe");
        fs::write(&candidate, b"candidate").unwrap();
        let bin = root.join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        let cargo_args = root.join("cargo-args.txt");
        fs::write(
            bin.join("cargo.cmd"),
            format!(
                "@echo off\r\necho %*>>\"{}\"\r\nexit /b 0\r\n",
                cargo_args.display()
            ),
        )
        .unwrap();
        let cargo_home = root.join("cargo-home");
        let installed = cargo_home.join("bin/hrdr.cmd");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        let marker = root.join("launches.txt");
        fs::write(
            &installed,
            format!(
                "@echo off\r\nif \"%1\"==\"--build-info\" (\r\necho version: {}\r\necho commit: {commit}\r\necho target: {}\r\necho source: clean\r\nexit /b 0\r\n)\r\necho %1^|%2>>\"{}\"\r\n",
                env!("CARGO_PKG_VERSION"),
                env!("HRDR_BUILD_TARGET"),
                marker.display(),
            ),
        ).unwrap();
        let previous_test_cargo = std::env::var_os("HRDR_TEST_CARGO");
        let previous_cargo_home = std::env::var_os("CARGO_HOME");
        unsafe {
            std::env::set_var("HRDR_TEST_CARGO", bin.join("cargo.cmd"));
            std::env::set_var("CARGO_HOME", &cargo_home);
        }
        let request_path = staging.join("request.json");
        let mut signed = request(root, candidate);
        signed.expected_commit = commit;
        signed.intended_cargo_bin = installed;
        signed.source_pid = u32::MAX;
        signed.source_created = 1;
        write_request(&request_path, &mut signed);
        let result = run(request_path);
        unsafe {
            match previous_test_cargo {
                Some(value) => std::env::set_var("HRDR_TEST_CARGO", value),
                None => std::env::remove_var("HRDR_TEST_CARGO"),
            }
            match previous_cargo_home {
                Some(value) => std::env::set_var("CARGO_HOME", value),
                None => std::env::remove_var("CARGO_HOME"),
            }
        }
        assert_eq!(result, Status::Launched);
        assert_eq!(
            fs::read_to_string(cargo_args).unwrap(),
            "install --path apps/hrdr --locked\r\n"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            "__recover-handoff|session\r\n"
        );
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
        assert_eq!(run(path), Status::Invalid);
    }
}

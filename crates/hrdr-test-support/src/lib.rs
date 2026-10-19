//! A test can never touch the developer's real files — and no test has to ask.
//!
//! hrdr's user state lives under `$HOME` and the XDG roots: sessions and the
//! input history in `$XDG_DATA_HOME/hrdr`, `config.toml` / `auth.json` in
//! `$XDG_CONFIG_HOME/hrdr`, the models.dev catalog in
//! `$XDG_CACHE_HOME/hrdr`. The code that writes them is the code under test: run an
//! agent, submit a message, pick a model, and the *developer's* store is what moves.
//! It did, for a long time — the owner's machine ended up with 3,179 junk `tmp-*`
//! session directories and a `last_model.json` the suite had quietly rewritten.
//!
//! The previous fix was a helper called from three test constructors. That only ever
//! protects the tests that remember it, and a new test written by someone who has
//! read no documentation remembers nothing. So the isolation moved to where it cannot
//! be forgotten: a **life-before-main constructor**.
//!
//! [`sandbox_user_state`] is a `#[ctor]`. Any binary that links this crate runs it at
//! load, before `main`, before the test harness has spawned a single thread — which is
//! also the only moment `std::env::set_var` is sound (it is `unsafe` in edition 2024
//! and racy once threads exist). It points `$HOME` and all four XDG roots at a
//! throwaway per-process directory. Only test binaries link this crate (it is a
//! dev-dependency, `publish = false`), so nothing here can reach a release build.
//!
//! Every crate in the workspace pulls it in with a single line in its crate root:
//!
//! ```ignore
//! #[cfg(test)] extern crate hrdr_test_support; // sandboxes $HOME before any test runs
//! ```
//!
//! and every `tests/*.rs` integration binary — its own crate, which does *not* get the
//! library's `#[cfg(test)]` code — carries the same line without the `cfg`. A bare
//! dev-dependency is not enough: rustc never links a crate nothing references, and an
//! unlinked ctor never runs (proven, and the reason for the `extern crate` line). Two
//! automatic checks keep that from rotting:
//!
//! * `tests/every_test_binary_is_sandboxed.rs` fails if any crate or any `tests/*.rs`
//!   in the workspace is missing the line.
//! * `tests/leak_guard.rs` runs the whole suite in a subprocess whose `$HOME` and XDG
//!   roots are a sentinel directory, and fails — naming the files — if anything lands
//!   in it.

use std::path::{Path, PathBuf};

/// Env var the ctor exports: the sandbox root for this process.
const SANDBOX_ENV: &str = "HRDR_TEST_SANDBOX";
/// Env var the ctor exports: `$HOME` as it was *before* the sandbox replaced it, so a
/// test can assert it never wrote there.
const REAL_HOME_ENV: &str = "HRDR_TEST_REAL_HOME";

/// Child-test mode: leave `HOME` absent while keeping `USERPROFILE` sandboxed.
/// The ctor consumes this flag; its value must be `1`, never a profile path.
#[cfg(windows)]
pub const WINDOWS_USERPROFILE_CHILD_ENV: &str = "HRDR_TEST_WINDOWS_USERPROFILE_CHILD";

/// Point `$HOME` and every XDG root at a throwaway directory, before `main`.
///
/// Runs once per test binary, at load, single-threaded. `unsafe` marks the
/// attribute: the body calls `env::set_var` (unsafe in edition 2024), and ctor
/// 1.x requires the ctor to declare it.
#[ctor::ctor(unsafe)]
fn sandbox_user_state() {
    let root = std::env::temp_dir().join(format!("{SANDBOX_PREFIX}{}", std::process::id()));
    // A process with this pid is long gone; start from nothing.
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    for dir in ["home", "data", "config", "state", "cache", "runtime", "tmp"] {
        std::fs::create_dir_all(root.join(dir)).expect("a test sandbox is creatable");
    }
    hold_sandbox_alive(&root);
    sweep_abandoned_sandboxes(&root);

    // SAFETY: a ctor runs before `main`, and therefore before the test harness has
    // created any thread. This is the one point in a test binary's life at which
    // `set_var` has no other thread to race.
    unsafe {
        // `USERPROFILE` is the home on Windows, where `HOME` is usually unset.
        if let Some(real) = ["HOME", "USERPROFILE"]
            .into_iter()
            .find_map(|var| std::env::var_os(var).filter(|h| !h.is_empty()))
        {
            std::env::set_var(REAL_HOME_ENV, real);
        }
        std::env::set_var(SANDBOX_ENV, &root);
        std::env::set_var("HOME", &home);
        #[cfg(windows)]
        {
            let profile_child = std::env::var_os(WINDOWS_USERPROFILE_CHILD_ENV);
            std::env::remove_var(WINDOWS_USERPROFILE_CHILD_ENV);
            if profile_child.as_deref() == Some(std::ffi::OsStr::new("1")) {
                std::env::remove_var("HOME");
            }
        }
        // hrdr's own `home_dir()` helpers fall back to this one where `HOME` is
        // unset. `dirs::home_dir()` does NOT read it on Windows — it asks the shell
        // API for the profile folder — so every user-state root below has to stay
        // set: a path that falls back to `dirs` is the real profile there.
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_STATE_HOME", root.join("state"));
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        // hrdr's `tool_output_base()` reads `XDG_RUNTIME_DIR`; override it so
        // session spool directories land under the test sandbox instead of the
        // real `/run/user/$UID`, which a Landlock sandbox inherited from the
        // parent process (the hrdr agent) would otherwise block.
        std::env::set_var("XDG_RUNTIME_DIR", root.join("runtime"));
        // Session messaging registers every running session in a per-user
        // discovery directory outside `$HOME` (`/tmp` on unix, the known
        // LocalAppData folder on Windows, which ignores the environment). Give
        // the suite its own, so a test session is never discoverable by — and
        // never discovers — the developer's real sessions.
        let ipc = ipc_parent(&root);
        std::fs::create_dir_all(&ipc).expect("a test IPC directory is creatable");
        std::env::set_var("HRDR_IPC_DIR", &ipc);
        // No test reaches models.dev. Building an `Agent` warms the catalog in the
        // background, which is right in production and wrong in a test suite:
        // hundreds of agents would each open a network request, making the suite
        // slow, flaky offline, and dependent on a third party being up. Tests that
        // need catalog data pin it with `HRDR_MODELS_PATH`, which takes precedence
        // over both the cache and any fetch.
        std::env::set_var("HRDR_DISABLE_MODELS_FETCH", "1");
    }
}

/// Delete the sandbox when the test binary exits — /tmp does not need one of these per
/// run. Best-effort: a leaked thread still holding a file is not worth a failure.
/// `unsafe` marks the attribute: the dtor runs at exit, outside normal Rust
/// runtime guarantees, so dtor 1.x requires the marker even for safe bodies.
#[dtor::dtor(unsafe)]
fn remove_sandbox() {
    if let Some(root) = std::env::var_os(SANDBOX_ENV) {
        let root = PathBuf::from(root);
        let _ = std::fs::remove_dir_all(ipc_parent(&root));
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Every test binary's sandbox root is `<temp>/<SANDBOX_PREFIX><pid>`.
const SANDBOX_PREFIX: &str = "hrdr-test-sandbox-";

/// The file in a sandbox root its process holds an exclusive lock on for life.
const ALIVE_FILE: &str = ".alive";

/// How old a sandbox root with no [`ALIVE_FILE`] must be before a sweep removes
/// it: long enough that it cannot be a sibling binary still between creating its
/// root and locking the file.
const UNLOCKED_SANDBOX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The lock that marks this process's sandbox as in use; never released while the
/// process runs, and released by the OS however the process ends.
static SANDBOX_ALIVE: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

/// Lock `root`'s [`ALIVE_FILE`] for the life of the process.
///
/// The exit dtors cannot be the only cleanup: on Windows `std::process::exit` —
/// how libtest ends a binary with a failed test — calls `ExitProcess`, which runs
/// no `atexit` handler, and `dtor` registers through `atexit` there. So every
/// failing run used to leave its whole sandbox behind. A kernel lock is released
/// on any exit, which is what lets [`sweep_abandoned_sandboxes`] tell an
/// abandoned root from a live one.
///
/// The file is locked under a name no sweep opens and only then renamed into
/// place — the lock belongs to the open handle and survives the rename — so a
/// sibling binary's sweep can never take a live root's lock in the moment
/// between creating the file and locking it, which would both fail this ctor
/// and delete a running binary's sandbox.
fn hold_sandbox_alive(root: &Path) {
    let pending = root.join(".alive-pending");
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&pending)
        .expect("a test sandbox's liveness file is creatable");
    file.try_lock()
        .expect("a liveness file no other process has opened is unlocked");
    std::fs::rename(&pending, root.join(ALIVE_FILE))
        .expect("a test sandbox's liveness file is publishable");
    let _ = SANDBOX_ALIVE.set(file);
}

/// Remove sandbox roots their processes left behind, which a live process's
/// [`ALIVE_FILE`] lock rules out. A root with no such file predates it, or
/// belongs to a sibling binary that has not locked it yet, so it goes only once
/// older than [`UNLOCKED_SANDBOX_AGE`].
///
/// Best-effort, like the exit dtor it backs up: two binaries starting together
/// may race to remove the same root, and a leftover that resists removal is
/// retried by the next run rather than failing this one.
fn sweep_abandoned_sandboxes(own: &Path) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(SANDBOX_PREFIX))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if path == own || !sandbox_is_abandoned(&path) {
            continue;
        }
        let _ = std::fs::remove_dir_all(ipc_parent_for(&path, pid));
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Whether nothing holds `root` — see [`sweep_abandoned_sandboxes`].
fn sandbox_is_abandoned(root: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(ALIVE_FILE))
    {
        // Taking the lock proves its holder is gone; the handle is closed before
        // the caller removes the directory.
        Ok(file) => file.try_lock().is_ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::metadata(root)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= UNLOCKED_SANDBOX_AGE),
        Err(_) => false,
    }
}

/// The parent of the suite's session-messaging discovery directory.
///
/// Unix sockets live in it, and a socket path is capped at about a hundred
/// bytes (`sun_path`). macOS's per-user temp dir is long enough that a socket
/// under the sandbox would not fit, so unix uses a short per-process `/tmp`
/// directory instead, removed at exit like the sandbox.
fn ipc_parent(root: &Path) -> PathBuf {
    ipc_parent_for(root, std::process::id())
}

/// [`ipc_parent`] for the process `pid` that owns `root`.
fn ipc_parent_for(root: &Path, pid: u32) -> PathBuf {
    if cfg!(unix) {
        PathBuf::from("/tmp").join(format!("hrdr-test-ipc-{pid}"))
    } else {
        root.join("runtime")
    }
}

/// A private temp dir inside this process's sandbox root rather than the system
/// temp dir — what a test hands to [`defer_tempdir`] should come from here, so
/// that a run whose exit dtors never ran leaves it where the next run's sweep
/// reclaims it (see [`hold_sandbox_alive`]).
pub fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir_in(sandbox_root().join("tmp")).expect("a sandbox temp dir is creatable")
}

/// Keep a test's private temp dir alive until the test binary exits.
///
/// The one place a `TempDir` legitimately outlives the test that created it: a
/// private XDG root that parallel sibling tests resolve paths into (the e2e
/// harness's `DataHomeGuard`), and the wire-log test's latched log file, both
/// need the directory to survive until the last sibling thread is done. Handing
/// the `TempDir` here defers its drop to the exit dtor below instead of leaving
/// the directory on disk forever — the correctness window is the process, which
/// is exactly how long the registry holds it.
pub fn defer_tempdir(tmp: tempfile::TempDir) {
    DEFERRED_TEMP_DIRS
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(tmp);
}

/// Temp dirs that must outlive their creating test (see [`defer_tempdir`]),
/// held until the exit dtor drops them.
static DEFERRED_TEMP_DIRS: std::sync::OnceLock<std::sync::Mutex<Vec<tempfile::TempDir>>> =
    std::sync::OnceLock::new();

/// Run `f` with a private `XDG_DATA_HOME` — a `tempfile` tempdir — restored
/// afterwards. ONE lock for the whole process: the var is process-global, so
/// two test modules each holding their own lock would swap it under each other,
/// which is exactly how hrdr-app's completion and sessions tests raced and
/// flaked (a sessions test's root appeared where the completion test's sessions
/// should have been). `std::env::set_var` is `unsafe` in edition 2024; the lock
/// is held for the whole call so no other env-swapping helper can race it.
///
/// Restored to what it was, never removed: the ctor's sandbox root is what every
/// later test in the binary relies on, and with the variable unset the data dir
/// falls back to `$HOME/.local/share` — which on Windows is the developer's real
/// profile, because `dirs` ignores the sandboxed `HOME` there.
pub fn with_test_env(f: impl FnOnce(&tempfile::TempDir)) {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = std::env::var_os("XDG_DATA_HOME");
    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("XDG_DATA_HOME", tmp.path()) };
    f(&tmp);
    match previous {
        Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }
}

/// Drop the deferred temp dirs at process exit: `TempDir::drop` removes the
/// directory, so what the tests kept alive until the last sibling finished is
/// reclaimed instead of accumulating in /tmp run after run.
#[dtor::dtor(unsafe)]
fn drop_deferred_temp_dirs() {
    let Some(reg) = DEFERRED_TEMP_DIRS.get() else {
        return;
    };
    let roots = match reg.lock() {
        Ok(mut r) => std::mem::take(&mut *r),
        Err(p) => std::mem::take(&mut *p.into_inner()),
    };
    // Dropping the Vec drops each `TempDir` (and its `remove_dir_all`).
    drop(roots);
}

/// The workspace root: the nearest directory, from the running package up,
/// whose `Cargo.toml` is a `[workspace]` manifest. Test binaries that walk the
/// whole workspace (crate inventory, duration-constant scan, the leak guard)
/// all derive it the same way.
///
/// Starts from the `CARGO_MANIFEST_DIR` cargo sets when it runs a test, not
/// this crate's compile-time one: cargo reuses a binary built from another
/// worktree sharing `CARGO_TARGET_DIR`, and a path baked in at compile time
/// then names that worktree. The compile-time path is the fallback for a
/// binary run by hand.
pub fn workspace_root() -> PathBuf {
    let start = std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
    workspace_root_from(&start).expect("a test runs inside the hrdr workspace")
}

/// The nearest ancestor of `start` (itself included) holding a `[workspace]`
/// manifest.
fn workspace_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| {
            std::fs::read_to_string(dir.join("Cargo.toml"))
                .is_ok_and(|manifest| manifest.lines().any(|line| line.trim() == "[workspace]"))
        })
        .map(Path::to_path_buf)
}

/// Every crate directory listed in the root manifest's `[workspace] members`.
///
/// Members only, deliberately — the workspace is what cargo builds, and a directory left
/// behind on disk by a removed crate is not part of it. Both workspace-walking rules
/// (`every_test_binary_is_sandboxed`, `duration_constant_names`) go through here so
/// neither can quietly disagree with the other about what "the workspace" means. A crate
/// outside it (`[workspace] exclude`) is never compiled or run by the workspace
/// `cargo test`, so no rule about test binaries or source conventions has anything to say
/// about it. The root manifest currently has no `exclude` key at all.
///
/// The flip side: a crate that is missing from `members` is not scanned. That is the same
/// condition as not being tested at all, so it costs no coverage — but it does mean adding
/// a crate to the workspace is what enrolls it here.
pub fn workspace_crates() -> Vec<PathBuf> {
    workspace_crates_in(&workspace_root())
}

/// [`workspace_crates`] for an arbitrary workspace root — what lets the rules that use it
/// be tested against a fixture workspace instead of only against this repo.
pub fn workspace_crates_in(root: &Path) -> Vec<PathBuf> {
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

/// The throwaway root the ctor installed for this process.
///
/// Panics if the ctor did not run — which means this crate was not linked, and the test
/// asking is running against the developer's real files.
pub fn sandbox_root() -> PathBuf {
    std::env::var_os(SANDBOX_ENV).map(PathBuf::from).expect(
        "the hrdr-test-support ctor ran: `extern crate hrdr_test_support;` is in the crate root",
    )
}

/// The XDG roots as the ctor set them: `(data, config, cache)`.
///
/// What a test holding a *private* root of its own restores to when it releases it.
pub fn user_state_dirs() -> (PathBuf, PathBuf, PathBuf) {
    let root = sandbox_root();
    (root.join("data"), root.join("config"), root.join("cache"))
}

/// `$HOME` (`%USERPROFILE%` where `HOME` is unset) as it was before the sandbox replaced
/// it — the developer's real home.
///
/// For tests that assert a write did *not* land there. `None` when the environment had
/// neither to begin with (a bare CI container).
pub fn real_home() -> Option<PathBuf> {
    std::env::var_os(REAL_HOME_ENV)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Assert that `path` is inside this process's sandbox, and not under the real
/// home when the OS temp root itself is outside that home.
///
/// The shape of the promise this crate makes, spelled out for a test that wants to
/// check it: whatever hrdr resolved as a user-state path came from the sandbox.
pub fn assert_sandboxed(path: &Path) {
    let root = sandbox_root();
    assert!(
        path.starts_with(&root),
        "{} is outside the test sandbox {}",
        path.display(),
        root.display()
    );
    if let Some(real) = real_home()
        && !root.starts_with(&real)
    {
        assert!(
            !path.starts_with(&real),
            "{} is under the developer's real home {}",
            path.display(),
            real.display()
        );
    }
}

/// Whether a test should skip for want of a prerequisite (a shell, an
/// interpreter, a sandbox backend) — **never in CI**.
///
/// Locally a missing prerequisite is an environment fact, and failing on it says
/// nothing about the code. On a runner it is a broken environment, and the only
/// useful thing a test can do is fail: a skip that cannot tell those apart turns
/// an infrastructure failure into a green tick, which is worse than either. That
/// is how whole families of tests once reported "passed" on a platform where they
/// had exercised nothing.
///
/// Call it as `if skip_for_want_of("python", found) { return; }`.
pub fn skip_for_want_of(what: &str, present: bool) -> bool {
    if present {
        return false;
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is missing on a CI runner — that is a broken environment, not a \
         reason to report the test as passed"
    );
    eprintln!("skipping: {what} is not available on this machine");
    true
}

/// Create a symlink at `link` pointing at `target` — relative targets resolve
/// against `link`'s directory, as the OS resolves them — and return whether it
/// was created. Windows makes a directory or a file link by the target's kind
/// (a missing target gets a file link) and refuses both without Developer Mode
/// or the create-symlink privilege; that refusal skips through
/// [`skip_for_want_of`], so a runner that ought to have the privilege fails
/// instead of passing having tested nothing.
///
/// Call it as `if !symlink_or_skip(target, &link) { return; }`.
pub fn symlink_or_skip(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).expect("create symlink");
        true
    }
    #[cfg(windows)]
    {
        /// `ERROR_PRIVILEGE_NOT_HELD`: no Developer Mode, no privilege.
        const PRIVILEGE_NOT_HELD: i32 = 1314;
        let resolved = link
            .parent()
            .map_or_else(|| target.to_path_buf(), |dir| dir.join(target));
        let created = if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        match created {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(PRIVILEGE_NOT_HELD) => {
                !skip_for_want_of("the privilege to create symlinks", false)
            }
            Err(error) => panic!("create symlink {}: {error}", link.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// The sandbox's data root survives a private swap: after `with_test_env` the
    /// variable is the ctor's value again, not unset — unset sends the next test's
    /// sessions to the fallback under the real profile on Windows.
    #[test]
    fn with_test_env_hands_the_sandbox_root_back() {
        let sandboxed = super::user_state_dirs().0;
        assert_eq!(
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            Some(sandboxed.as_os_str())
        );
        super::with_test_env(|tmp| {
            assert_eq!(
                std::env::var_os("XDG_DATA_HOME").as_deref(),
                Some(tmp.path().as_os_str())
            );
        });
        assert_eq!(
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            Some(sandboxed.as_os_str())
        );
    }

    /// The root is found from wherever the walk starts, by its `[workspace]`
    /// manifest — a member's own manifest, which only inherits from it, is
    /// passed over.
    #[test]
    fn the_workspace_root_is_the_nearest_workspace_manifest() {
        let (root, member) = fake_workspace();
        assert_eq!(
            super::workspace_root_from(&member).as_deref(),
            Some(root.path())
        );
        assert_eq!(
            super::workspace_root(),
            super::workspace_root_from(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap(),
            "this checkout's own root, found the same way"
        );
    }

    /// The walk starts from the `CARGO_MANIFEST_DIR` cargo sets at run time, not
    /// the one compiled in — the case of a binary built in another worktree and
    /// reused here. A child run of this binary is handed a fake member's dir.
    #[test]
    fn the_workspace_root_follows_the_run_time_manifest_dir() {
        let (root, member) = fake_workspace();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::workspace_root_child",
                "--ignored",
                "--nocapture",
            ])
            .env("CARGO_MANIFEST_DIR", &member)
            .env("HRDR_EXPECTED_WORKSPACE_ROOT", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child test ran: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    #[ignore = "child of the_workspace_root_follows_the_run_time_manifest_dir"]
    fn workspace_root_child() {
        let expected = std::env::var_os("HRDR_EXPECTED_WORKSPACE_ROOT").unwrap();
        assert_eq!(super::workspace_root(), Path::new(&expected));
    }

    /// A workspace root holding one member, whose manifest only inherits.
    fn fake_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .unwrap();
        let member = root.path().join("crates").join("one");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"one\"\nversion.workspace = true\n",
        )
        .unwrap();
        (root, member)
    }

    /// A sandbox is reclaimed only once nothing holds its liveness lock, so a
    /// sweep never removes a running binary's root — this process's own
    /// included — and does remove one whose process ended without its dtors.
    #[test]
    fn only_an_unheld_sandbox_is_abandoned() {
        assert!(
            !super::sandbox_is_abandoned(&super::sandbox_root()),
            "this process holds its own sandbox"
        );

        let root = tempfile::tempdir().unwrap();
        let alive = std::fs::File::create(root.path().join(super::ALIVE_FILE)).unwrap();
        alive.try_lock().unwrap();
        assert!(
            !super::sandbox_is_abandoned(root.path()),
            "a held lock means a live owner"
        );
        drop(alive);
        assert!(
            super::sandbox_is_abandoned(root.path()),
            "a released lock means the owner is gone"
        );

        let unlocked = tempfile::tempdir().unwrap();
        assert!(
            !super::sandbox_is_abandoned(unlocked.path()),
            "a fresh root with no liveness file may be a sibling still starting"
        );
    }
}

//! Kill the whole process *tree*, not just the direct child.
//!
//! Every subprocess this crate spawns is started with `kill_on_drop(true)`
//! and, on the explicit kill paths (timeout, cancel), an extra kill call.
//! Both of those act on the single pid tokio spawned — the shell leader.
//! That is not enough: `bash -c "npm run dev"` forks `node`, and killing only
//! `bash` leaves `node` running (holding its port) forever. "Esc stops
//! everything" needs to mean everything.
//!
//! The fix is platform-specific, but both cover the two paths that matter — an
//! *explicit* kill (timeout/overflow) and a *dropped* future (Esc-cancelled
//! turn, LSP registry teardown):
//!
//! * **Unix** — [`configure`] puts the child in a *new process group* of its
//!   own (`pgid == pid`) before it is spawned. [`ProcessGroup::kill`] signals
//!   the *negative* pid — the whole group — which reaches every descendant that
//!   hasn't itself broken out of the group (e.g. with its own `setsid`). The
//!   guard also carries the pgid and its `Drop` repeats that group-kill, so a
//!   future being torn down without an explicit kill (the Esc/cancel path, which
//!   `handle.abort()`s the turn and drops its locals) still takes the tree down
//!   — not just the leader that `kill_on_drop` reaps.
//! * **Windows** — there is no process-group equivalent, so each child is
//!   assigned to a Windows *Job Object* carrying
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: every process in the job (and
//!   anything it spawns, unless that child opts out with
//!   `CREATE_BREAKAWAY_FROM_JOB`) is terminated the moment the job is
//!   explicitly terminated *or* its last handle is closed — so dropping the
//!   [`ProcessGroup`] guard kills the tree too. A `taskkill /T /F` shell-out was
//!   the other option; a Job Object was chosen because it doesn't spawn yet
//!   another process to do the killing.
//!
//! Known limitation (Windows only): the child is spawned running and assigned to
//! the job a moment later, so a descendant forked in that narrow window escapes
//! the job. The race-free form (`CREATE_SUSPENDED` → assign → `ResumeThread`) is
//! awkward through tokio's spawn API; the window is tiny and this is a
//! best-effort resource guard, not a security boundary. Unix has no such race —
//! `process_group(0)` is applied pre-exec, atomically with the spawn.

use std::ffi::{OsStr, OsString};
use std::io;

/// The program to hand `Command::new` for a configured program name: a bare name
/// resolved through `PATH` (the child's own `PATH` when `path` overrides it) to the
/// file that search actually finds, anything else unchanged.
///
/// `Command::new("name")` is not a `PATH` search on Windows. std looks in the
/// application's directory and in System32 and the Windows directory *before*
/// `PATH`, and tries a name without an extension only as `name.exe`. So `bash`
/// meant System32's WSL launcher even with Git Bash first on `PATH`, and a Node
/// shim — `npx.cmd`, `typescript-language-server.cmd` — could not be started at
/// all. `which` walks `PATH` in order with `PATHEXT`, and std runs a resolved
/// `.cmd`/`.bat` through `cmd.exe` with its own argument escaping.
///
/// On unix the result is the file `exec` would have found by the same search, so
/// resolving there changes nothing but makes the lookup one code path everywhere.
/// A path with a separator is left for the spawn to interpret (relative to the
/// child's working directory, which `which` does not know), and a name nothing on
/// `PATH` provides is passed through so the spawn error still names it.
pub(crate) fn resolve_program(program: &str, path: Option<&OsStr>) -> OsString {
    if std::path::Path::new(program).components().count() != 1 {
        return program.into();
    }
    let search = path
        .map(OsStr::to_os_string)
        .or_else(|| std::env::var_os("PATH"));
    which::which_in_global(program, search)
        .ok()
        .and_then(|mut found| found.next())
        .map_or_else(|| program.into(), OsString::from)
}

/// Whether process `pid` is still running.
///
/// For telling an abandoned lock or per-process directory from one a live hrdr
/// still owns, so every answer this cannot give is "alive": waiting out a lock or
/// keeping a stale directory is recoverable, reaping a live process's is not.
/// Verified on Linux locally; the Windows arm is exercised by the cross-process
/// lock tests on the Windows CI runner.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill(0, 0)` would probe this process's own group and `-1` every
        // process; no real pid is either.
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 delivers nothing; the call only reports existence.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        // Only ESRCH proves it is gone. EPERM is a live process of another user.
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

        // Fixed Win32 ABI values, spelled out: the names are what move between
        // `windows-sys` releases.
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const ERROR_INVALID_PARAMETER: u32 = 87;
        const WAIT_OBJECT_0: u32 = 0;

        // SAFETY: by-value arguments only; a non-null handle is closed exactly
        // once, below.
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // No such pid is the one refusal that means gone; access denied is a
            // live process hrdr may not open.
            return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
        }
        // A process handle is signalled once the process has exited — unlike its
        // exit code, which a live process can never be told apart from one that
        // exited with `STILL_ACTIVE`. A failed wait answers "alive".
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        unsafe { CloseHandle(handle) };
        wait != WAIT_OBJECT_0
    }
}

/// Put `cmd`'s future child in a position to have its whole process tree
/// killed later, not just its own pid. Call this before `spawn()`, alongside
/// the stdio/`kill_on_drop` setup every call site already does.
///
/// Unix: makes the child the leader of a brand new process group
/// (`process_group(0)` — pgid becomes the child's own pid), so
/// [`ProcessGroup::kill`] can later signal `-pgid` for the whole group.
///
/// Windows: a no-op here. Grouping happens after spawn, once there is a
/// process handle to assign into a Job Object — see [`ProcessGroup::attach`].
pub(crate) fn configure(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        let _ = cmd;
    }
}

/// Spawn `cmd` with its whole process tree made killable: [`configure`] it,
/// `spawn()` it, and [`attach`](ProcessGroup::attach) a group to the resulting
/// child — the three steps that only work together, in that order.
///
/// Prefer this over a bare `cmd.spawn()` for anything long-running. The stdio
/// and `kill_on_drop(true)` setup stays with the caller (it differs per site);
/// what this owns is the tree-kill invariant: `kill_on_drop`, and an explicit
/// `child.kill()`, both act only on the single pid tokio spawned — the shell
/// leader — so whatever that shell forked survives them. See the module docs
/// for how each platform makes the tree reachable instead.
///
/// The returned [`GroupKill`] is the one place the explicit tree-kill is
/// written; call [`GroupKill::kill`] on whatever path decides to stop the child
/// early (a timeout elapsing, an output cap overflowing). The
/// dropped-future path needs no call — the guard's `Drop` covers it. A path
/// that completes *normally* must [`disarm`](GroupKill::disarm) the guard
/// first: the drop SIGKILLs the whole group, and a descendant the command
/// backgrounded on purpose (stdio redirected away from the caller's pipes)
/// legitimately outlives the leader.
///
/// The spawn is deliberately *outside* the caller's timeout race: the pid and
/// group handle have to be captured while the `Child` is still in hand, and the
/// timed future typically consumes it (`wait_with_output()`).
///
/// A failed attach is fatal here — the child is dropped (and so reaped by
/// `kill_on_drop`) and the error propagates. Callers that would rather run a
/// child un-guarded than not run it at all use [`spawn_group_best_effort`].
pub(crate) fn spawn_group(
    cmd: &mut tokio::process::Command,
) -> io::Result<(tokio::process::Child, GroupKill)> {
    configure(cmd);
    let child = cmd.spawn()?;
    let pid = child.id();
    let group = ProcessGroup::attach(&child)?;
    Ok((
        child,
        GroupKill {
            group: Some(group),
            pid,
        },
    ))
}

/// Like [`spawn_group`], but a failed *attach* is not fatal: the child is
/// returned anyway, with a [`GroupKill`] whose [`kill`](GroupKill::kill) is a
/// no-op. Only a spawn failure is an error.
///
/// For callers whose child is worth running even un-guarded — a formatter hook
/// is useful whether or not its forks can be reached later — where refusing to
/// run it would be the bigger regression. (Attach only ever fails on Windows,
/// where it creates a Job Object.)
pub(crate) fn spawn_group_best_effort(
    cmd: &mut tokio::process::Command,
) -> io::Result<(tokio::process::Child, GroupKill)> {
    configure(cmd);
    let child = cmd.spawn()?;
    let pid = child.id();
    let group = ProcessGroup::attach(&child).ok();
    Ok((child, GroupKill { group, pid }))
}

/// A spawned child's tree-kill handle: the [`ProcessGroup`] guard plus the pid
/// [`ProcessGroup::kill`] needs, captured at spawn so the caller doesn't have to
/// carry it past the point where the `Child` is moved into a timed future.
///
/// `group` is `None` only for a [`spawn_group_best_effort`] child whose attach
/// failed; [`kill`](GroupKill::kill) is then a no-op and only `kill_on_drop` (or
/// an explicit child kill) reaches the leader.
pub(crate) struct GroupKill {
    group: Option<ProcessGroup>,
    pid: Option<u32>,
}

impl GroupKill {
    /// Kill the whole tree, now — the explicit path (timeout, output cap).
    /// Pair with a direct child kill where the leader must also be reaped
    /// immediately rather than whenever the `Child` is dropped.
    pub(crate) fn kill(&self) {
        if let Some(group) = &self.group {
            group.kill(self.pid);
        }
    }

    /// Disarm the guard: from here on both [`kill`](GroupKill::kill) and the
    /// guard's `Drop` are no-ops. A caller that completed *normally* must call
    /// this before the guard drops — a command can background a child on
    /// purpose (stdio redirected away from the caller's pipes), and the
    /// drop's group-kill would SIGKILL it milliseconds after the leader exits.
    ///
    /// The group is taken out and forgotten rather than set to `None`: the
    /// guard's own `Drop` IS the kill, so dropping it here would fire exactly
    /// the signal this is meant to prevent. On unix the forgotten value is a
    /// bare pgid — nothing leaks. On windows it is the job handle, left open
    /// deliberately so the kill-on-close never fires (one leaked handle per
    /// disarmed command — the price of not killing).
    pub(crate) fn disarm(&mut self) {
        if let Some(group) = self.group.take() {
            std::mem::forget(group);
        }
        self.pid = None;
    }
}

/// Handle to whatever OS resource lets [`kill`](ProcessGroup::kill) take down
/// an entire process tree spawned through a [`configure`]d `Command`.
///
/// Unix carries the leader pid (which *is* the group's pgid) so both an explicit
/// [`kill`](ProcessGroup::kill) and the guard's `Drop` can signal `-pgid`.
/// Windows carries the Job Object the child was assigned to.
pub(crate) struct ProcessGroup {
    #[cfg(unix)]
    pgid: Option<u32>,
    #[cfg(windows)]
    job: windows_job::Job,
}

impl ProcessGroup {
    /// Attach to `child` right after spawning it (before awaiting anything
    /// else on it). Unix: records the leader pid (the group already exists
    /// because `cmd` was [`configure`]d before `spawn()`). Windows: creates a
    /// kill-on-close Job Object and assigns `child` to it.
    pub(crate) fn attach(child: &tokio::process::Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self { pgid: child.id() })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                job: windows_job::Job::new_and_assign(child)?,
            })
        }
    }

    /// Kill every process in the tree, now. `pid` is the child's own pid, as
    /// reported by `Child::id()` right after spawn — used on unix to target
    /// `-pid` (the whole process group); ignored on windows, where the job
    /// handle alone identifies the whole tree.
    ///
    /// A `pid` of `None` (the child already reaped) is a silent no-op. The
    /// guard's `Drop` performs the same group-kill, so the explicit call here
    /// is really just "don't wait for the drop" on the timeout/overflow paths.
    pub(crate) fn kill(&self, pid: Option<u32>) {
        #[cfg(unix)]
        {
            if let Some(pid) = pid {
                unix_group_kill(pid);
            }
        }
        #[cfg(windows)]
        {
            let _ = pid;
            self.job.terminate();
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The drop-path backstop: a cancelled turn `abort()`s its task, which
        // drops this guard's locals without calling `kill()`. `kill_on_drop`
        // only reaps the leader; this takes the whole group down, so "Esc stops
        // everything" holds on unix too (matching the Windows job-handle-close
        // behaviour).
        //
        // NOT harmless on the normal path: a descendant that outlives the
        // leader (a child the command backgrounded with stdio redirected away
        // from the caller's pipes) is still in the group and is SIGKILLed
        // here. A caller that completed normally MUST call
        // [`GroupKill::disarm`] before dropping the guard so such a child
        // survives; this drop is the cancellation backstop, for the paths
        // where the task was torn down without an explicit `kill()`.
        if let Some(pgid) = self.pgid {
            unix_group_kill(pgid);
        }
    }
}

/// SIGKILL the whole process group led by `pid` (`kill(-pid)`).
///
/// Guards `pid > 1`: `-0` would signal the *caller's* own group and `-1` every
/// process on the system. A real child pid from `Child::id()` is always `> 1`,
/// so this only ever hardens against future misuse. ESRCH (group already gone)
/// is the common, ignored case.
#[cfg(unix)]
fn unix_group_kill(pid: u32) {
    if pid > 1 {
        // SAFETY: `libc::kill` is a plain syscall wrapper; with any argument it
        // can only fail (ESRCH/EPERM), never cause undefined behaviour.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
mod windows_job {
    use std::io;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// A Windows Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set:
    /// every process assigned to it — and anything *that* spawns, unless it
    /// opts out with `CREATE_BREAKAWAY_FROM_JOB` — is terminated the moment
    /// the job is explicitly terminated or its last handle is closed. This is
    /// the Windows analogue of a unix process group plus `kill(-pgid)`.
    pub(crate) struct Job {
        handle: windows_sys::Win32::Foundation::HANDLE,
    }

    // The HANDLE is an opaque kernel-object reference; nothing about it is
    // thread-affine, so it's fine to hold across await points / move between
    // the tokio runtime's worker threads.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// Create a fresh kill-on-close job and assign `child` to it.
        pub(crate) fn new_and_assign(child: &tokio::process::Child) -> io::Result<Self> {
            // SAFETY: FFI calls per the documented Win32 Job Object API,
            // using out-parameters/handles exactly as their signatures
            // require; every failure path is checked and propagated.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self { handle };

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error()); // `job`'s Drop closes the handle
            }

            let Some(raw) = child.raw_handle() else {
                return Err(io::Error::other("child has already exited"));
            };
            let ok = unsafe { AssignProcessToJobObject(job.handle, raw) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        /// Terminate everything in the job immediately.
        pub(crate) fn terminate(&self) {
            // SAFETY: `handle` is a valid Job Object handle for the lifetime
            // of `self`.
            unsafe {
                TerminateJobObject(self.handle, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing the job's last handle terminates anything still in it
            // (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) — the drop-path backstop,
            // mirroring what `kill_on_drop` gives the leader pid on unix, but
            // covering the whole tree.
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::time::Instant;

    use crate::test_env;

    /// A grandchild started by a `configure`d command is reachable through
    /// its group: the kill reaches it, not just the leader.
    ///
    /// This exercises the primitive directly (spawn a shell that backgrounds
    /// a sleep, then kill the group) rather than going through a `Tool`, to
    /// keep the unit test fast and independent of any particular call site's
    /// plumbing. The end-to-end version (via the `bash` tool's timeout path)
    /// lives in `tools/shell.rs`.
    #[tokio::test]
    async fn killing_the_group_reaches_a_backgrounded_grandchild() {
        let Some(shell) = test_env::shell() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-alive");

        let started = Instant::now();
        let mut cmd = shell.command(&format!(
            "{} wait",
            test_env::backgrounded_grandchild(shell, &marker)
        ));
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        super::configure(&mut cmd);

        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let group = super::ProcessGroup::attach(&child).unwrap();

        // Give the backgrounded `sleep` a moment to actually start before we
        // kill the group out from under it.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        group.kill(pid);
        let _ = child.kill().await; // reap the leader, as every real call site does

        assert!(
            !test_env::grandchild_finished(&marker, started).await,
            "the grandchild's sleep completed — the group kill never reached it"
        );
    }

    /// The Esc/cancel path drops the future's locals without calling `kill()`.
    /// Dropping the [`ProcessGroup`](super::ProcessGroup) guard must still take
    /// the whole tree down, not just the leader `kill_on_drop` reaps.
    #[tokio::test]
    async fn dropping_the_guard_kills_the_group_not_just_the_leader() {
        let Some(shell) = test_env::shell() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-alive");

        let started = Instant::now();
        let mut cmd = shell.command(&format!(
            "{} wait",
            test_env::backgrounded_grandchild(shell, &marker)
        ));
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        super::configure(&mut cmd);

        let child = cmd.spawn().unwrap();
        let group = super::ProcessGroup::attach(&child).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Simulate the cancelled future's teardown: drop the guard (group-kill
        // via `Drop`) and the child (`kill_on_drop` reaps the leader). No
        // explicit `kill()` call — that is the whole point.
        drop(group);
        drop(child);

        assert!(
            !test_env::grandchild_finished(&marker, started).await,
            "the grandchild's sleep completed — dropping the guard did not reach it"
        );
    }

    /// A bare name resolves to the file a `PATH` search finds — on Windows that
    /// includes a `.cmd` shim, which `Command::new` alone never finds. The search
    /// uses the `PATH` it is given, so a child whose `PATH` is overridden is
    /// resolved against that one.
    #[test]
    fn a_bare_name_resolves_through_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let file = dir.path().join("hrdr-probe-tool.cmd");
        #[cfg(not(windows))]
        let file = dir.path().join("hrdr-probe-tool");
        std::fs::write(&file, "exit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let resolved = super::resolve_program("hrdr-probe-tool", Some(dir.path().as_os_str()));
        assert_eq!(std::path::Path::new(&resolved), file);
    }

    /// What no search can place is left alone: an unknown name reaches the spawn
    /// as typed, so its error names it, and a path is the spawn's to interpret.
    #[test]
    fn an_unknown_name_or_a_path_passes_through() {
        let empty = tempfile::tempdir().unwrap();
        let path = Some(empty.path().as_os_str());
        assert_eq!(
            super::resolve_program("hrdr-no-such-tool", path),
            OsStr::new("hrdr-no-such-tool")
        );
        assert_eq!(
            super::resolve_program("./bin/tool", path),
            OsStr::new("./bin/tool")
        );
    }

    /// A running process reads as alive, and one that has exited and been reaped
    /// reads as gone — on the platform's own probe, whichever that is.
    #[test]
    fn a_live_process_is_alive_and_a_reaped_one_is_not() {
        assert!(super::process_alive(std::process::id()), "this process");

        let exe = std::env::current_exe().unwrap();
        // `--list` makes the test harness print its test names and exit, so the
        // child runs nothing.
        let mut child = std::process::Command::new(exe)
            .args(["--list", "--exact", "no-such-test"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(
            !super::process_alive(pid),
            "pid {pid} exited and was reaped"
        );
    }
}

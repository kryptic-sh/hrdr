//! The config watcher against a real filesystem: a save burst must collapse into
//! far fewer reloads than the events the OS emits for it.
//!
//! Lives in its own integration binary because it sets `XDG_CONFIG_HOME` for the
//! whole process (`config_file_path()` reads it), which would race the unit
//! tests running in the library's test binary.

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Poll `cond` every 20ms until it holds or `timeout` elapses. Returns whether
/// it became true. Filesystem-notify delivery is asynchronous and, on macOS
/// (FSEvents) and Windows or a loaded CI runner, can lag well past a fixed
/// sleep — so the tests below wait for the invariant rather than asserting after
/// an arbitrary nap (the historical source of CI-only flakes).
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

/// Writing the config file a few times in quick succession — what an editor
/// save looks like from the outside — coalesces into a single reload, or at
/// worst a couple when the OS batches the events across the debounce window.
///
/// Regression: the watcher invoked its callback per raw inotify event, so a
/// single save reloaded the config and printed its "config reloaded" notice
/// several times in a burst.
///
/// The bound is `< raw events` rather than `== 1` because the exact count is not
/// ours to control: [`CONFIG_DEBOUNCE`](hrdr_app::CONFIG_DEBOUNCE) is the real
/// 100ms production window, and macOS' FSEvents delivers in latency-batched
/// clumps that can straddle it. Coalescing is the invariant; the precise
/// coalesced count is the OS'. The exact-once behaviour of the debouncer itself
/// is pinned by `util::debounce_tests`, which drives it off a channel with no
/// filesystem in the loop.
#[tokio::test]
async fn a_save_burst_collapses_into_far_fewer_reloads() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this binary runs only this test and the one below, which doesn't
    // read the env var.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
    let path = hrdr_agent::config_file_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "model = 'a'\n").unwrap();

    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let _guard = hrdr_app::watch_config(move || {
        h.fetch_add(1, Ordering::SeqCst);
    });
    // Let the OS watcher register before touching the file.
    std::thread::sleep(Duration::from_millis(200));

    const WRITES: usize = 8;
    for i in 0..WRITES {
        std::fs::write(&path, format!("model = 'b{i}'\n")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    // Wait for the burst to land at least one reload — however long the OS takes
    // to deliver the events — then let the debouncer settle any tail before
    // reading the final count.
    let reloaded = wait_until(Duration::from_secs(10), || hits.load(Ordering::SeqCst) >= 1);
    std::thread::sleep(hrdr_app::CONFIG_DEBOUNCE * 4);

    let n = hits.load(Ordering::SeqCst);
    assert!(
        reloaded && n >= 1,
        "the burst reloaded the config at least once"
    );
    assert!(
        n < WRITES,
        "{WRITES} writes inside the debounce window → {n} reloads (not coalesced)"
    );
}

/// Guards the test above from going vacuous: the burst it writes really does
/// produce many filesystem events, so coalescing them to one is doing work.
#[tokio::test]
async fn a_save_burst_really_emits_many_raw_events() {
    use notify::{RecursiveMode, Watcher};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, "a\n").unwrap();

    let raw = Arc::new(AtomicUsize::new(0));
    let r = raw.clone();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            r.fetch_add(1, Ordering::SeqCst);
        }
    })
    .unwrap();
    w.watch(tmp.path(), RecursiveMode::NonRecursive).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    for i in 0..8 {
        std::fs::write(&path, format!("b{i}\n")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    // Wait for the raw events to arrive rather than asserting after a fixed nap:
    // on macOS/Windows or a loaded runner they can be delivered late.
    let bursty = wait_until(Duration::from_secs(10), || raw.load(Ordering::SeqCst) > 1);
    let n = raw.load(Ordering::SeqCst);
    assert!(
        bursty && n > 1,
        "8 writes should emit a burst of raw events, got {n} — \
         the debounce test would prove nothing"
    );
}

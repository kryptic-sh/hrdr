//! Process-level coverage for `hrdr run` — the headless single-turn command —
//! driven against a self-contained mock endpoint (see `common`).
//!
//! The unit tests in `hrdr-agent` already drive `Agent::run` below the process
//! boundary. These run the *built binary* instead, so they exercise everything
//! `main.rs` adds on top: identity settling, config loading, the startup
//! checks, and — the point of the file — the stdout/stderr/exit-code contract
//! that scripts and CI depend on.
//!
//! ## Observed stdout / stderr / exit contract (`hrdr run`)
//!
//! Default (no `--json`, no `--quiet`):
//!   * stdout — the assistant reply text, streamed verbatim (no per-delta
//!     newline), with a single trailing newline at end of turn (`TurnDone`).
//!   * stderr — tool/usage "chrome", all ANSI-colored: `⚙ <tool> <args>` at a
//!     tool start, streamed tool output, `✓/✗ <tool>` at tool end, a
//!     `[usage] ctx … · out …` line, `[notice]` lines, MCP/hook notes, plus any
//!     startup warnings (e.g. a missing API key). Nothing a script parses.
//!   * exit — `0` on a completed turn.
//!
//! `--json`:
//!   * stdout — one JSON object per line, one per `AgentEvent`. Types seen:
//!     `text`, `reasoning`, `tool_start`, `tool_output`, `tool_end`, `history`,
//!     `notice`, `steer`, `todo`, `usage`, `done`. A turn ends with exactly one
//!     `{"type":"done"}` as its final line. `usage` carries `prompt_tokens`,
//!     `completion_tokens`, `cached_prompt_tokens`, `reasoning_tokens`,
//!     `cost_usd`, `session_cost_usd`, `cost_partial`.
//!   * on a turn error — a final `{"type":"error","message":…}` line on stdout,
//!     AND the process exits non-zero (anyhow's `main` prints `Error: …` to
//!     stderr).
//!
//! `--quiet`: stdout is the reply text only; the stderr chrome is suppressed.
//!
//! Exit codes: `0` success; `1` on a turn/agent error (bubbled out of `main`);
//! `2` on a config-compat or startup-check failure (before the turn).

// Its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this the test would run against the
// developer's real `$HOME`. `every_test_binary_is_sandboxed` fails the build for
// a `tests/*.rs` that omits it.
extern crate hrdr_test_support;

mod common;

use std::process::{Command, Output};

use common::{
    Chat, MockServer, stop_chunk, text_chunk, tool_args_chunk, tool_calls_stop_chunk,
    tool_start_chunk, write_config,
};

/// Run `hrdr <args…>` against `server`, in throwaway HOME/XDG/cwd dirs so the
/// developer's real config and sessions are never read or written.
fn run_hrdr(server: &MockServer, args: &[&str]) -> Output {
    run_hrdr_in(server, args, None)
}

/// As [`run_hrdr`], but a caller-provided project dir (for tests that pre-seed a
/// file the model will read). When `None`, a fresh tempdir is used.
fn run_hrdr_in(server: &MockServer, args: &[&str], project: Option<&std::path::Path>) -> Output {
    let home = tempfile::tempdir().expect("temp home");
    write_config(home.path(), &server.base_url());
    let fresh = tempfile::tempdir().expect("temp project");
    let cwd = project.unwrap_or_else(|| fresh.path());

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hrdr"));
    cmd.args(args);
    cmd.current_dir(cwd);
    for (key, value) in [
        ("HOME", home.path()),
        ("USERPROFILE", home.path()),
        ("APPDATA", home.path()),
        ("LOCALAPPDATA", home.path()),
        ("XDG_CONFIG_HOME", home.path()),
        ("XDG_DATA_HOME", home.path()),
        ("XDG_STATE_HOME", home.path()),
    ] {
        cmd.env(key, value);
    }
    // The developer's own model/key must not reach the child; the config.toml is
    // the only identity source.
    for key in ["HRDR_MODEL", "HRDR_API_KEY", "RUST_LOG"] {
        cmd.env_remove(key);
    }
    cmd.output().expect("spawn hrdr")
}

/// A scripted plain-text turn: one text delta, a stop, the DONE sentinel.
fn text_turn(text: &str) -> Chat {
    Chat::Sse(vec![
        text_chunk("c1", text),
        stop_chunk("c1"),
        "[DONE]".to_string(),
    ])
}

// ── 1. plain streamed text turn ──────────────────────────────────────────────

/// `hrdr run "<prompt>"` streams the model's reply to stdout and exits 0.
#[test]
fn run_streams_plain_text_to_stdout() {
    let server = MockServer::start(vec![text_turn("Hello from the mock endpoint.")]);
    let out = run_hrdr(&server, &["run", "say hello"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("Hello from the mock endpoint."),
        "reply text must reach stdout, got: {stdout:?}"
    );
}

// ── 2. tool round trip ───────────────────────────────────────────────────────

/// A turn that calls a tool (`read` of a temp file), gets the result, then
/// answers — the whole round completes and exits 0. The reply text and the
/// read file content are distinct strings, so seeing the reply proves the
/// second model call (post-tool) ran, not just the first.
#[test]
fn run_completes_a_tool_round_trip() {
    let project = tempfile::tempdir().expect("temp project");
    let file = project.path().join("note.txt");
    std::fs::write(&file, "the-secret-content").unwrap();
    let args_json = serde_json::to_string(&serde_json::json!({
        "path": file.to_string_lossy(),
    }))
    .unwrap();

    let server = MockServer::start(vec![
        // Model call 1: ask to read the file.
        Chat::Sse(vec![
            tool_start_chunk("c1", "call_1", "read"),
            tool_args_chunk("c1", &args_json),
            tool_calls_stop_chunk("c1"),
            "[DONE]".to_string(),
        ]),
        // Model call 2: the final answer, after the tool result is fed back.
        text_turn("I read the file successfully."),
    ]);

    let out = run_hrdr_in(&server, &["run", "read the note"], Some(project.path()));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains("I read the file successfully."),
        "the post-tool answer must reach stdout, got: {stdout:?}"
    );
    // The tool chrome lands on stderr (default, not quiet): the `read` ran.
    assert!(
        stderr.contains("read"),
        "the tool-start chrome names the tool on stderr, got: {stderr:?}"
    );
}

// ── 3. NDJSON stream (`--json`) ──────────────────────────────────────────────

/// `--json`: every stdout line is a JSON object, a `usage` event carries the
/// documented numeric fields, and the turn's final line is `{"type":"done"}`.
#[test]
fn run_json_emits_well_formed_ndjson() {
    let server = MockServer::start(vec![text_turn("streamed reply")]);
    let out = run_hrdr(&server, &["run", "--json", "hi"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "some NDJSON must be emitted");

    // EVERY line parses as a JSON object with a string `type`.
    let events: Vec<serde_json::Value> = lines
        .iter()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("line is not JSON: {l:?} ({e})"))
        })
        .collect();
    for ev in &events {
        assert!(
            ev.get("type").and_then(|t| t.as_str()).is_some(),
            "every event has a string `type`: {ev}"
        );
    }

    // The reply text is carried by a `text` event.
    assert!(
        events.iter().any(|e| e["type"] == "text"
            && e["text"]
                .as_str()
                .is_some_and(|t| t.contains("streamed reply"))),
        "a text event carries the reply: {events:?}"
    );

    // A `usage` event carries the documented fields (counts are numbers; the
    // optional token fields may be null but must be present).
    let usage = events
        .iter()
        .find(|e| e["type"] == "usage")
        .expect("a usage event is emitted");
    assert!(usage["prompt_tokens"].is_number(), "usage: {usage}");
    assert!(usage["completion_tokens"].is_number(), "usage: {usage}");
    for field in [
        "cached_prompt_tokens",
        "reasoning_tokens",
        "cost_usd",
        "session_cost_usd",
        "cost_partial",
    ] {
        assert!(usage.get(field).is_some(), "usage missing {field}: {usage}");
    }

    // The turn ends with exactly one `done`, and it is the final line.
    assert_eq!(
        events.last().map(|e| e["type"].clone()),
        Some(serde_json::json!("done")),
        "the last event is `done`: {events:?}"
    );
    assert_eq!(
        events.iter().filter(|e| e["type"] == "done").count(),
        1,
        "exactly one `done`"
    );
}

/// `--json` on a turn error: a `{"type":"error",…}` line on stdout, non-zero
/// exit. The mock returns HTTP 400 (a terminal, non-retryable status), so the
/// turn fails deterministically without burning the retry budget.
#[test]
fn run_json_reports_errors_as_a_json_event_and_exits_nonzero() {
    let server = MockServer::start(vec![Chat::Status(400)]);
    let out = run_hrdr(&server, &["run", "--json", "hi"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "a failed turn must exit non-zero; stdout: {stdout}"
    );
    let err = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("an error event must be on stdout, got: {stdout:?}"));
    assert!(
        err["message"].as_str().is_some_and(|m| !m.is_empty()),
        "the error event carries a message: {err}"
    );
}

// ── 4. network failure ───────────────────────────────────────────────────────

/// A server that accepts the request then drops the connection mid-response is
/// a network failure: the run exits non-zero and puts a diagnostic on stderr
/// (there is no `--json`, so nothing structured is expected on stdout).
///
/// A dropped connection is classified transient, so the turn loop retries with
/// bounded backoff (~8s worst case) before giving up — hence the diagnostic is
/// asserted loosely and the test carries no tight timing.
#[test]
fn run_reports_a_network_failure_on_stderr_and_exits_nonzero() {
    // Enough Drops to outlast the retry budget (MAX_RETRIES = 4).
    let server = MockServer::start(vec![
        Chat::Drop,
        Chat::Drop,
        Chat::Drop,
        Chat::Drop,
        Chat::Drop,
        Chat::Drop,
    ]);
    let out = run_hrdr(&server, &["run", "hi"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a network failure must exit non-zero.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.trim().is_empty(),
        "a diagnostic must land on stderr, got empty stderr (stdout: {stdout:?})"
    );
}

// ── 5. max-cost enforcement ──────────────────────────────────────────────────

/// `--max-cost 0` trips the budget before any model call: the cap-exhausted
/// check is model-agnostic (`spent 0 ≥ cap 0`), so the run stops with a budget
/// error and a non-zero exit, and the endpoint is never hit.
#[test]
fn run_max_cost_zero_stops_before_any_model_call() {
    // The queue is a poisoned pill: any request pops a 500, so if the run were
    // to reach the endpoint the failure mode would differ from a budget stop.
    let server = MockServer::start(vec![Chat::Status(500)]);
    let out = run_hrdr(&server, &["run", "--max-cost", "0", "hi"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a tripped cost budget must exit non-zero.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("budget") || stderr.to_lowercase().contains("cost"),
        "stderr names the budget as the reason, got: {stderr:?}"
    );
}

/// A negative/invalid `--max-cost` is rejected up front (before any turn).
#[test]
fn run_rejects_a_negative_max_cost() {
    let server = MockServer::start(vec![text_turn("unused")]);
    let out = run_hrdr(&server, &["run", "--max-cost", "-1", "hi"]);
    assert!(
        !out.status.success(),
        "a negative cap is a usage error: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

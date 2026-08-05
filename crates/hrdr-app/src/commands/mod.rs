//! Shared slash-command layer. The command *implementations* live here, behind
//! the [`CommandHost`] trait, so every frontend drives the exact same logic and
//! gains new commands for free — a frontend just implements the host
//! capabilities (emit a line, access the agent, clipboard, sessions, …) and
//! calls [`dispatch`]. Frontend-coupled commands (scrolling, find/goto, expand,
//! theme/timestamps, editor) stay in the frontends and are handled before
//! delegating here.
//!
//! Async work (network, subprocess, filesystem, agent lock) is expressed as a
//! [`LineFuture`] the host spawns; its returned string (if non-empty) is shown
//! as a system line. This keeps the layer uniform across a sync-polled frontend
//! (the TUI) and an async-locked one, which both hold the agent as
//! `Arc<tokio::sync::Mutex>`.

mod compaction;
mod conversation;
mod dispatch;
mod helpers;
mod host;
mod model;
mod types;

pub use compaction::*;
pub use conversation::*;
pub use dispatch::*;
pub use helpers::*;
pub use host::*;
pub use model::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_agent::Message;

    #[test]
    fn unreachable_guidance_shows_setup_paths() {
        let msg = unreachable_guidance("http://localhost:8080/v1", "connection refused");
        // Names the failed endpoint + the underlying error…
        assert!(msg.contains("http://localhost:8080/v1"));
        assert!(msg.contains("connection refused"));
        // …then points at both a local server and the /login wizard.
        assert!(msg.contains("infr serve"));
        assert!(msg.contains("llama-server"));
        assert!(msg.contains("/login"));
    }

    #[test]
    fn auto_compact_threshold_and_messages() {
        // Reserved 200 → fires at window − reserved = 800. Below/at/over,
        // disabled, and missing inputs.
        assert!(should_auto_compact(Some(800), Some(1000), 200, true));
        assert!(should_auto_compact(Some(950), Some(1000), 200, true));
        assert!(!should_auto_compact(Some(799), Some(1000), 200, true));
        assert!(!should_auto_compact(Some(999), Some(1000), 200, false)); // disabled
        assert!(!should_auto_compact(None, Some(1000), 200, true));
        assert!(!should_auto_compact(Some(999), None, 200, true));
        // Reserved larger than a quarter-window is clamped, so the trigger
        // never collapses to 0: with window 1000 it fires at 75% (750), not 0.
        assert!(should_auto_compact(Some(750), Some(1000), 5000, true));
        assert!(!should_auto_compact(Some(749), Some(1000), 5000, true));
        // Message formatting covers the three outcomes.
        let report = |before, after| hrdr_agent::CompactionReport {
            reason: hrdr_agent::CompactionReason::UserRequested,
            before,
            after,
            context_after: 0,
            prompt_tokens: 1_000,
            cached_prompt_tokens: Some(900),
            output_tokens: 42,
            cost_usd: None,
            stage: hrdr_agent::ShrinkStage::Full,
            attempts: 1,
        };
        assert_eq!(
            compaction_message(&Ok(report(2, 2))),
            "nothing to compact yet"
        );
        let line = compaction_message(&Ok(report(10, 2)));
        assert!(
            line.contains("compacted on request 10 → 2 messages"),
            "{line}"
        );
        // The cache-read fraction is the number that shows the compaction
        // request still matched the session's cached prefix.
        assert!(line.contains("90% from cache"), "{line}");
        // The message is multi-line: the summary-call figures and the suffix
        // each sit on their own line rather than one wrapped string.
        assert!(
            line.ends_with("\n(summary kept; scrollback above is preserved for you)"),
            "{line}"
        );
        assert!(
            line.contains("\nsummary call: 1000 prompt tokens, 90% from cache, 42 output"),
            "{line}"
        );
        assert!(compaction_message(&Err("boom".into())).contains("[compact failed] boom"));
    }

    #[test]
    fn conversation_export_covers_only_user_assistant() {
        let msgs = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::assistant(""), // empty assistant (tool-call turn) skipped
        ];
        let md = conversation_to_markdown(&msgs);
        assert!(md.contains("## User\nhello"));
        assert!(md.contains("## Assistant\nhi there"));
        assert!(!md.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&conversation_to_json(&msgs)).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
    }
}

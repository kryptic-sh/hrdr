//! The clock on an agent's turn — *per agent*, not per session.
//!
//! How long the model has been working, when its first token arrived, how many it
//! has produced, whether it is inferring or waiting on a tool: these describe **a
//! turn on an agent**. Every agent has turns, so every agent has these.
//!
//! They used to live in the frontend, which meant they existed only for the main
//! agent — so a sub-agent's view borrowed the main agent's clock. Watching a
//! sub-agent work showed you the *main* agent's spinner, throughput and elapsed
//! time, and a sub-agent grinding away under an idle main agent showed no loader
//! at all.
//!
//! Kept beside the event record it is derived from: [`crate::LiveAgents::record`]
//! feeds every event through [`TurnStats::record`], so the figures are right with
//! no UI attached.

use std::time::{Duration, Instant, SystemTime};

use crate::AgentEvent;

/// The live state of one agent's turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnStats {
    /// When the turn began (monotonic — for durations).
    pub started: Option<Instant>,
    /// When the turn began (wall clock — for "started 2m ago").
    pub started_at: Option<SystemTime>,
    /// When the first token arrived — the provider's time-to-first-token.
    pub first_token_at: Option<Instant>,
    /// Streamed deltas this turn (text + reasoning), for throughput.
    pub out_tokens: usize,
    /// Tool calls in flight this round. While any is running the model is idle.
    pub tools_running: usize,
    /// Model working time banked from earlier stretches of this turn.
    infer_banked: Duration,
    /// Start of the stretch the model is working *now*, if it is.
    infer_started: Option<Instant>,
    /// Prompt tokens served from cache on the latest call, when reported.
    pub last_cached_tokens: Option<u32>,
    /// Completion tokens spent on reasoning on the latest call, when reported.
    pub last_reasoning_tokens: Option<u32>,
}

impl TurnStats {
    /// A turn is starting: reset the clock and put the model to work.
    pub fn begin(&mut self) {
        let now = Instant::now();
        *self = Self {
            started: Some(now),
            started_at: Some(SystemTime::now()),
            infer_started: Some(now),
            ..Default::default()
        };
    }

    /// Fold one of this agent's events into the clock.
    pub fn record(&mut self, ev: &AgentEvent) {
        match ev {
            // A streamed delta: the first one is time-to-first-token.
            AgentEvent::Text(_) | AgentEvent::Reasoning(_) => {
                self.first_token_at.get_or_insert_with(Instant::now);
                self.out_tokens += 1;
            }
            // The model has handed off: it is idle until every tool of this round
            // returns, so its clock stops rather than inflating the turn with time
            // it spent waiting.
            AgentEvent::ToolStart { .. } => {
                self.tools_running += 1;
                if self.tools_running == 1 {
                    self.pause();
                }
            }
            // The last tool of the round returned: it is about to ask the model
            // again, so it is working from here.
            AgentEvent::ToolEnd { .. } => {
                self.tools_running = self.tools_running.saturating_sub(1);
                if self.tools_running == 0 {
                    self.resume();
                }
            }
            AgentEvent::Usage {
                cached_prompt_tokens,
                reasoning_tokens,
                ..
            } => {
                self.last_cached_tokens = *cached_prompt_tokens;
                self.last_reasoning_tokens = *reasoning_tokens;
            }
            AgentEvent::TurnDone => self.end(),
            _ => {}
        }
    }

    /// The turn is over: stop the clock.
    pub fn end(&mut self) {
        self.pause();
        self.tools_running = 0;
    }

    fn pause(&mut self) {
        if let Some(t) = self.infer_started.take() {
            self.infer_banked += t.elapsed();
        }
    }

    fn resume(&mut self) {
        self.infer_started.get_or_insert_with(Instant::now);
    }

    /// Whether the model is working *right now* — as opposed to waiting on a tool
    /// call, or not being in a turn at all. This is what a loader shows.
    pub fn inferring(&self) -> bool {
        self.infer_started.is_some()
    }

    /// How long the model has actually worked this turn: the banked stretches plus
    /// the one in progress. Excludes time spent waiting on tool calls.
    pub fn infer_elapsed(&self) -> Duration {
        self.infer_banked
            + self
                .infer_started
                .map(|t| t.elapsed())
                .unwrap_or(Duration::ZERO)
    }

    /// Time-to-first-token, in seconds.
    pub fn ttft(&self) -> Option<f64> {
        let (start, first) = (self.started?, self.first_token_at?);
        Some(first.duration_since(start).as_secs_f64())
    }

    /// Streamed tokens per second *of model working time* — not of wall clock, so
    /// a long tool call doesn't read as a slow model.
    pub fn tok_per_sec(&self) -> f64 {
        let secs = self.infer_elapsed().as_secs_f64();
        match self.out_tokens {
            0 => 0.0,
            n if secs > 0.0 => n as f64 / secs,
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_start() -> AgentEvent {
        AgentEvent::ToolStart {
            id: "c1".into(),
            name: "bash".into(),
            args: "{}".into(),
        }
    }

    fn tool_end() -> AgentEvent {
        AgentEvent::ToolEnd {
            id: "c1".into(),
            name: "bash".into(),
            result: String::new(),
            ok: true,
        }
    }

    /// The clock stops while the model waits on a tool call — otherwise a turn that
    /// spent a minute in `bash` reads as a minute of slow inference.
    #[test]
    fn the_model_clock_stops_while_a_tool_runs() {
        let mut t = TurnStats::default();
        t.begin();
        assert!(t.inferring(), "a turn opens with the model working");

        t.record(&tool_start());
        assert!(!t.inferring(), "it is idle while a tool runs");
        let banked = t.infer_elapsed();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(
            t.infer_elapsed(),
            banked,
            "and the clock does not advance while it waits"
        );

        t.record(&tool_end());
        assert!(t.inferring(), "it is working again once the round returns");
    }

    /// A round with several tools is one idle stretch, not one per tool.
    #[test]
    fn a_round_of_several_tools_is_one_idle_stretch() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&tool_start());
        t.record(&tool_start());
        t.record(&tool_end());
        assert!(!t.inferring(), "still waiting on the second tool");
        t.record(&tool_end());
        assert!(t.inferring());
    }

    #[test]
    fn deltas_count_toward_throughput_and_the_first_one_is_ttft() {
        let mut t = TurnStats::default();
        t.begin();
        assert_eq!(t.ttft(), None, "nothing has streamed yet");
        t.record(&AgentEvent::Text("a".into()));
        t.record(&AgentEvent::Reasoning("b".into()));
        assert_eq!(t.out_tokens, 2, "reasoning is output too");
        assert!(t.ttft().is_some(), "the first delta is time-to-first-token");
        assert!(t.tok_per_sec() > 0.0);
    }

    #[test]
    fn a_finished_turn_stops_the_clock() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&AgentEvent::TurnDone);
        assert!(!t.inferring(), "a finished turn is not still generating");
        let elapsed = t.infer_elapsed();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(t.infer_elapsed(), elapsed, "and its clock is frozen");
    }
}

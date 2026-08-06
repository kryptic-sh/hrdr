//! Turn execution, tool dispatch, streaming, retries, and cleanup.

use super::*;

/// With this many tool rounds left in a turn, the model is told to wrap up
/// (appended to the last tool result of that round).
const WRAP_UP_WARNING_ROUNDS: usize = 3;

/// Fraction of the tool-round budget after which the model gets one *soft*
/// warning — numerator over [`CHECKPOINT_WARNING_DEN`].
///
/// [`WRAP_UP_WARNING_ROUNDS`] is far too late to act on: three rounds is enough
/// to write a summary, not to commit half-finished work and re-plan the rest.
/// Transcripts show the hard stop landing mid-plan with no warning at all — an
/// autonomous run died roughly a quarter of the way through its list, leaving
/// uncommitted edits and nothing sequenced. At 80% there is still real budget
/// left to checkpoint with.
const CHECKPOINT_WARNING_NUM: usize = 4;
const CHECKPOINT_WARNING_DEN: usize = 5;

/// The round after which the soft checkpoint warning fires, or `None` when the
/// budget is too small for it to be worth anything (the 80% mark lands on the
/// last round, where [`WRAP_UP_WARNING_ROUNDS`] already speaks).
pub(crate) fn checkpoint_warning_round(max_steps: usize) -> Option<usize> {
    let at = (max_steps * CHECKPOINT_WARNING_NUM).div_ceil(CHECKPOINT_WARNING_DEN);
    (at >= 1 && at < max_steps).then_some(at)
}

/// Capacity of the per-tool and shared live-output channels used to forward
/// [`ToolContext::stream`](hrdr_tools::ToolContext::stream) chunks to the UI
/// (see `run_tool_batch`). This is advisory progress output, not the
/// authoritative tool result, so both channels are bounded rather than
/// unbounded: a tool that emits output faster than the UI drains it (e.g. a
/// shell command printing millions of lines) must never queue without limit.
/// The cap is generous for a normal burst — far more than a screen's worth
/// — while keeping the per-in-flight-tool buffer small and fixed; anything past
/// the cap is dropped (`try_send` returns `Full`), never queued or blocked on.
///
/// This bounds the two channels this pipeline owns (`ctx.stream` and the shared
/// forwarder), which fully defeats a synchronous emit tight-loop. The frontend's
/// own `AgentEvent` queue downstream is a separate, still-unbounded hop, so a
/// *streaming* flood can still grow memory there under a lagging renderer — a
/// known follow-up (bound/coalesce that queue), not covered here.
const UI_STREAM_CAP: usize = 1024;

/// Consecutive identical failures after which the exact same call is refused
/// without executing (small models loop on verbatim retries).
const REPEAT_REFUSE_AFTER: u32 = 2;

/// Consecutive identical calls — **whatever their outcome** — after which the
/// model is told it is repeating itself (see [`RepeatGuard`]).
///
/// Three, not two: the second identical call is often a real check rather than a
/// loop (re-`read` a file after editing it, re-run the test that just failed).
/// The third is where the pattern stops being a check — nothing between call two
/// and call three changed the world, so call three cannot learn anything call two
/// didn't. Opencode picked the same number independently (`session/processor.ts`
/// fires on the last 3 tool parts being identical).
const REPEAT_NUDGE_AFTER: u32 = 3;

/// Byte budget for per-turn **relevance recall** injected alongside the opening
/// user message (the full text of the most relevant memories). It stays small
/// next to the always-loaded pointer index (~25 KB) while giving the model room
/// for a few complete facts; recall truncates/drops entries to never exceed it.
const MEMORY_RECALL_BUDGET: usize = 4 * 1024;

/// Anti-loop breaker: tracks the last call (tool + raw args), how many times
/// that *exact same* call has been made in a row, and how many of those in a row
/// failed. Any intervening different call resets both counters, so a legitimate
/// `test → edit → test` cycle never trips anything.
///
/// Two failure modes, two answers:
///
/// - **Failing** verbatim retries are refused outright after
///   [`REPEAT_REFUSE_AFTER`] — the call has nothing to offer and small models
///   will retry it forever.
/// - **Succeeding** identical calls are the quieter wedge: `read` the same file,
///   `grep` the same pattern, re-run a `cargo test` that exits 0. Nothing errors,
///   so nothing notices, and the round budget and cost cap drain at full speed.
///   Those get a nudge on the result after [`REPEAT_NUDGE_AFTER`] and nothing
///   more — refusing a call that works would break real work, and hrdr is
///   autonomous, so there is nobody to ask.
#[derive(Default)]
pub(crate) struct RepeatGuard {
    key: Option<u64>,
    /// Consecutive calls with `key`, successes included.
    calls: u32,
    /// Consecutive *failures* with `key`; a success zeroes it but keeps `key`.
    failures: u32,
}

fn call_key(name: &str, raw_args: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    raw_args.hash(&mut h);
    h.finish()
}

impl RepeatGuard {
    /// The refusal message when this call must not run again (it already
    /// failed [`REPEAT_REFUSE_AFTER`]+ times in a row), else `None`.
    pub(crate) fn refusal(&self, name: &str, raw_args: &str) -> Option<String> {
        (self.key == Some(call_key(name, raw_args)) && self.failures >= REPEAT_REFUSE_AFTER).then(
            || {
                format!(
                    "refused without running: this exact {name} call already failed {} \
                     times in a row — change the arguments or the approach; if you're \
                     stuck, stop and tell the user what you tried",
                    self.failures
                )
            },
        )
    }

    /// Record a call's outcome; returns the nudge to append to the result the
    /// model sees, when this call is a repeat worth telling it about.
    ///
    /// `repeatable` is the tool's own opt-out (see
    /// [`Tool::repeatable`](hrdr_tools::Tool::repeatable)): a poll answering the
    /// same question until the answer changes is *supposed* to be identical, so
    /// it never earns the success nudge. It still earns the failure nudge —
    /// polling that keeps erroring is a loop whatever the tool.
    pub(crate) fn record(
        &mut self,
        name: &str,
        raw_args: &str,
        ok: bool,
        repeatable: bool,
    ) -> Option<String> {
        let k = call_key(name, raw_args);
        if self.key != Some(k) {
            self.key = Some(k);
            self.calls = 1;
            self.failures = u32::from(!ok);
            return None;
        }
        self.calls += 1;
        if !ok {
            self.failures += 1;
            // Gated on the streak length rather than "this is a repeat" so a
            // success in the middle of the streak still costs a full pair of
            // failures before the nudge returns — the same point the refusal
            // arms at.
            return (self.failures >= REPEAT_REFUSE_AFTER).then(|| {
                format!(
                    "\n[note: this exact call has failed {} times in a row — change the input \
                     or approach instead of retrying it verbatim]",
                    self.failures
                )
            });
        }
        self.failures = 0;
        (!repeatable && self.calls >= REPEAT_NUDGE_AFTER).then(|| {
            format!(
                "\n[note: you have now made this exact {name} call {} times in a row. It \
                 succeeded and nothing changed in between, so making it again cannot tell you \
                 anything new — change approach, or stop and report what you have]",
                self.calls
            )
        })
    }
}

/// Render a tool's error for the model: the full `anyhow` context chain, not
/// just the outermost frame.
///
/// `{e}` prints only the last `.context(...)`, which is the summary a *human*
/// wants and the opposite of what the model needs — "invalid edit args" without
/// "missing field `old_string`" gives it nothing to correct. `{e:#}` appends
/// each source, `outer: inner: root`.
pub(crate) fn tool_error_text(e: &anyhow::Error) -> String {
    format!("Error: {e:#}")
}

/// Error classification lives in hrdr-llm, next to the [`hrdr_llm::ChatError`]
/// it reads — the agent decides what to *do* about a failure ("compact and
/// retry", "give up on this round"), not what the failure *is*. `is_transient`
/// and `retry_after_hint` moved with it and are now read only by
/// [`hrdr_llm::RetryBudget`]; overflow is the one classification the agent still
/// acts on itself, so it is re-exported here where its call sites already
/// spell it `crate::is_context_overflow`.
pub(crate) use hrdr_llm::is_context_overflow;

/// Drain a chat stream into an [`Accumulator`], emitting `Reasoning` and `Text`
/// deltas as they arrive. Shared by the turn loop, the budget-exhausted wrap-up
/// round, and (with a no-op sink) the one-off compaction call.
///
/// Also times the round's generation window — see [`Drained::decode`].
pub(crate) async fn drain_stream<F: FnMut(AgentEvent)>(
    stream: &mut ChatStream,
    on_event: &mut F,
) -> Result<Drained> {
    let mut acc = Accumulator::new();
    // First chunk that carried a payload of *any* kind. Not `first_token_at`
    // from the events: a round whose whole output is one tool call streams only
    // `input_json_delta`s, which are accumulated silently and would leave this
    // round timed at zero.
    let mut first_payload: Option<std::time::Instant> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if first_payload.is_none() && chunk_has_payload(&chunk) {
            first_payload = Some(std::time::Instant::now());
        }
        // Empty deltas are dropped rather than forwarded, in BOTH directions.
        // Servers do send them: a Qwen3-style backend keeps emitting
        // `reasoning_content: ""` on every content chunk once it stops thinking
        // (and `content: ""` while it is still thinking), where other providers
        // omit the field entirely. Either one, forwarded, silently shreds the
        // transcript — the frontend only merges a delta into the previous entry
        // when that entry is the matching kind, so an empty event of the *other*
        // kind lands in between and forces a new block per chunk. An empty
        // `Text` also closes the open reasoning block, fragmenting reasoning the
        // same way. Both render as nothing, so the only visible symptom is one
        // `#N assistant` header per token group.
        //
        // `acc.push` is still called for every chunk — it accumulates content
        // and tool-call fragments; only the *event* is suppressed. Its error
        // (the accumulated reply past the byte budget) ends the stream exactly
        // like the SSE-overflow error would.
        if let Some(choice) = chunk.choices.first()
            && let Some(r) = &choice.delta.reasoning_content
            && !r.is_empty()
        {
            on_event(AgentEvent::Reasoning(r.clone()));
        }
        let delta = acc.push(&chunk)?;
        if let Some(text) = delta
            && !text.is_empty()
        {
            on_event(AgentEvent::Text(text));
        }
    }
    Ok(Drained {
        decode: first_payload
            .map(|t| t.elapsed())
            .unwrap_or(std::time::Duration::ZERO),
        acc,
    })
}

/// One drained round: what the model produced, and how long it took to produce
/// it.
pub(crate) struct Drained {
    pub acc: Accumulator,
    /// From the round's first streamed payload byte to the end of its stream —
    /// generation time, with the prefill wait ahead of it excluded. Zero for a
    /// round that streamed nothing at all.
    pub decode: std::time::Duration,
}

/// Which view of `self.messages` a round sends. It is a *view*, rebuilt per
/// connection attempt, rather than a swap of the field: overflow recovery
/// compacts `self.messages`, and it has to compact the canonical history — the
/// one with the tool protocol intact — or the retry would flatten a history
/// that had already lost its tool_use/tool_result pairing.
#[derive(Clone, Copy)]
enum RequestHistory {
    /// Send `self.messages` as-is.
    Canonical,
    /// Send [`flatten_tool_protocol`]'s output: the forced wrap-up round carries
    /// no `tools` definition, and backends 400 on tool blocks without one.
    ProtocolFree,
}

/// Whether a chunk carried any of the model's output, as opposed to being the
/// trailing usage-only chunk (or a keep-alive, or one of the empty deltas a
/// Qwen3-style backend sprays). Tool-call fragments count: they are output the
/// model had to generate, and for many rounds they are all of it.
fn chunk_has_payload(chunk: &hrdr_llm::ChatChunk) -> bool {
    chunk.choices.first().is_some_and(|c| {
        c.delta
            .content
            .as_ref()
            .is_some_and(|text| !text.is_empty())
            || c.delta
                .reasoning_content
                .as_ref()
                .is_some_and(|r| !r.is_empty())
            || c.delta.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
    })
}

/// Repair a history left dangling by an interrupted turn. An assistant message
/// with `tool_calls` must be followed by a `role:"tool"` result for every call
/// id, or strict servers (OpenAI, and infr) reject the next request. Any
/// tool-calling assistant message missing results (the turn was cancelled
/// mid tool-call) gets a stub result appended for each unanswered id, inserted
/// right after that turn's existing results so ordering stays correct.
///
/// Scans the **whole** history, not just the most recent tool-calling turn: a
/// resumed or hand-edited session can carry an older dangling turn buried
/// earlier in the messages (e.g. two interrupted turns before a save), and
/// leaving it unrepaired would keep the session permanently invalid even after
/// the newest turn is fixed.
/// What hrdr says when its endpoint hands back a tool call as prose.
///
/// The failure it describes is silent by construction. An OpenAI-compatible
/// server that has not been told how to *parse* its model's tool-call syntax
/// still answers 200 with a perfectly well-formed completion — the tool call is
/// just sitting in `content` as text, and `tool_calls` is empty. vLLM does
/// exactly this when started without `--enable-auto-tool-choice
/// --tool-call-parser <parser>`: it returns the raw text rather than erroring,
/// so nothing in the response says anything is wrong. hrdr sees a model that
/// narrates tool use and never calls a tool, forever, with no error to retry.
pub(crate) const UNPARSED_TOOL_CALL_NOTICE: &str = "⚠ this endpoint returned a tool call as plain text — it looks like the server isn't \
     parsing tool calls. vLLM needs `--enable-auto-tool-choice --tool-call-parser <parser>`; \
     llama.cpp needs `--jinja` (and a template that supports tools).";

/// Whether `text` looks like a tool call the server failed to parse.
///
/// Keyed on the wrappers the chat templates emit, not on JSON: a model
/// *discussing* a tool call writes its name and arguments, but it does not
/// reproduce the template's control markers. Deliberately narrow — this drives a
/// warning about the operator's setup, and crying wolf about it is worse than
/// staying quiet.
///
/// Host is not part of the test. The symptom only occurs on a self-hosted
/// server (a hosted API parses its own models' output), and "self-hosted" is not
/// something an endpoint's hostname tells you — a vLLM box behind private DNS on
/// another machine is the common case, not the exception.
pub(crate) fn looks_like_unparsed_tool_call(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "<tool_call>",           // Qwen, Hermes, many others
        "<|tool_call",           // Granite / assorted pipe-delimited templates
        "<|python_tag|>",        // Llama 3.x
        "<function=",            // Llama 3.1 built-in / functionary
        "<tool▁call▁begin｜>",   // DeepSeek (its own unicode delimiters)
        "<|tool▁calls▁begin|>",  // DeepSeek R1
        "[TOOL_CALLS]",          // Mistral
        "<|channel|>commentary", // gpt-oss / harmony
        "functools[",            // assorted fine-tunes
    ];
    MARKERS.iter().any(|m| text.contains(m))
}

pub(crate) fn repair_dangling_tool_calls(messages: &mut Vec<ChatMessage>) {
    let mut idx = 0;
    while idx < messages.len() {
        if messages[idx].role != Role::Assistant || messages[idx].tool_calls.is_none() {
            idx += 1;
            continue;
        }
        let call_ids: Vec<String> = messages[idx]
            .tool_calls
            .as_ref()
            .map(|calls| calls.iter().map(|c| c.id.clone()).collect())
            .unwrap_or_default();
        // This turn's own results are the contiguous run of `role:"tool"`
        // messages immediately following it — the next non-tool message starts
        // a different turn, so it can't answer this one's calls.
        let mut end = idx + 1;
        while end < messages.len() && messages[end].role == Role::Tool {
            end += 1;
        }
        let answered: std::collections::HashSet<&str> = messages[idx + 1..end]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let missing: Vec<String> = call_ids
            .into_iter()
            .filter(|id| !answered.contains(id.as_str()))
            .collect();
        let inserted = missing.len();
        for (offset, id) in missing.into_iter().enumerate() {
            messages.insert(end + offset, ChatMessage::tool_result(id, "[interrupted]"));
        }
        idx = end + inserted;
    }
}

/// Render the not-yet-`completed`/`cancelled` TODO items as `[ ] content` / `[~] content`
/// lines, one per item — mirrors the checkbox rendering `todo`'s own tool
/// produces (see `render_todos` in `hrdr-tools::tools::todo`), minus the
/// completed/cancelled items, since those are exactly what a turn-end nudge needs to
/// call out.
pub(crate) fn render_unfinished_todos(todos: &[TodoItem]) -> String {
    todos
        .iter()
        .filter(|t| !matches!(t.status.as_str(), "completed" | "cancelled"))
        .map(|t| {
            let mark = if t.status.as_str() == "in_progress" {
                "~"
            } else {
                " "
            };
            format!("[{mark}] {}", t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Guard against an assistant turn carrying neither text nor a tool call.
///
/// `Accumulator::into_message` leaves both `content` and `tool_calls` unset
/// when the model's reply was genuinely empty (e.g. a `stop` with no delta
/// and no tool call), which serializes as a bare `{"role":"assistant"}` on
/// the wire. Some strict OpenAI-compatible servers 400 on *any* request whose
/// history contains one of those, wedging every later request in the
/// session. A short placeholder keeps the message round-trippable; nothing
/// else about it (in particular, no `tool_calls`) changes, so no
/// tool-call/result pairing invariant is affected.
pub(crate) fn ensure_assistant_has_content(msg: &mut ChatMessage) {
    let empty_text = msg
        .content
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty();
    if empty_text && msg.tool_calls.is_none() {
        msg.content = Some("(no response)".to_string());
    }
}

/// Human-readable elapsed time, magnitude-relative: the two largest adjacent
/// units — hours+minutes, minutes+seconds, or seconds+milliseconds — or just
/// milliseconds under one second. Examples: `53ms`, `5s 12ms`, `1m 31s`,
/// `1h 32m`. The coarse unit gives the magnitude; the finer one keeps
/// precision without a wall of units.
pub(crate) fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms >= 3_600_000 {
        format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    } else if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else if ms >= 1_000 {
        format!("{}s {}ms", ms / 1_000, ms % 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// State the turn-end TODO nudge arms so the rounds after it can tell
/// *reconciliation* from *deletion*.
///
/// Transcripts show the failure this catches: nudged about four unfinished
/// items, the model called `todo` once with a single `completed` item — "all
/// done" — and the bookkeeping was square by erasure. The todo tool replaces the
/// whole list, so content strings are the only identity items have; a shrinking
/// list plus a named item gone missing is the honest signal, and it is checked
/// only while a nudge is outstanding.
struct TodoShrinkWatch {
    /// Contents of the items the nudge named as unfinished.
    unfinished: Vec<String>,
    /// Length of the whole list when the nudge was sent.
    len: usize,
}

/// One tool call's measured outcome: its wall-clock duration and result.
type TimedResult = (std::time::Duration, Result<String>);

/// Aborts every in-flight tool task when the batch scope unwinds.
///
/// Each tool call in a batch runs as its own spawned task (see
/// `run_tool_batch`), and dropping a [`tokio::task::JoinHandle`] merely
/// detaches the task — the tool would keep running to its own timeout. This
/// guard is the explicit abort: on a cancelled turn (Esc-Esc aborts the turn
/// task, dropping `join_all`) or a panicking sibling (`resume_unwind` in
/// `run_tool_batch`), the guard's `Drop` aborts every handle still in flight,
/// the tool futures drop, and a shell tool's `kill_on_drop` kills the child.
/// On the normal completion path every handle has already finished, and
/// aborting a completed handle is a no-op.
struct ToolBatchGuard {
    handles: Vec<tokio::task::JoinHandle<TimedResult>>,
}

impl Drop for ToolBatchGuard {
    fn drop(&mut self) {
        for h in &mut self.handles {
            h.abort();
        }
    }
}

impl Agent {
    /// The current TODO list's item contents and length, for [`TodoShrinkWatch`].
    fn todo_snapshot(&self) -> (Vec<String>, usize) {
        let items = self
            .ctx
            .todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            items.iter().map(|t| t.content.clone()).collect(),
            items.len(),
        )
    }

    /// Run one user turn to completion, emitting events as it goes. `steering` is
    /// a shared queue the caller can push to mid-turn (see [`SteeringQueue`]);
    /// pass [`steering_queue()`] when there's no interactive steering.
    pub async fn run<F>(&mut self, steering: SteeringQueue, mut on_event: F) -> Result<()>
    where
        F: FnMut(AgentEvent),
    {
        // Anything raised when there was no turn to carry it — the model pre-flight,
        // at construction or on a switch — is said before the turn it might explain.
        for notice in self.take_pending_notices() {
            on_event(AgentEvent::Notice(notice));
        }
        // A previous turn interrupted mid tool-call can leave the history ending
        // with an assistant `tool_calls` message whose results are missing —
        // strict servers reject that. Backfill stubs before the new user turn.
        repair_dangling_tool_calls(Arc::make_mut(&mut self.messages));
        // Drain the turn opener from the queue — the same queue a mid-turn steer
        // lands on. A normal turn has one waiting (the caller enqueued it); an
        // opener-less turn — nothing queued — exists only to hand the agent
        // something already in its history (a `!command`'s output, a landed
        // background result), so it skips delivery and proceeds straight to the
        // loop.
        let opening = steering
            .lock()
            .map(|mut q| q.pop_front())
            .unwrap_or_default();
        if let Some(opening) = opening {
            self.deliver_user_message(opening, /*opening*/ true, &mut on_event)
                .await?;
        }
        let defs = self.tools.defs();
        // The tool surface is part of the prompt on every round, so the no-usage
        // token fallback has to count it. Estimated once here, alongside the defs
        // it measures: the registry is fixed for the turn (a `/reload` builds the
        // next turn's defs afresh), and re-serializing every schema each round
        // would be pure waste.
        let tool_tokens = estimate_tokens_in_tools(&defs);
        // Allow one automatic compaction per turn when the context overflows.
        let mut overflow_compacted = false;
        // Anti-loop breaker for verbatim retries — failing (refused) or
        // succeeding-but-going-nowhere (nudged).
        let mut repeat = RepeatGuard::default();
        // At most one turn-end nudge (see below) per turn — a genuinely blocked
        // or deferring model must still be able to stop.
        let mut nudged_this_turn = false;
        // One soft checkpoint warning per turn at 80% of the round budget.
        let mut checkpoint_warned = false;
        // Armed by the turn-end nudge: the unfinished items it named, plus the
        // list length at that moment. The rounds that follow the nudge are
        // watched for the list *shrinking* — see `TodoShrinkWatch`.
        let mut todo_watch: Option<TodoShrinkWatch> = None;

        let mut step = 0usize;
        while step < self.max_steps {
            // Deliver any steering messages submitted since the last request — a
            // mid-turn correction reaches the model after the current tool round.
            // A steer is the user piling on more work: reset the round budget so
            // the model gets a fresh `max_steps` of tool rounds to take it on,
            // instead of running out mid-way. `step` counts 1-based from here,
            // so the round that follows a reset reads as round 1 of the fresh
            // budget.
            if self.drain_steering(&steering, &mut on_event).await {
                step = 0;
            }
            step += 1;
            // Fold in any detached background sub-agent results that have landed.
            self.drain_background(&mut on_event);
            // Compact before the next request if this agent manages its own
            // context and is close to filling it (a small local model reading a
            // lot of files gets there fast). The only answer to a filling context:
            // tool-output pruning used to get first shot at it and was removed,
            // because it invalidated the prompt cache each time it fired, could
            // fire repeatedly, and still ended here.
            self.maybe_self_compact(&mut on_event).await;
            // Cost budget: stop before issuing another model call once the
            // session's estimated spend (incl. sub-agents) reaches the cap.
            if let Err(error) = self.budget_preflight().await {
                on_event(AgentEvent::Notice(error.to_string()));
                return Err(error);
            }
            // Stream one assistant turn, accumulating text + tool calls. The
            // connect is retried on transient errors and auto-compacted once on
            // a context-length overflow. Mid-stream failures are retried too
            // (history is unchanged at that point, so re-requesting is safe).
            let Drained { acc, decode } = self
                .connect_and_drain(
                    &defs,
                    RequestHistory::Canonical,
                    &mut overflow_compacted,
                    &mut on_event,
                )
                .await?;
            if let Some(warning) = hrdr_llm::take_client_warning() {
                on_event(AgentEvent::Notice(warning));
            }
            // The sandbox's own degradation channel: a shell command that ran
            // with less OS confinement than its mode promised says so here,
            // once per agent, through the same event the frontends already
            // render. The channel is this agent's own — a sibling's degradation
            // is a sibling's news, and draining a shared queue told whichever
            // session got here first.
            if let Some(warning) = self.ctx.sandbox_notices.take() {
                on_event(AgentEvent::Notice(warning));
            }

            // Emit usage for the status bar + auto-compaction. Prefer the
            // server's reported counts; when it doesn't send any (e.g. a server
            // that ignores `stream_options.include_usage`), fall back to a rough
            // estimate so the context bar and compaction still work — an estimate
            // beats a stale/zero reading, and the overflow-retry path covers any
            // under-estimate.
            let spend = self.account_usage(&acc, tool_tokens).await;
            self.last_prompt_tokens = Some(spend.prompt_tokens);
            on_event(AgentEvent::Usage {
                prompt_tokens: spend.prompt_tokens,
                completion_tokens: spend.completion_tokens,
                decode_ms: decode.as_millis().min(u32::MAX as u128) as u32,
                cached_prompt_tokens: spend.cached_prompt_tokens,
                cache_creation_tokens: spend.cache_creation_tokens,
                reasoning_tokens: acc.usage.as_ref().and_then(|u| u.reasoning_tokens()),
                cost_usd: spend.cost_usd,
                session_cost_usd: spend.session_cost_usd,
                cost_partial: self.session_cost_partial(),
            });

            // The reply hit the output cap — warn so a silently-truncated answer
            // or edit isn't mistaken for a complete one (raise `max_tokens` on the
            // Anthropic backend, or the model's cap otherwise). The *model* is
            // told too, on this round's last tool result (see below): it otherwise
            // resumes believing it emitted everything it meant to.
            let truncated = acc.truncated();
            if truncated {
                on_event(AgentEvent::Notice(
                    "⚠ response truncated at the output limit — it may be incomplete \
                     (raise max_tokens if this recurs)"
                        .to_string(),
                ));
            }

            let mut assistant = acc.into_message();
            ensure_assistant_has_content(&mut assistant);
            let tool_calls = assistant.tool_calls.clone().unwrap_or_default();
            // A server that isn't parsing tool calls hands the template's raw
            // markup back as ordinary text instead, with a 200 and no error to
            // catch. Say so once, with the flags that fix it — see
            // [`UNPARSED_TOOL_CALL_NOTICE`].
            if !self.tool_syntax_warned
                && tool_calls.is_empty()
                && !defs.is_empty()
                && let Some(text) = assistant.content.as_deref()
                && looks_like_unparsed_tool_call(text)
            {
                self.tool_syntax_warned = true;
                on_event(AgentEvent::Notice(UNPARSED_TOOL_CALL_NOTICE.to_string()));
            }
            Arc::make_mut(&mut self.messages).push(assistant);

            if tool_calls.is_empty() {
                // A degraded high-context model sometimes ends its turn on a
                // promise instead of doing the work — "I'll implement now",
                // zero tool calls, TODO items left dangling, and no background
                // sub-agent still doing that work. Give it exactly one chance
                // per turn to either finish the list or explicitly defer it,
                // instead of silently accepting the promise as done.
                if !nudged_this_turn && self.bg_handle_count() == 0 {
                    let unfinished: Vec<TodoItem> = self
                        .ctx
                        .todos
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .iter()
                        .filter(|t| {
                            t.status.as_str() != "completed" && t.status.as_str() != "cancelled"
                        })
                        .cloned()
                        .collect();
                    if !unfinished.is_empty() {
                        nudged_this_turn = true;
                        // Watch the rounds that follow for the list being
                        // *emptied* instead of reconciled (see below).
                        todo_watch = Some(TodoShrinkWatch {
                            unfinished: unfinished.iter().map(|t| t.content.clone()).collect(),
                            len: self.todo_snapshot().1,
                        });
                        on_event(AgentEvent::Notice(format!(
                            "turn ended with {} unfinished TODOs — nudging the model to \
                             finish or defer explicitly",
                            unfinished.len()
                        )));
                        self.push_user_message(
                            format!(
                                "[Your turn was about to end, but these TODO items are not \
                                 finished:\n{}\nEither continue now and complete them, or \
                                 reconcile the list item by item with the todo tool: send the \
                                 full list back with every one of these items still in it, \
                                 each marked `completed` or `cancelled` — and say plainly, to \
                                 the user, why you cancelled what you cancelled. Do not \
                                 replace, collapse, or drop items to make the list look \
                                 finished; a shorter list is not a resolved list.]",
                                render_unfinished_todos(&unfinished)
                            ),
                            MessageOrigin::Nudge,
                        );
                        continue;
                    }
                }
                // The model answered without calling a tool: the turn is over,
                // even if a steering message is pending. It has no tool result to
                // ride in on, so the frontend sends it as a turn of its own —
                // steering redirects work in progress, it doesn't extend a turn
                // the model already finished.
                self.fire_turn_end_hooks(&mut on_event).await;
                self.release_finished_subagents();
                self.age_todos();
                on_event(AgentEvent::TurnDone);
                return Ok(());
            }

            // Execute the requested tools, feeding results back. Runs of
            // consecutive concurrency-safe calls (reads/searches/fetches, and
            // `task` sub-agents) execute concurrently; a file-mutating call is a
            // barrier, run alone — so a read after a write still observes the
            // write, and results always land in call order.
            let mut idx = 0;
            while idx < tool_calls.len() {
                let concurrent = self.tools.is_concurrent(&tool_calls[idx].function.name);
                let mut end = idx + 1;
                while concurrent
                    && end < tool_calls.len()
                    && self.tools.is_concurrent(&tool_calls[end].function.name)
                {
                    end += 1;
                }
                let batch = &tool_calls[idx..end];
                idx = end;

                // One path for both: a read-only run executes concurrently, a
                // lone mutating call is a one-element batch. The refusal check,
                // arg parse, streamed output, and in-order results all live in
                // `run_tool_batch`.
                self.run_tool_batch(batch, &mut repeat, &mut on_event).await;
            }

            // A truncated reply is the one thing the model cannot notice itself:
            // it was cut off, so whatever it meant to emit after this point never
            // reached us, and next round it reads its own message as complete.
            // Ride this round's last tool result — the same channel as the budget
            // notes below — so it re-issues what is missing instead of assuming it
            // ran. Before the History snapshot, so a resume sees the note too.
            //
            // A truncated reply with *no* tool calls has no result to ride on and
            // ends the turn where it stands; that case is left to the user-facing
            // Notice above, since there is no next round to correct.
            if truncated
                && let Some(last) = Arc::make_mut(&mut self.messages).last_mut()
                && let Some(content) = &mut last.content
            {
                content.push_str(
                    "\n\n[note: your reply was cut off at the output limit — anything you \
                     intended after this point, including further tool calls, was lost and \
                     never ran. Re-issue what is missing rather than assuming it happened, and \
                     keep the next reply shorter.]",
                );
            }

            // Backstop for the turn-end TODO nudge: the model can "satisfy" it by
            // replacing the list with one collapsed "all done" item instead of
            // resolving the items it was shown. Fires at most once per turn (the
            // watch is disarmed either way), and only on the unambiguous shape —
            // the list got *shorter* and an item the nudge named is gone
            // altogether, not merely reworded or re-statused.
            if let Some(watch) = todo_watch.take() {
                let (current, len) = self.todo_snapshot();
                let removed: Vec<&String> = watch
                    .unfinished
                    .iter()
                    .filter(|c| !current.contains(c))
                    .collect();
                if len < watch.len && !removed.is_empty() {
                    on_event(AgentEvent::Notice(format!(
                        "{} nudged TODO items were removed rather than resolved — asking \
                         the model to restore them",
                        removed.len()
                    )));
                    let list = removed
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.push_user_message(
                        format!(
                            "[These TODO items were removed from the list rather than \
                             resolved:\n{list}\nDeleting an item is not finishing it. Send the \
                             list back through the todo tool with each of them restored and \
                             marked `completed` or `cancelled`, and tell the user which ones \
                             you did not actually do.]"
                        ),
                        MessageOrigin::Nudge,
                    );
                } else {
                    // Not a shrink (yet) — keep watching the rest of the turn.
                    todo_watch = Some(watch);
                }
            }

            // Mid-turn durability: every result of this round is committed, so
            // hand the frontend a history snapshot to persist. A crash from
            // here on loses at most the next round.
            on_event(AgentEvent::History(Arc::clone(&self.messages)));

            // 80% of the budget: one soft warning, early enough that the model
            // can still commit what's done and sequence what's left. Rides the
            // last tool result of the round, exactly like the wrap-up note below
            // — the model reads it with that round's results.
            let used = step;
            if !checkpoint_warned
                && checkpoint_warning_round(self.max_steps) == Some(used)
                && let Some(last) = Arc::make_mut(&mut self.messages).last_mut()
                && let Some(content) = &mut last.content
            {
                checkpoint_warned = true;
                content.push_str(&format!(
                    "\n\n[note: you've used {used} of {max} tool rounds this turn — checkpoint \
                     your work (commit what's done) and sequence what remains; the turn ends \
                     at {max}]",
                    max = self.max_steps
                ));
            }

            // Near the budget: tell the model so it wraps up instead of
            // getting cut off mid-plan.
            let remaining = self.max_steps - step;
            if remaining == WRAP_UP_WARNING_ROUNDS
                && let Some(last) = Arc::make_mut(&mut self.messages).last_mut()
                && let Some(content) = &mut last.content
            {
                content.push_str(&format!(
                    "\n\n[note: only {remaining} tool rounds remain this turn — finish up \
                     and summarize]"
                ));
            }
        }

        // Budget exhausted: instead of failing the turn, run one final round
        // with no tools so the model must answer in text.
        on_event(AgentEvent::Notice(format!(
            "tool-round limit reached ({}) — asking the model to wrap up",
            self.max_steps
        )));
        Arc::make_mut(&mut self.messages).push(ChatMessage::user(
            "[The tool-call budget for this turn is exhausted. Do not request more tool \
             calls. Summarize what you accomplished and what remains to be done.]"
                .to_string(),
        ));
        // No `tools` are sent for this round (the model must answer in text),
        // but the turn's history is full of tool_use/tool_result blocks from
        // the rounds that already ran — the native Anthropic backend 400s any
        // request carrying those without a `tools` definition. Each connection
        // attempt builds a fresh protocol-free request view while canonical
        // history stays in `self.messages`, so overflow recovery compacts the
        // canonical history and the retry flattens that smaller result.
        if let Err(error) = self.budget_preflight().await {
            on_event(AgentEvent::Notice(error.to_string()));
            return Err(error);
        }
        let Drained { acc, decode } = self
            .connect_and_drain(
                &[],
                RequestHistory::ProtocolFree,
                &mut overflow_compacted,
                &mut on_event,
            )
            .await?;
        // No `tools` went out on this round (see above), so none are in the
        // prompt to account for.
        let spend = self.account_usage(&acc, 0).await;
        on_event(AgentEvent::Usage {
            prompt_tokens: spend.prompt_tokens,
            completion_tokens: spend.completion_tokens,
            decode_ms: decode.as_millis().min(u32::MAX as u128) as u32,
            cached_prompt_tokens: spend.cached_prompt_tokens,
            cache_creation_tokens: spend.cache_creation_tokens,
            reasoning_tokens: acc
                .usage
                .as_ref()
                .and_then(|usage| usage.reasoning_tokens()),
            cost_usd: spend.cost_usd,
            session_cost_usd: spend.session_cost_usd,
            cost_partial: self.session_cost_partial(),
        });
        let mut wrap_up_reply = acc.into_message();
        ensure_assistant_has_content(&mut wrap_up_reply);
        Arc::make_mut(&mut self.messages).push(wrap_up_reply);
        self.fire_turn_end_hooks(&mut on_event).await;
        self.release_finished_subagents();
        self.age_todos();
        on_event(AgentEvent::TurnDone);
        Ok(())
    }

    /// Deliver one queued user message into the turn: (opening only) run the
    /// `user_prompt` hook, then emit [`AgentEvent::Steered`] carrying the display
    /// form and push the (possibly hook-augmented) `sent` text into history.
    ///
    /// The single path a user message takes to reach the model, whether it opens
    /// a turn or steers one already in flight — so a normal message and a steering
    /// message are the same thing (a queued message), differing only in *when*
    /// they are drained. Both announce themselves with `Steered`, so every user
    /// turn is in the event stream.
    ///
    /// Returns `Err` only when a `user_prompt` hook blocks the turn (opening
    /// only); a mid-turn steer never runs the hook, so it never blocks.
    pub(crate) async fn deliver_user_message<F: FnMut(AgentEvent)>(
        &mut self,
        msg: Steer,
        opening: bool,
        on_event: &mut F,
    ) -> Result<()> {
        let mut sent = msg.sent;
        // The user's original text, before any hook/recall augmentation — recall
        // keys on what the user actually typed, not the expanded form.
        let query = sent.clone();
        // `user_prompt` hooks see the message before the turn starts: a block
        // (exit 2) fails the turn before anything enters history; hook stdout
        // rides along as extra context for the model (the frontend still displays
        // only what the user typed). This fires for the turn opener, not for a
        // mid-turn steer — preserving today's behavior.
        if opening
            && !sent.trim().is_empty()
            && self.has_event_hooks(hrdr_tools::HookEvent::UserPrompt)
        {
            let payload = serde_json::json!({
                "event": "user_prompt",
                "prompt": sent,
                "cwd": self.ctx.cwd.display().to_string(),
                "model": self.client.model,
            });
            let out = hrdr_tools::run_event_hooks(
                &self.event_hooks,
                hrdr_tools::HookEvent::UserPrompt,
                None,
                &payload,
                &self.ctx.cwd,
            )
            .await;
            for note in out.notes {
                on_event(AgentEvent::Notice(note));
            }
            if let Some(reason) = out.block {
                bail!("blocked by user_prompt hook: {reason}");
            }
            if !out.context.is_empty() {
                sent.push_str("\n\n[hook context]\n");
                sent.push_str(&out.context.join("\n"));
            }
        }
        // Relevance recall (opening only, same shape as the hook context above):
        // surface the full text of the memories most relevant to the user's
        // original message into the model-facing `sent`, so it has the facts, not
        // just the always-loaded pointer index. Keyed on `query` (what the user
        // typed), never the hook-augmented text. `recall` is sync `std::fs` over
        // small files and best-effort — safe to call here without `spawn_blocking`.
        // The transcript/`Steered` display stays `msg.display`; a mid-turn steer
        // never recalls (guarded on `opening`, exactly like the hook).
        if opening
            && !query.trim().is_empty()
            && let Some(block) = hrdr_tools::memory::recall(
                self.ctx.memory_project.as_deref(),
                self.ctx.memory_global.as_deref(),
                &query,
                MEMORY_RECALL_BUDGET,
            )
        {
            sent.push_str("\n\n");
            sent.push_str(&block);
        }
        // The model reads the expanded (`sent`) form; the transcript shows what was
        // typed (`display`). Opener or mid-turn correction, this is the user
        // speaking, so both are plain `User` turns: the origin marker records WHO
        // sent a message, not when it arrived.
        on_event(AgentEvent::Steered(msg.display));
        self.push_user_message(sent, MessageOrigin::User);
        Ok(())
    }

    /// Emit the `ToolEnd` event and push the tool-result message for a
    /// completed call (shared by the sequential and concurrent paths). Feeds
    /// the repeat breaker, appending its nudge to a repeated call — failing or
    /// succeeding.
    fn finish_tool_call<F: FnMut(AgentEvent)>(
        &mut self,
        call: &hrdr_llm::ToolCall,
        elapsed: std::time::Duration,
        result: Result<String>,
        repeat: &mut RepeatGuard,
        on_event: &mut F,
    ) {
        let (ok, mut body) = match result {
            Ok(s) => (true, s),
            Err(e) => (false, tool_error_text(&e)),
        };
        let repeatable = self.tools.is_repeatable(&call.function.name);
        if let Some(nudge) = repeat.record(
            &call.function.name,
            &call.function.arguments,
            ok,
            repeatable,
        ) {
            body.push_str(&nudge);
        }
        on_event(AgentEvent::ToolEnd {
            id: call.id.clone(),
            name: call.function.name.clone(),
            result: body.clone(),
            ok,
        });
        // The `todo` tool replaces the shared list; emit the new state so every
        // listener — including this agent's own event log — records the update.
        if call.function.name == "todo" {
            let todos = self
                .ctx
                .todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            on_event(AgentEvent::TodoUpdated(todos));
        }
        // Record the call's wall-clock cost for the MODEL, appended after
        // (outside) any untrusted-content wrapper the tool added — trusted
        // harness metadata, present on failures too. Kept out of the ToolEnd
        // display event above: `(took 0ms)` on every instant tool is just noise
        // in the transcript, and the model is what asked for the timing.
        //
        // The mutation result is handed to the model in full: the diff is how
        // it verifies its own edit landed as intended and repairs what it did
        // wrong.
        let recorded = format!("{body}\n\n(took {})", format_duration(elapsed));
        Arc::make_mut(&mut self.messages).push(ChatMessage::tool_result(call.id.clone(), recorded));
    }

    /// Run a batch of tool calls, forwarding each call's streamed output as
    /// `ToolOutput` events (attributed by call id) while they run. A read-only
    /// run executes concurrently; a lone mutating call is a one-element batch.
    /// Results are emitted and recorded in call order.
    ///
    /// Each call runs as its own spawned task — so CPU-bound tools run in
    /// parallel across workers instead of serializing inside this turn task —
    /// and is joined through a [`ToolBatchGuard`], which owns two behaviors:
    ///
    /// - **Panic containment**: a panicking tool no longer unwinds this task,
    ///   because `spawn` captures the panic into the handle. It is resumed
    ///   after the join (see the body) so the turn task's `catch_unwind` still
    ///   records the `panicked` outcome and closes the open tool calls.
    /// - **Cancellation**: on a cancelled turn the guard's `Drop` aborts every
    ///   in-flight handle, dropping the tool futures (and killing shell
    ///   children via `kill_on_drop`) instead of silently detaching them.
    async fn run_tool_batch<F: FnMut(AgentEvent)>(
        &mut self,
        batch: &[hrdr_llm::ToolCall],
        repeat: &mut RepeatGuard,
        on_event: &mut F,
    ) {
        // One shared (id, chunk) channel; each call gets a private sink whose
        // chunks a forwarder task tags with the call id.
        //
        // Both channels are bounded — this is advisory live-progress output,
        // not the tool result, so a producer that outruns the UI consumer
        // (e.g. a shell command emitting millions of lines) must never queue
        // unboundedly. UI_STREAM_CAP buffers a normal burst; past that,
        // `ctx.emit`'s `try_send` (see `ToolContext::emit`) drops lines
        // rather than blocking the tool, and the forwarder below does the
        // same into `shared_tx` — dropping at either stage just means the UI
        // sees gaps in the live stream, never the model or the tool result.
        let (shared_tx, mut shared_rx) =
            tokio::sync::mpsc::channel::<(String, String)>(UI_STREAM_CAP);
        let mut futs = Vec::with_capacity(batch.len());
        for call in batch {
            // Record the arguments the call will RUN with, not the ones it was
            // typed with: every optional value the tool falls back to is frozen in
            // here, so a session read back after a default changes still describes
            // what actually happened. Unparseable arguments are passed through —
            // the tool is about to reject them, and the record should show what it
            // was given.
            let recorded = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                .ok()
                .and_then(|parsed| {
                    let tool = self.tools.get(&call.function.name)?;
                    serde_json::to_string(&tool.recorded_args(&parsed, &self.ctx)).ok()
                })
                .unwrap_or_else(|| call.function.arguments.clone());
            on_event(AgentEvent::ToolStart {
                id: call.id.clone(),
                name: call.function.name.clone(),
                args: recorded,
            });
            let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(UI_STREAM_CAP);
            let fwd_tx = shared_tx.clone();
            let fwd_id = call.id.clone();
            tokio::spawn(async move {
                while let Some(chunk) = rx.recv().await {
                    let _ = fwd_tx.try_send((fwd_id.clone(), chunk));
                }
            });
            let mut ctx = self.ctx.clone();
            ctx.stream = Some(tx);
            // So a `task` call can tag the background entry it spawns with the
            // transcript entry it came from.
            ctx.call_id = Some(call.id.clone());
            let name = call.function.name.clone();
            let raw_args = call.function.arguments.clone();
            // Cheap clone (Arc-backed registry) so the futures don't borrow
            // `self` — results are recorded with `&mut self` right after.
            let tools = self.tools.clone();
            let hooks = Arc::clone(&self.event_hooks);
            // A refused call (repeat breaker) resolves immediately instead of
            // executing; boxing keeps the join order == call order.
            let fut: std::pin::Pin<Box<dyn std::future::Future<Output = TimedResult> + Send>> =
                match repeat.refusal(&name, &raw_args) {
                    // A refused call never ran, so its cost is zero.
                    Some(msg) => {
                        Box::pin(
                            async move { (std::time::Duration::ZERO, Err(anyhow::anyhow!(msg))) },
                        )
                    }
                    None => Box::pin(async move {
                        let start = std::time::Instant::now();
                        let res: Result<String> = async move {
                            let args: serde_json::Value = if raw_args.trim().is_empty() {
                                serde_json::json!({})
                            } else {
                                match serde_json::from_str(&raw_args) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        return Err(anyhow::anyhow!(
                                            "invalid tool arguments JSON: {e}"
                                        ));
                                    }
                                }
                            };
                            // `pre_tool` hooks can veto the call (exit 2): the
                            // model sees the hook's reason as the tool error.
                            if hooks
                                .iter()
                                .any(|h| h.event == hrdr_tools::HookEvent::PreTool)
                            {
                                let payload = serde_json::json!({
                                    "event": "pre_tool",
                                    "tool": name,
                                    "args": args,
                                    "cwd": ctx.cwd.display().to_string(),
                                });
                                let out = hrdr_tools::run_event_hooks(
                                    &hooks,
                                    hrdr_tools::HookEvent::PreTool,
                                    Some(&name),
                                    &payload,
                                    &ctx.cwd,
                                )
                                .await;
                                if let Some(reason) = out.block {
                                    return Err(anyhow::anyhow!(
                                        "blocked by pre_tool hook: {reason}"
                                    ));
                                }
                                for note in out.notes {
                                    ctx.emit(format!("{note}\n"));
                                }
                            }
                            let mut res = tools.execute(&name, args.clone(), &ctx).await;
                            // `post_tool` hooks see the (bounded) result; their
                            // complaints ride back to the model with it.
                            if hooks
                                .iter()
                                .any(|h| h.event == hrdr_tools::HookEvent::PostTool)
                            {
                                let (ok, result_text) = match &res {
                                    Ok(r) => (true, hrdr_tools::truncate_inline(r, 30_000)),
                                    Err(e) => (false, e.to_string()),
                                };
                                let payload = serde_json::json!({
                                    "event": "post_tool",
                                    "tool": name,
                                    "args": args,
                                    "ok": ok,
                                    "result": result_text,
                                    "cwd": ctx.cwd.display().to_string(),
                                });
                                let out = hrdr_tools::run_event_hooks(
                                    &hooks,
                                    hrdr_tools::HookEvent::PostTool,
                                    Some(&name),
                                    &payload,
                                    &ctx.cwd,
                                )
                                .await;
                                let notes: Vec<String> =
                                    out.notes.into_iter().chain(out.block).collect();
                                if !notes.is_empty() {
                                    let joined = notes.join("\n");
                                    res = match res {
                                        Ok(r) => Ok(format!("{r}\n{joined}")),
                                        Err(e) => Err(anyhow::anyhow!("{e}\n{joined}")),
                                    };
                                }
                            }
                            res
                        }
                        .await;
                        (start.elapsed(), res)
                    }),
                };
            // Each call runs as its own task, so a CPU-bound tool executes on a
            // worker thread of its own instead of blocking this turn task's
            // turn. The handle keeps join order == call order.
            futs.push(tokio::spawn(fut));
        }
        drop(shared_tx); // forwarders hold the remaining senders

        // The spawned tasks are awaited through `&mut` handles, so `joined`
        // borrows the guard's vec: on unwind — the `resume_unwind` below or the
        // whole turn task being aborted (Esc-Esc) — `joined` drops first,
        // releasing the borrow, and the guard's `Drop` then aborts every
        // in-flight handle. That abort drops the tool futures, and a shell
        // tool's `kill_on_drop` kills the child.
        let mut guard = ToolBatchGuard { handles: futs };
        let joined = futures_util::future::join_all(guard.handles.iter_mut());
        tokio::pin!(joined);
        let joined_results = loop {
            tokio::select! {
                r = &mut joined => break r,
                Some((id, chunk)) = shared_rx.recv() => {
                    on_event(AgentEvent::ToolOutput { id, chunk });
                }
            }
        };
        // Drain chunks buffered between the last poll and completion.
        while let Ok((id, chunk)) = shared_rx.try_recv() {
            on_event(AgentEvent::ToolOutput { id, chunk });
        }
        // A panicking tool no longer unwinds the turn task directly: `spawn`
        // captures the panic into the handle, and `join_all` hands it back as a
        // `JoinError`. Resume it here so it propagates exactly as before —
        // through `Agent::run` to the turn task's `catch_unwind`, which records
        // `TurnOutcome { panicked: true }` and closes the open tool calls. A
        // cancelled handle is defensive only (nothing in this scope aborts an
        // individual task): surface it as a normal tool error.
        let results: Vec<TimedResult> = joined_results
            .into_iter()
            .map(|r| match r {
                Ok(r) => r,
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => (
                    std::time::Duration::ZERO,
                    Err(anyhow::anyhow!("tool task cancelled: {e}")),
                ),
            })
            .collect();
        for (call, (elapsed, result)) in batch.iter().zip(results) {
            self.finish_tool_call(call, elapsed, result, repeat, on_event);
        }
    }

    /// Recover one context overflow per turn, regardless of whether the provider
    /// rejected the request while connecting or reported failure in the stream.
    /// Returns `true` only when compaction shrank history and the caller should
    /// retry without spending its transient [`RetryBudget`].
    async fn recover_context_overflow<F: FnMut(AgentEvent)>(
        &mut self,
        error: &anyhow::Error,
        overflow_compacted: &mut bool,
        on_event: &mut F,
    ) -> Result<bool> {
        if !is_context_overflow(error) || *overflow_compacted || self.messages.len() <= 2 {
            return Ok(false);
        }
        on_event(AgentEvent::Notice(
            "context window exceeded — compacting and retrying".to_string(),
        ));
        let report = self
            .compact(hrdr_llm::CompactionReason::ContextOverflow, None, on_event)
            .await?;
        *overflow_compacted = true;
        if !report.shrank() {
            bail!(
                "context window exceeded and the current turn is too large to compact \
                 ({} messages, nothing left to shrink) — {error}",
                report.after
            );
        }
        // What the rescue itself cost, and how much of it the prefix cache
        // absorbed — the notice above fired before the call and could not know.
        on_event(AgentEvent::Notice(report.notice()));
        Ok(true)
    }

    /// Recover from this endpoint rejecting an optional request parameter, by
    /// dropping it and reporting `true` so the caller retries the same request
    /// without it. `false` for every other error — including a second rejection
    /// of a parameter already dropped, which is what bounds the caller's loop.
    ///
    /// **Session-wide and one-way.** Re-probing a parameter the server has
    /// already refused cannot discover anything new and buys another guaranteed
    /// 400, so the drop outlives the request that provoked it — and outlives the
    /// *compaction* that provoked it, which is why this lives on the agent
    /// rather than in a `&mut bool` threaded through one summarization.
    ///
    /// The user is told, because the parameter may be one they configured and
    /// this silently changes what their setting does. The notice goes on
    /// `pending_notices` rather than to a sink parameter: `compact` is
    /// deliberately silent and has none to offer, and queueing means the one
    /// caller that *does* have a sink still delivers it immediately — the drain
    /// at the top of [`Self::connect_stream`] runs on the retry this returns
    /// `true` for.
    pub(crate) fn drop_unsupported_param(&mut self, error: &anyhow::Error) -> bool {
        let Some(param) = hrdr_llm::unsupported_param(error) else {
            return false;
        };
        if self.unsupported_params.contains(&param) {
            return false;
        }
        self.unsupported_params.push(param);
        self.client.clear_unsupported_param(param);
        self.pending_notices.push(format!(
            "this endpoint rejected `{}` — dropping it for the rest of the session",
            param.as_str()
        ));
        true
    }

    /// Stream one assistant turn, retrying both the connect and any transient
    /// mid-stream failure with the same backoff the connect path uses. A failed
    /// `drain_stream` appends nothing to history, so a clean re-request is safe
    /// — the one exception is [`Self::recover_context_overflow`], which shrinks
    /// history on purpose and is why the retry re-reads `self.messages` rather
    /// than resending a request view captured before the failure.
    ///
    /// **The round is the unit the retry budget is measured in.** Connecting and
    /// draining are two ways for one request to fail, so they share the single
    /// [`RetryBudget`] created here, threaded into [`Self::connect_stream`].
    /// They used to hold a budget each — 3 drain retries wrapped around 4
    /// connect retries — and because the connect loop restarted on every pass of
    /// the drain loop, its allowance was handed back out four times: one round
    /// could issue 20 requests, a number neither constant named. Anything added
    /// to this loop later must take `budget` too, not open its own.
    async fn connect_and_drain<F: FnMut(AgentEvent)>(
        &mut self,
        defs: &[ToolDef],
        history: RequestHistory,
        overflow_compacted: &mut bool,
        on_event: &mut F,
    ) -> Result<Drained> {
        let mut budget = RetryBudget::new(self.retry_policy);
        loop {
            let mut stream = self
                .connect_stream(defs, history, overflow_compacted, &mut budget, on_event)
                .await?;
            match drain_stream(&mut stream, on_event).await {
                // Only the round that actually streamed is timed: the retried
                // attempts and the backoff between them are not generation.
                Ok(drained) => return Ok(drained),
                Err(e) => {
                    if self
                        .recover_context_overflow(&e, overflow_compacted, on_event)
                        .await?
                    {
                        continue;
                    }
                    if self.drop_unsupported_param(&e) {
                        continue;
                    }
                    let retried = budget
                        .retry(&e, &mut |a: RetryAttempt| {
                            on_event(AgentEvent::Notice(format!(
                                "stream interrupted — retrying in {:.0}s \
                                 (attempt {}/{})",
                                a.delay.as_secs_f64(),
                                a.attempt,
                                a.max_attempts
                            )));
                        })
                        .await;
                    if !retried {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Before a request, inject fresh OAuth credentials for trusted ChatGPT, or
    /// strip any stale OAuth state when this is not ChatGPT.
    ///
    /// The gate is [`ResolvedModel::is_codex_oauth`] — the trusted kind AND the
    /// canonical endpoint, one definition — so a custom shadow, or a ChatGPT
    /// identity anywhere else, never receives the bearer/account header. On the
    /// non-ChatGPT path
    /// the resolved provider's own headers are restored (dropping any
    /// `ChatGPT-Account-Id` left over from a prior ChatGPT turn); the API key is
    /// left untouched (it is the key provider's real credential).
    pub(crate) async fn refresh_oauth_if_needed(&mut self) {
        if !self.resolved.is_codex_oauth() {
            // Defensive: ensure no stale bearer/account header survives a switch
            // away from ChatGPT. Idempotent for a steady-state key provider.
            if self.client.extra_headers_contains("ChatGPT-Account-Id") {
                self.client.set_headers(self.resolved.headers().to_vec());
            }
            return;
        }
        // A failed refresh leaves the previous state untouched; the authenticated
        // catalog/health path surfaces a genuine auth warning.
        if let Ok(access) =
            oauth::coordinated_oauth_access(self.resolved.kind(), self.resolved.base_url()).await
        {
            self.client.set_api_key(Some(access.access));
            let mut headers = self.resolved.headers().to_vec();
            if let Some(id) = access.account_id {
                headers.push(("ChatGPT-Account-Id".to_string(), id));
            }
            self.client.set_headers(headers);
        }
    }

    /// Open a chat stream, retrying transient network/server errors with
    /// exponential backoff and auto-compacting once on a context-length
    /// overflow. Emits `Notice` events for each recovery attempt.
    ///
    /// `budget` belongs to the caller's round, not to this function — see
    /// [`Self::connect_and_drain`] for why it must not create its own.
    async fn connect_stream<F: FnMut(AgentEvent)>(
        &mut self,
        defs: &[ToolDef],
        history: RequestHistory,
        overflow_compacted: &mut bool,
        budget: &mut RetryBudget,
        on_event: &mut F,
    ) -> Result<ChatStream> {
        self.refresh_oauth_if_needed().await;
        loop {
            // Deliver anything queued since the last turn started — in practice
            // a parameter this endpoint refused, dropped either by the retry
            // that lands here or by a summarizer call, which has no sink of its
            // own (see `drop_unsupported_param`).
            for notice in self.take_pending_notices() {
                on_event(AgentEvent::Notice(notice));
            }
            let flattened = match history {
                RequestHistory::Canonical => None,
                RequestHistory::ProtocolFree => Some(flatten_tool_protocol(&self.messages)),
            };
            let messages = flattened.as_deref().unwrap_or(&self.messages);
            match self.client.chat_stream(messages, defs).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    if self
                        .recover_context_overflow(&e, overflow_compacted, on_event)
                        .await?
                    {
                        continue;
                    }
                    if self.drop_unsupported_param(&e) {
                        continue;
                    }
                    // Transient network/server error → backoff and retry (the
                    // driver honours a server `Retry-After` over its own
                    // schedule). The overflow recovery above `continue`s
                    // *without* touching `budget`: a compaction is not a retry
                    // of the same request, it is a different, smaller one, and
                    // charging it here would cost the round a network retry it
                    // may still need.
                    if budget
                        .retry(&e, &mut |a: RetryAttempt| {
                            on_event(AgentEvent::Notice(format!(
                                "network error — retrying in {:.0}s (attempt {}/{})",
                                a.delay.as_secs_f64(),
                                a.attempt,
                                a.max_attempts
                            )));
                        })
                        .await
                    {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
    /// Release sub-agents whose work is done, whose answers the main agent has,
    /// and that nobody is looking at.
    ///
    /// At **turn end**, not per tool round. A blocking sub-agent is marked done and
    /// delivered inside the very round that spawned it (its answer *is* the tool
    /// result), so pruning mid-loop dropped it before the user could so much as see
    /// its row — the retained agent was unreachable in practice unless they were
    /// already looking at it. Holding until the turn ends gives the frontend the
    /// whole turn to pin the one being read.
    ///
    /// Running inside the agent, rather than leaving it to the frontend, is what
    /// keeps a headless run (which pins nothing) from leaking agents.
    fn release_finished_subagents(&mut self) {
        self.registry.prune();
    }

    /// Age out TODOs that have been finished for `todo_ttl` turns.
    ///
    /// The TODO list is the agent's own state — the model re-reads it every turn —
    /// so ageing belongs here, not in a frontend. It used to run only in the TUI,
    /// which meant a headless run and every delegated sub-agent carried their
    /// finished items forever and paid for them in context on every request.
    fn age_todos(&mut self) {
        self.todo_turn += 1;
        if let Ok(mut todos) = self.ctx.todos.lock() {
            age_completed_todos(
                &mut todos,
                &mut self.todo_completed_at,
                self.todo_turn,
                self.todo_ttl,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_llm::{ChatChunk, ChunkChoice, Delta};

    /// The unparsed-tool-call detector fires on the template markers a server
    /// leaks when it isn't parsing tool calls, and stays quiet for a model that
    /// is merely *talking* about tools — the false positive that would matter,
    /// since this drives a warning about the operator's setup.
    #[test]
    fn unparsed_tool_call_detection_keys_on_template_markers() {
        for leaked in [
            "<tool_call>\n{\"name\": \"read\", \"arguments\": {}}\n</tool_call>",
            "let me look: <|python_tag|>read(path=\"a.rs\")",
            "<function=read>{\"path\": \"a.rs\"}</function>",
            "[TOOL_CALLS] [{\"name\": \"read\"}]",
            "<|channel|>commentary to=functions.read",
            "functools[{\"name\": \"read\"}]",
        ] {
            assert!(
                looks_like_unparsed_tool_call(leaked),
                "must detect leaked markup: {leaked}"
            );
        }
        for innocent in [
            "I'll call the `read` tool with {\"path\": \"a.rs\"} next.",
            "The tool_call field was empty, so nothing ran.",
            "Use tools like read and grep to explore.",
            "",
        ] {
            assert!(
                !looks_like_unparsed_tool_call(innocent),
                "must not fire on ordinary prose: {innocent}"
            );
        }
    }

    fn chunk(content: Option<&str>, reasoning: Option<&str>) -> ChatChunk {
        ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta {
                    content: content.map(str::to_string),
                    reasoning_content: reasoning.map(str::to_string),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![],
        }
    }

    async fn events_for(chunks: Vec<ChatChunk>) -> (Vec<AgentEvent>, Accumulator) {
        let (events, drained) = drain(chunks).await;
        (events, drained.acc)
    }

    async fn drain(chunks: Vec<ChatChunk>) -> (Vec<AgentEvent>, Drained) {
        let mut stream: ChatStream =
            Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok)));
        let mut seen = Vec::new();
        let drained = drain_stream(&mut stream, &mut |ev| seen.push(ev))
            .await
            .unwrap();
        (seen, drained)
    }

    fn tool_chunk(args: &str) -> ChatChunk {
        ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![hrdr_llm::ToolCallDelta {
                        index: 0,
                        id: Some("c1".into()),
                        function: Some(hrdr_llm::FunctionDelta {
                            name: Some("write".into()),
                            arguments: Some(args.to_string()),
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![],
        }
    }

    /// A round that streams only tool-call arguments emits no `Text` and no
    /// `Reasoning` — the events a clock could hang off — yet the model spent
    /// that whole round generating. The stream is where those fragments are
    /// visible, so the stream is where the round gets timed.
    #[tokio::test]
    async fn a_tool_call_only_round_is_still_timed() {
        let (events, drained) = drain(vec![tool_chunk("{\"path\":"), tool_chunk("\"x\"}")]).await;
        assert!(
            events.is_empty(),
            "nothing to render, which is exactly the trap: {events:?}"
        );
        assert!(
            drained.decode > std::time::Duration::ZERO,
            "but the round is timed all the same"
        );
    }

    /// The usage-only trailing chunk is not output, so it must not open the
    /// generation window — a round that streamed nothing has no window at all.
    #[tokio::test]
    async fn a_round_that_streams_nothing_is_timed_at_zero() {
        let (_, drained) = drain(vec![chunk(Some(""), Some(""))]).await;
        assert_eq!(drained.decode, std::time::Duration::ZERO);
    }

    /// An empty delta is dropped rather than forwarded — in both directions.
    ///
    /// Regression: a Qwen3-style backend keeps emitting `reasoning_content: ""`
    /// on every content chunk once it stops thinking, and `content: ""` while it
    /// is still thinking. Providers that omit the field deserialize to `None` and
    /// never reach here. Forwarded, each empty event lands between two deltas of
    /// the *other* kind, and the frontend only merges into the previous entry
    /// when it is the matching kind — so the reply came out as one
    /// `#N assistant` header per token group, with reasoning shredded the same
    /// way. Both empties render as nothing, so the split was the only symptom.
    #[tokio::test]
    async fn empty_deltas_are_not_forwarded_as_events() {
        let (events, _) = events_for(vec![
            chunk(None, Some("thinking")),
            chunk(Some(""), None), // must not close the reasoning block
            chunk(None, Some(" harder")),
            chunk(Some("answer"), Some("")), // empty reasoning must not split text
            chunk(Some(" more"), Some("")),
        ])
        .await;

        let reasoning: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Reasoning(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let text: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(
            reasoning,
            vec!["thinking", " harder"],
            "no empty reasoning event may be forwarded"
        );
        assert_eq!(
            text,
            vec!["answer", " more"],
            "no empty text event may be forwarded"
        );
    }

    /// Suppressing the *event* must not suppress accumulation: `acc.push` still
    /// runs for every chunk, so the assembled reply is unaffected.
    #[tokio::test]
    async fn empty_deltas_still_accumulate_into_the_final_message() {
        let (_, acc) = events_for(vec![
            chunk(Some("hello"), Some("")),
            chunk(Some(""), None),
            chunk(Some(" world"), Some("")),
        ])
        .await;
        assert_eq!(acc.into_message().content.as_deref(), Some("hello world"));
    }
}

//! Sub-agent delegation, worktrees, and background-task orchestration —
//! extracted from [`Agent`] into its own module to keep `lib.rs` manageable.
//!
//! Holds the `task*` tool family (spawn/list/output/steer/cancel/cleanup/diff),
//! the background-handle registry and detached [`spawn_background`] path, the
//! git worktree lifecycle ([`Worktree`]/[`KeptWorktree`] and their gc/reaping),
//! the sub-agent transcript plumbing, and the per-task config derivation
//! ([`subagent_base_config`], the model-ref overrides, agent-profile resolution).
//! Move-only: behavior is unchanged.

use super::*;

/// Monotonic id source for detached background sub-agents (`task` background mode).
static BG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Shared list of background-task `JoinHandle`s, keyed by task id.
pub(crate) type BgHandles = Arc<Mutex<Vec<(u64, tokio::task::JoinHandle<()>)>>>;

/// Live sub-agent slots, by capability. Acquired before a `task` spawns and
/// released when it finishes, so the caps bound *concurrent* sub-agents rather
/// than how many a turn may issue in total.
#[derive(Debug, Default)]
pub(crate) struct SubagentSlots {
    read_only: std::sync::atomic::AtomicUsize,
    write: std::sync::atomic::AtomicUsize,
}

impl SubagentSlots {
    /// Take a slot, or `None` when `max` are already running. The compare-and-set
    /// loop matters: several `task` calls in one turn run concurrently, so a
    /// load-then-store would let them all pass a cap of 1.
    pub(crate) fn acquire(self: &Arc<Self>, write: bool, max: usize) -> Option<SubagentSlot> {
        use std::sync::atomic::Ordering;
        let counter = if write { &self.write } else { &self.read_only };
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()?;
        Some(SubagentSlot {
            slots: Arc::clone(self),
            write,
        })
    }

    pub(crate) fn live(&self, write: bool) -> usize {
        use std::sync::atomic::Ordering;
        let counter = if write { &self.write } else { &self.read_only };
        counter.load(Ordering::SeqCst)
    }
}

/// A held sub-agent slot; releases on drop, so a panicking or aborted sub-agent
/// can't leak one.
pub(crate) struct SubagentSlot {
    slots: Arc<SubagentSlots>,
    write: bool,
}

impl Drop for SubagentSlot {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        let counter = if self.write {
            &self.slots.write
        } else {
            &self.slots.read_only
        };
        let _ = counter.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

/// Create an empty [`BgHandles`] store.
pub(crate) fn bg_handles() -> BgHandles {
    Arc::new(Mutex::new(Vec::new()))
}

/// Spawn `cfg`'s sub-agent detached: it streams into the shared background
/// registry and, on completion, records its result there for the run loop to
/// deliver. Returns immediately with an acknowledgement for the model.
///
/// A write-capable sub-agent is handed its own git `worktree` (its `cfg.cwd` is
/// already pointed at it); the worktree's cleanup is detached here so it survives
/// the run for the parent to review and merge. A read-only sub-agent has
/// `worktree = None` and shares the main dir.
///
/// The task is wrapped in a nested spawn so a panic in the body sets
/// `done = true` with an error message rather than leaving the registry entry
/// live forever. The outer [`JoinHandle`](tokio::task::JoinHandle) is stored in
/// `handles` so [`Agent::clear`] can abort running tasks on session reset.
/// The most of a background sub-agent's final report delivered verbatim into
/// the parent's context, in bytes. The parent needs the answer, not a full
/// re-read of a long run — the durable transcript keeps everything, and an
/// oversized report is middle-truncated (`hrdr_tools::truncate_middle`) with
/// a pointer at the transcript for the rest.
pub(crate) const BACKGROUND_REPORT_MAX_BYTES: usize = 24_000;

#[allow(clippy::too_many_arguments)]
fn spawn_background(
    cfg: AgentConfig,
    prompt: String,
    label: String,
    tool_id: Option<String>,
    slot: SubagentSlot,
    registry: &Arc<Mutex<Vec<hrdr_tools::BackgroundTask>>>,
    handles: &BgHandles,
    cost_total: Arc<std::sync::Mutex<f64>>,
    cost_partial: Arc<std::sync::atomic::AtomicBool>,
    lsp: Option<Arc<hrdr_tools::LspRegistry>>,
    transcript_dir: SubagentDirCell,
    live: LiveSubagents,
    // Present for a write-capable sub-agent: its isolated worktree. `cfg.cwd` is
    // already set to the worktree by the caller; here we record its path/branch
    // on the registry entry and *detach* its cleanup, so it survives the run for
    // the parent to review and merge (removed only by `task_cancel` / reset).
    worktree: Option<Worktree>,
) -> Result<String> {
    use std::sync::atomic::Ordering;
    let id = BG_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let header = format!("↳ task#{id} ({}): {label}", cfg.model.model());
    // Identity for the live registry, taken before `tool_id` is moved into the
    // background-task row below.
    let live_key = LiveSubagents::next_key();
    let tool_id_for_live = tool_id.clone();
    let label_for_live = label.clone();
    let model_for_live = cfg.model.model().to_string();
    let provider_for_live = Some(cfg.model.provider().to_string());
    let base_url_for_live = cfg.base_url.clone();
    let usage_for_live = subagent_usage(&cfg);
    // Build and register synchronously so `task_steer` can address the id as soon as
    // `task` returns; registration inside the spawned future races the caller.
    let mut sub = Agent::new(cfg)?;
    // The parent's cwd, captured before `keep()` below discards it — needed at
    // completion time to compute the size summary (`git diff`/`git log` run from
    // here, like `task_diff`, since worktrees share the parent's object store).
    let repo_cwd = worktree.as_ref().map(|w| w.repo.clone());
    // Detach the worktree's automatic cleanup only now that the agent exists — so
    // if `Agent::new` fails above, the still-un-kept worktree is torn down by its
    // `Drop` instead of being orphaned (it is not yet on any registry). It must
    // outlive the run from here on; its path/branch go onto the registry entry.
    let worktree = worktree.map(|w| w.keep());
    // (repo cwd, branch) for the completion-time size summary — `None` for a
    // read-only task, which has no worktree/branch to diff.
    let size_summary_target: Option<(PathBuf, String)> = worktree
        .as_ref()
        .zip(repo_cwd)
        .map(|(w, repo)| (repo, w.branch.clone()));
    sub.cost_total = cost_total;
    sub.cost_partial = cost_partial;
    sub.attach_live(live.clone(), live_key);
    sub.ctx.lsp = lsp;
    let steering = steering_queue();
    let sub = Arc::new(tokio::sync::Mutex::new(sub));
    live.register(LiveSubagent {
        key: live_key,
        bg_id: Some(id),
        tool_id: tool_id_for_live,
        label: label_for_live,
        model: model_for_live.clone(),
        provider: provider_for_live,
        base_url: base_url_for_live,
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: Default::default(),
        usage: usage_for_live,
        events: subagent_live::event_log(),
        turn: TurnStats::default(),
        kind: SubagentKind::Background,
        agent: Arc::clone(&sub),
        steering: Arc::clone(&steering),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
    });
    // Open the durable transcript first, so its path can go onto the registry
    // entry as a `task_output` fallback. Shared so the inner task records events
    // and the outer guard can still write a terminal `End` on panic/cancel.
    let transcript = Arc::new(Mutex::new(
        resolve_subagent_dir(&transcript_dir)
            .and_then(|dir| open_next_subagent_transcript(&dir, &label)),
    ));
    let transcript_path = transcript
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|t| t.path().to_path_buf()));
    if let Ok(mut g) = transcript.lock()
        && let Some(t) = g.as_mut()
    {
        t.write(&subagent_transcript::Event::Start {
            model: model_for_live.clone(),
            label: label.clone(),
            kind: subagent_transcript::SpawnKind::Background,
            prompt: prompt.clone(),
        });
    }
    if let Ok(mut v) = registry.lock() {
        v.push(hrdr_tools::BackgroundTask {
            id,
            tool_id,
            label: label.clone(),
            log: header,
            done: false,
            result: None,
            delivered: false,
            cancelled: false,
            worktree: worktree.as_ref().map(|w| w.path.clone()),
            branch: worktree.as_ref().map(|w| w.branch.clone()),
            size_summary: None,
            model: model_for_live.clone(),
            started: Some(std::time::Instant::now()),
            transcript: transcript_path.clone(),
        });
    }
    let ts_inner = transcript.clone();
    let ts_outer = transcript;
    let reg = registry.clone();
    let reg_done = reg.clone();
    // One handle for the inner task (which registers the sub-agent once it
    // exists) and one for the outer guard (which marks it idle on every exit
    // path, including panic and cancellation).
    let live_done = live.clone();
    // The inner task does the actual work; the outer task is the panic guard:
    // it always sets `done = true` + a result, even on panic.
    let handle = tokio::spawn(async move {
        // The slot is released when this task ends — including on abort,
        // since the entire future is dropped.
        let _slot = slot;
        // Single task with catch_unwind so a panic sets done=true and writes a
        // terminal End event rather than crashing and leaving the registry entry
        // live forever. On abort the whole future is dropped — the slot and
        // RunGuard are released, and no stale result reaches the registry or
        // live-subagent store.
        let result = AssertUnwindSafe(async move {
            let mut out = String::new();
            // The contiguous assistant text since the last tool call — reset on
            // every `ToolStart`, appended on every `Text`. At the end of the run
            // this is the sub-agent's final report (its system prompt already
            // tells it that's the hand-off), as opposed to `out`, which is the
            // whole prose stream across every turn including interim narration
            // between tool calls. Only the report belongs in the parent's
            // context; `out` (and the durable transcript) still exist so a
            // run that ends mid-tool-call with no closing text has a fallback.
            let mut final_segment = String::new();
            let result: anyhow::Result<()> = async {
                // Open its record with the task it was given, so its transcript shows
                // the question and not just the answer.
                live.begin_turn(live_key);
                live.record(live_key, &AgentEvent::Steered(prompt.clone()));
                let _run_guard = RunGuard::new(live.clone(), live_key);
                let usage_live = live.clone();
                let mut sub = sub.lock().await;
                let mut next_prompt = prompt;
                loop {
                    sub.run(next_prompt, Arc::clone(&steering), |ev| {
                        // Its run is recorded on its own entry — what it did and what it
                        // spent. This is the *only* way a background sub-agent's work
                        // reaches a frontend: its `task` call returned the instant it was
                        // spawned, so there is no live tool call left to stream through.
                        usage_live.record(live_key, &ev);
                        if let Ok(mut g) = ts_inner.lock()
                            && let Some(t) = g.as_mut()
                            && let Some(tev) = subagent_event_for(&ev)
                        {
                            t.write(&tev);
                        }
                        let chunk = match ev {
                            AgentEvent::Text(t) => {
                                out.push_str(&t);
                                final_segment.push_str(&t);
                                Some(t)
                            }
                            AgentEvent::ToolStart { name, .. } => {
                                // A new tool call starts a fresh segment — whatever
                                // text preceded it was narration, not the report.
                                final_segment.clear();
                                Some(format!("\n· {name}"))
                            }
                            _ => None,
                        };
                        if let Some(c) = chunk
                            && let Ok(mut v) = reg.lock()
                            && let Some(t) = v.iter_mut().find(|t| t.id == id)
                        {
                            t.log.push_str(&c);
                        }
                    })
                    .await?;
                    let Some(next) = live.take_pending_or_finish(live_key) else {
                        break;
                    };
                    live.begin_turn(live_key);
                    live.record(live_key, &AgentEvent::Steered(next.display));
                    next_prompt = next.sent;
                }
                Ok(())
            }
            .await;
            match result {
                Ok(()) => {
                    let o = out.trim().to_string();
                    if let Ok(mut g) = ts_inner.lock()
                        && let Some(t) = g.as_mut()
                    {
                        // The transcript is the durable full record — its byte
                        // count is the whole run, not the (possibly narrower)
                        // report delivered to the parent below.
                        t.write(&subagent_transcript::Event::End {
                            status: subagent_transcript::EndStatus::Ok,
                            bytes: o.len(),
                        });
                    }
                    // Prefer the final segment (the report) over the full prose
                    // stream; fall back to `out` if the run ended mid-tool-call
                    // with no closing text (rare, but the segment would be empty).
                    let segment = final_segment.trim();
                    let report = if segment.is_empty() {
                        o.as_str()
                    } else {
                        segment
                    };
                    if report.is_empty() {
                        "(no text output)".to_string()
                    } else {
                        let over_budget = report.len() > BACKGROUND_REPORT_MAX_BYTES;
                        let mut text =
                            hrdr_tools::truncate_middle(report, BACKGROUND_REPORT_MAX_BYTES);
                        if over_budget && let Some(p) = &transcript_path {
                            text.push_str(&format!(
                                "\n\n(full transcript: {} — `read` it for the complete run)",
                                p.display()
                            ));
                        }
                        text
                    }
                }
                Err(e) => {
                    if let Ok(mut g) = ts_inner.lock()
                        && let Some(t) = g.as_mut()
                    {
                        t.write(&subagent_transcript::Event::Error {
                            msg: format!("{e:#}"),
                        });
                        t.write(&subagent_transcript::Event::End {
                            status: subagent_transcript::EndStatus::Failed,
                            bytes: out.len(),
                        });
                    }
                    format!("(background task failed: {e})")
                }
            }
        })
        .catch_unwind()
        .await;
        let final_result = match result {
            Ok(s) => s,
            Err(panic_err) => {
                let msg = panic_err
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic_err.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("(unknown panic)");
                if let Ok(mut g) = ts_outer.lock()
                    && let Some(t) = g.as_mut()
                {
                    t.write(&subagent_transcript::Event::End {
                        status: subagent_transcript::EndStatus::Panicked,
                        bytes: 0,
                    });
                }
                format!("(background task panicked: {msg})")
            }
        };
        // Best-effort size summary for the delivery message — computed here (once,
        // at completion, in this async context) rather than in `drain_background`,
        // which is synchronous formatting code and must not shell out. `None` for a
        // read-only task (no worktree/branch) or on any git failure; a missing
        // summary never blocks or fails the delivery.
        let size_summary = match &size_summary_target {
            Some((repo, branch)) => task_size_summary(repo, branch).await,
            None => None,
        };
        if let Ok(mut v) = reg_done.lock()
            && let Some(t) = v.iter_mut().find(|t| t.id == id)
        {
            t.done = true;
            t.result = Some(final_result);
            t.size_summary = size_summary;
        }
        // The sub-agent is idle now (RunGuard's drop inside catch_unwind
        // already sets running=false, done=true), but its answer is still
        // owed to the main agent, so `delivered` stays false — the entry
        // survives the prune until the result is injected via deliver_background.
        live_done.update(live_key, |e| {
            e.running = false;
            e.done = true;
        });
    });
    if let Ok(mut v) = handles.lock() {
        // Best-effort reaping: drop handles for tasks that have already
        // finished. A finished task's result is already recorded in the
        // registry, so dropping the JoinHandle is safe. This keeps the Vec
        // bounded over a long session without requiring an explicit drain.
        // Note: this is best-effort — a panicked task is also considered
        // finished (is_finished returns true) and is reaped here.
        v.retain(|(_, h)| !h.is_finished());
        v.push((id, handle));
    }
    let isolation = if worktree.is_some() {
        " It is write-capable, so it works in its own isolated git worktree; when it \
         finishes you review its changes and merge them into your working dir."
    } else {
        ""
    };
    Ok(format!(
        "Started background task #{id} ({label}) — it runs concurrently in the background. \
         You will be notified automatically, and its result will be delivered to you when it \
         finishes; continue with your other work — do not poll or wait. If you have nothing to \
         do until it finishes, tell the user in one line what it is doing and end your turn.{isolation}"
    ))
}

/// Best-effort size summary for a finished write task: file count and
/// insertions/deletions (`git diff --shortstat`, reformatted as `+ins -del`)
/// plus the commit subjects (`git log --oneline`) — the same two facts
/// `task_diff` would show, surfaced up front in the delivery message so the
/// parent knows the SCALE of the result before deciding how to review it. Run
/// from `repo` (the parent's cwd), like `task_diff`: worktrees share the
/// parent's object store, so `branch` is visible there too. `None` on any git
/// failure or when there is nothing to summarize (no commits) — this must
/// never fail or block a delivery, only enrich it.
pub(crate) async fn task_size_summary(repo: &std::path::Path, branch: &str) -> Option<String> {
    let shortstat_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--shortstat", &format!("HEAD...{branch}")])
        .output()
        .await
        .ok()?;
    let log_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--oneline", &format!("HEAD..{branch}")])
        .output()
        .await
        .ok()?;
    if !shortstat_out.status.success() || !log_out.status.success() {
        return None;
    }
    let shortstat = format_shortstat(&String::from_utf8_lossy(&shortstat_out.stdout));
    let commits = String::from_utf8_lossy(&log_out.stdout).trim().to_string();
    if shortstat.is_none() && commits.is_empty() {
        return None; // nothing landed — no summary worth showing
    }
    let mut out = String::new();
    if let Some(s) = shortstat {
        out.push_str(&format!("  size:     {s}\n"));
    }
    if !commits.is_empty() {
        out.push_str("  commits:\n");
        for line in commits.lines() {
            out.push_str(&format!("    {line}\n"));
        }
    }
    Some(out.trim_end().to_string())
}

/// Reformat `git diff --shortstat`'s output (`" 7 files changed, 182 \
/// insertions(+), 46 deletions(-)"`, with either count clause absent when
/// zero) into the delivery message's compact `"7 files changed, +182 -46"`.
/// `None` for empty input (a branch with commits that net no line changes,
/// e.g. a pure rename).
pub(crate) fn format_shortstat(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut parts = raw.split(',');
    let files = parts.next()?.trim().to_string();
    let mut insertions = 0u64;
    let mut deletions = 0u64;
    for clause in parts {
        let clause = clause.trim();
        let Some(n) = clause
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        if clause.contains("insertion") {
            insertions = n;
        } else if clause.contains("deletion") {
            deletions = n;
        }
    }
    Some(format!("{files}, +{insertions} -{deletions}"))
}

/// The shared, lazily-resolved sub-agent transcript directory cell (see
/// [`AgentConfig::subagent_transcript_dir`]).
pub(crate) type SubagentDirCell =
    Option<std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>;

/// Monotonic counter for sub-agent transcript file ids, shared by the blocking
/// and background spawn paths so ids are ordered and unique within a session
/// dir. Separate from `BG_SEQ`, which numbers background-task registry entries.
static SUBAGENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A transcript file id: `NNN-<slug>`, where `slug` is the sanitized label.
/// `seq` is the pre-fetched counter value.
pub(crate) fn subagent_transcript_id(seq: u64, label: &str) -> String {
    let lowered: String = label
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = lowered
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = if slug.is_empty() {
        "task".to_string()
    } else {
        slug.chars().take(32).collect()
    };
    format!("{seq:03}-{slug}")
}

/// Read the resolved transcript dir from the shared cell, if the feature is on
/// and a session id has been assigned.
pub(crate) fn resolve_subagent_dir(cell: &SubagentDirCell) -> Option<std::path::PathBuf> {
    cell.as_ref()?.lock().ok()?.clone()
}

/// How many ids to try before giving up on a transcript (best-effort — a run
/// must never fail because we could not name its log).
const SUBAGENT_ID_ATTEMPTS: u64 = 10_000;

/// Open a transcript for one run under `dir`, claiming the next free id.
///
/// The id counter restarts at 0 in every process while `dir` is keyed by session
/// id and survives a resume, so `NNN-<slug>` collides with a previous run's file
/// on the very first task after `/resume` (the default label is `sub-task`, so
/// this is the common case, not a corner). [`SubagentTranscript::create`] is
/// exclusive, so a taken id fails and we advance instead of appending a new run
/// onto an old run's log.
///
/// Shared by the blocking and background spawn paths so they cannot drift.
fn open_next_subagent_transcript(
    dir: &std::path::Path,
    label: &str,
) -> Option<subagent_transcript::SubagentTranscript> {
    open_next_subagent_transcript_from(&SUBAGENT_SEQ, dir, label)
}

/// Core of [`open_next_subagent_transcript`] with the id counter injected, so a
/// test can drive it from its own counter instead of poking the process-global
/// one (tests share a process and run in parallel).
pub(crate) fn open_next_subagent_transcript_from(
    seq_source: &std::sync::atomic::AtomicU64,
    dir: &std::path::Path,
    label: &str,
) -> Option<subagent_transcript::SubagentTranscript> {
    for _ in 0..SUBAGENT_ID_ATTEMPTS {
        let seq = seq_source.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = subagent_transcript_id(seq, label);
        match subagent_transcript::SubagentTranscript::create(dir, &id) {
            Ok(t) => return Some(t),
            // Taken by a previous run (or a concurrent spawn): try the next id.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // Anything else (unwritable dir, …) is not going to fix itself.
            Err(_) => return None,
        }
    }
    None
}

/// Map a live agent event to the transcript event to record, if any.
pub(crate) fn subagent_event_for(ev: &AgentEvent) -> Option<subagent_transcript::Event> {
    use subagent_transcript::Event;
    match ev {
        AgentEvent::Text(t) => Some(Event::Text { chunk: t.clone() }),
        AgentEvent::ToolStart { name, .. } => Some(Event::Tool { name: name.clone() }),
        _ => None,
    }
}

/// The context window a delegated sub-agent should run against, given the window
/// it would inherit from its parent and the sub-agent's own
/// `(provider, base_url, model)`.
///
/// The Codex endpoint is the only path this fix changes: its account catalog is
/// authoritative and per-model, so a Codex sub-agent ALWAYS re-derives and never
/// carries a wrong inherited preset — the reported overflow (a sub-agent told the
/// old 400k, or a repoint's 272k preset, for a 128k model).
///
/// Every other endpoint keeps the pre-existing behaviour: prefer `inherited`,
/// which may be the parent's endpoint-probed value (a local server's
/// `max_model_len` / `n_ctx`) or a user-configured window — both more exact for
/// this model than a generic catalog — and fall back to the catalog only to fill
/// a gap, never blinding the agent. (A stale `inherited` after a cross-provider
/// `/model` switch is a pre-existing, separately-tracked limitation; correcting it
/// needs the parent's live window published on the delegation runtime.)
pub(crate) fn subagent_context_window(
    inherited: Option<u32>,
    provider: Option<&str>,
    base_url: &str,
    model: &str,
) -> Option<u32> {
    if base_url == CHATGPT_CODEX_BASE_URL {
        return context_window_for(provider, base_url, model);
    }
    inherited.or_else(|| context_window_for(provider, base_url, model))
}

/// The opening usage counters for a delegated sub-agent — zeroed, but knowing
/// the context window it is working against.
///
/// The window is resolved the same way the agent resolves its own
/// (`Agent::ensure_context_window`): the config's, else per-model via
/// [`context_window_for`] (the ChatGPT account cache, or models.dev),
/// network-free. Without it a sub-agent's pane had a used-tokens count and no
/// maximum, so its gauge could not draw — it showed a bare number where the main
/// agent shows a bar.
pub(crate) fn subagent_usage(cfg: &AgentConfig) -> AgentUsage {
    AgentUsage {
        context_window: cfg.context_window.or_else(|| {
            context_window_for(
                Some(cfg.model.provider().as_str()),
                &cfg.base_url,
                cfg.model.model(),
            )
        }),
        ..Default::default()
    }
}

pub(crate) fn subagent_base_config(config: &AgentConfig) -> AgentConfig {
    let mut base = config.clone();
    base.subagents = false;
    base.mcp = Vec::new();
    // Sub-agents share the parent's language servers (`SubagentTool` hands
    // them its registry Arc) instead of spawning their own set — but still
    // register the LSP tools, which resolve the registry at call time.
    base.lsp = false;
    base.lsp_shared = true;
    // The unnamed default sub-agent runs the main prompt with the full tool set;
    // profiles opt into a persona / read-only scope via `config_for_agent_profile`.
    base.agent_prompt = None;
    base.allowed_tools = None;
    base.read_only = false;
    // Sub-agents never spawn sub-agents, so they never write transcripts.
    base.subagent_transcript_dir = None;
    // ── The session/sub-agent seam ──────────────────────────────────────────
    // A sub-agent is an agent. It keeps every capability the main agent has;
    // what it may *do* is bounded by its type and permissions (`read_only`,
    // `allowed_tools`), never by the mere fact that it was
    // delegated. Only genuinely structural limits live here:
    //   - it cannot delegate (recursion is bounded to one level), and so
    //   - it writes no sub-agent transcripts of its own.
    // Everything else — memory, compaction, guardrails, hooks, the cost ceiling
    // — is inherited, and the agent works with no UI attached.
    base.is_subagent = true;
    // The sub-agent model. A bare id is a model on the SAME provider — "Opus
    // drives, Sonnet implements", same endpoint, same key, same bill. A whole
    // `provider://model` moves the sub-agents to another provider, and the endpoint
    // (key, headers, api-version) has to follow it, or they would be sent to the
    // parent's endpoint under another provider's model id.
    // A bare `provider://` takes that provider's DECLARED model — the strict,
    // store-free policy, because a sub-agent's model is not an interactive choice.
    if let Some(spec) = &config.subagent_model
        && let Ok(reference) = strict_spec_ref(config, spec, &config.model)
    {
        let (key, url) = (base.api_key.clone(), base.base_url.clone());
        let parent = AuthContext {
            api_key: key.as_deref(),
            base_url: &url,
        };
        if apply_model_ref(&mut base, reference.clone(), Some(&parent)).is_err() {
            // An unresolvable provider is reported when a `task` actually spawns
            // (where there is somewhere to report it); the identity still stands.
            base.model = reference;
        }
    }
    base
}

/// Move `cfg` onto the identity `reference`: re-derive its endpoint, key,
/// api-version and headers from the provider that identity names, atomically with
/// the identity itself. Endpoint/identity only — does NOT touch persona or tool
/// scope, so it is safe to layer on top of an already-resolved agent profile.
///
/// `parent` is the key-inheritance context (see [`AuthContext`]); passing the
/// caller's own endpoint + key lets a same-endpoint child inherit the credential,
/// and the `same_endpoint` guard inside [`resolve_api_key`] is what stops that key
/// from leaking to a different provider's host.
///
/// The endpoint is re-derived ONLY when the provider changes — because it is a
/// property OF the provider, and a same-provider model change cannot have moved it.
/// (This is now a shortcut rather than a load-bearing rule: re-deriving it would
/// produce the same URL.)
pub(crate) fn apply_model_ref(
    cfg: &mut AgentConfig,
    reference: ModelRef,
    parent: Option<&AuthContext<'_>>,
) -> Result<()> {
    if reference.provider() == cfg.model.provider() {
        cfg.model = reference;
        return Ok(());
    }
    let name = reference.provider().as_str();
    let resolved = resolve(&reference, cfg, parent)?;
    // The provider's CONFIGURED window (a `[providers.*].context_window`, or the
    // ChatGPT preset floor) — a user override, so it outranks the derived one, and
    // it is applied only when the preset actually declares one: most built-ins
    // carry `None`, and overwriting an inherited (probed) window with `None` would
    // blind the agent to how full it is, silently disabling its own compaction.
    if let Some(w) = cfg.resolve_provider(name).and_then(|p| p.context_window) {
        cfg.context_window = Some(w);
    }
    cfg.base_url = resolved.base_url().to_string();
    cfg.api_key = resolved.api_key().map(str::to_string);
    cfg.api_version = resolved.api_version().map(str::to_string);
    cfg.headers = resolved.headers().to_vec();
    cfg.model = reference;
    Ok(())
}

/// The identity a **model spec** names, against the identity `cfg` is already on.
/// This is the **programmatic** entry point — agent profiles (`[[subagent]]`,
/// `agents/*.md`) and the `task` tool's `model` argument.
///
/// The three shapes a source can spell, and only these:
/// - `provider://model` → that exact identity ([`ModelSpec::Full`]);
/// - a bare `model` → [`ModelSpec::ModelOnly`]: same provider, new model;
/// - `provider://` (a provider, no model) → the model that provider itself
///   DECLARES, else an error. NEVER `cfg`'s current model id, which belongs to the
///   provider being left — that silent carry-over is the bug this whole seam
///   exists to kill.
///
/// Note what is deliberately absent: the interactive last-used store
/// ([`model_for_provider`]). A profile is configuration, so it must resolve the
/// same way for everyone — folding in "whatever a human last picked on that
/// provider" would make the same sub-agent run a different model on each
/// developer's machine and a third one in CI. The store is consulted only by the
/// interactive switches (`/login`, the `/model` picker) and by the startup launch
/// fallback, where carrying on with what you were using is precisely the intent.
pub(crate) fn named_spec_ref(cfg: &AgentConfig, spec: Option<&str>) -> Result<Option<ModelRef>> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let spec: ModelSpec = spec.parse()?;
    strict_spec_ref(cfg, &spec, &cfg.model).map(Some)
}

/// **THE PROGRAMMATIC POLICY** for a [`ModelSpec::ProviderOnly`]: the model that
/// provider itself DECLARES (`[providers.<name>].model`, or a built-in preset's),
/// else an error.
///
/// [`ModelSpec::apply`] answers `None` for that shape precisely so this choice has
/// to be made explicitly, here, by the paths that need a *reproducible* answer.
/// `base` supplies the provider for a bare model id, and nothing else — a
/// `provider://` spec never inherits `base`'s model, which belongs to the provider
/// being LEFT.
pub(crate) fn strict_spec_ref(
    cfg: &AgentConfig,
    spec: &ModelSpec,
    base: &ModelRef,
) -> Result<ModelRef> {
    if let Some(reference) = spec.apply(base) {
        return Ok(reference);
    }
    let ModelSpec::ProviderOnly(p) = spec else {
        unreachable!("apply() answers None only for ProviderOnly");
    };
    let declared = cfg
        .resolve_provider(p.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider '{p}' (built-ins: {}, or define [providers.{p}])",
                BUILTIN_PROVIDERS.join(", ")
            )
        })?
        .model;
    let Some(m) = declared else {
        bail!(
            "provider '{p}' needs a model — name one as '{p}://<model>' \
             (it declares no default)"
        );
    };
    Ok(ModelRef::new(p.clone(), &m)?)
}

/// Apply the `task` tool's ad-hoc `model` argument — a [`ModelSpec`] — on top of an
/// already-resolved config (post agent-profile). A bare model id overrides on the
/// provider in force; a `provider://model` (or a `provider://`, which takes the
/// provider's declared model) switches provider too, and that target is auth-gated
/// here — fail fast, before spawning.
pub(crate) fn apply_task_overrides(
    cfg: &mut AgentConfig,
    parent: &AgentConfig,
    spec: Option<&str>,
) -> Result<()> {
    // The identity this delegation runs on.
    //
    // A `task` must be REPRODUCIBLE. When it names a provider but no model, the
    // model comes from what the provider itself declares — never from the
    // interactive last-used store. Consulting that store would make the same
    // delegation resolve to a different model on a developer's machine than in CI,
    // depending on what a human last happened to pick. The last-used fallback is
    // for *interactive* switches (`/login`, the `/model` picker), where "carry on
    // with what I was using" is the whole point; a spawned sub-agent is not that.
    let reference = named_spec_ref(cfg, spec).map_err(|e| anyhow::anyhow!("task: {e:#}"))?;
    let Some(reference) = reference else {
        return Ok(());
    };
    // A change of PROVIDER is what needs gating: the sub-agent is about to be sent
    // to another endpoint, with another credential.
    let switching = reference.provider() != cfg.model.provider();
    if switching {
        let pname = reference.provider().as_str();
        let p = cfg.resolve_provider(pname).ok_or_else(|| {
            anyhow::anyhow!(
                "task: unknown provider '{pname}' (built-ins: {}, or define [providers.{pname}])",
                BUILTIN_PROVIDERS.join(", ")
            )
        })?;
        let current_auth = provider_auth_state(
            pname,
            &p,
            cfg.api_key.as_deref(),
            Some(cfg.base_url.as_str()),
        );
        let parent_auth = provider_auth_state(
            pname,
            &p,
            parent.api_key.as_deref(),
            Some(parent.base_url.as_str()),
        );
        if current_auth == ProviderAuthState::Missing && parent_auth == ProviderAuthState::Missing {
            // Only suggest an env var when the provider actually reads one;
            // key_env-less providers (chatgpt OAuth, a keyless [providers.*])
            // would be sent chasing a var that resolve_api_key never consults.
            let hint = match p.key_env.as_deref() {
                Some(env) => format!("set ${env}, or run /login"),
                None => format!(
                    "run /login, or add an `api_key`/`key_env` to a [providers.{pname}] entry"
                ),
            };
            bail!("task: provider '{pname}' is not configured — {hint}");
        }
    }
    // Key inheritance: the CHILD's own context first (it may already sit on this
    // endpoint), then the parent's. `AuthContext` carries the endpoint each key
    // belongs to, so `resolve_api_key`'s `same_endpoint` guard can refuse to hand
    // a credential to a different provider's host. Snapshotted (owned) because
    // `apply_model_ref` mutates the very config they borrow from.
    let (child_key, child_url) = (cfg.api_key.clone(), cfg.base_url.clone());
    let child_ctx = AuthContext {
        api_key: child_key.as_deref(),
        base_url: &child_url,
    };
    let parent_ctx = AuthContext {
        api_key: parent.api_key.as_deref(),
        base_url: parent.base_url.as_str(),
    };
    let inherited = resolve(&reference, cfg, Some(&child_ctx))
        .ok()
        .and_then(|r| r.api_key().map(str::to_string))
        .or_else(|| {
            resolve(&reference, cfg, Some(&parent_ctx))
                .ok()
                .and_then(|r| r.api_key().map(str::to_string))
        });
    apply_model_ref(cfg, reference, Some(&child_ctx))?;
    if switching {
        cfg.api_key = inherited;
    }
    Ok(())
}

/// Apply a named agent profile onto `base`: (if the profile names a provider)
/// switch the identity — endpoint, auth, headers, and `api-version` follow it — so
/// the agent can run on a **different provider**, then set the persona, tool
/// scope, and runtime knobs. Used both for delegated sub-agents (with a
/// [`subagent_base_config`] base) and for `--agent` primary mode (applied directly
/// onto the main config, keeping delegation + MCP).
pub fn config_for_agent_profile(
    base: &AgentConfig,
    profile: &SubagentProfile,
) -> Result<AgentConfig> {
    let mut cfg = base.clone();
    let spec = profile.model.as_ref().map(ModelSpec::to_string);
    if let Some(reference) = named_spec_ref(&cfg, spec.as_deref())? {
        // The profile's own endpoint inherits the parent's key only across the
        // SAME endpoint (`resolve_api_key`'s guard) — a profile naming another
        // provider must not be handed this one's credential. Snapshotted: the
        // apply below mutates the config these borrow from.
        let (key, url) = (cfg.api_key.clone(), cfg.base_url.clone());
        let parent_ctx = AuthContext {
            api_key: key.as_deref(),
            base_url: &url,
        };
        apply_model_ref(&mut cfg, reference, Some(&parent_ctx))?;
    }
    // Persona + tool scope: an explicit `tools` list wins; otherwise `read_only`
    // (resolved to the read-only tool set in `Agent::new`, which has the registry).
    cfg.agent_prompt = profile.prompt.clone();
    cfg.allowed_tools = profile.tools.clone();
    cfg.read_only = profile.is_read_only();
    // Per-agent runtime knobs, each inheriting the main agent's when omitted.
    if profile.temperature.is_some() {
        cfg.temperature = profile.temperature;
    }
    if profile.effort.is_some() {
        cfg.effort = profile.effort.clone();
    }
    if let Some(s) = profile.max_steps {
        cfg.max_steps = s;
    }
    Ok(cfg)
}

/// The `task` tool: delegate a self-contained sub-task to a fresh sub-agent that
/// has its own context and (optionally) a different model **or provider**. The
/// sub-agent runs to completion and its final text becomes the tool result; its
/// tool activity is streamed to the parent as live output.
pub(crate) struct SubagentTool {
    /// Base policy for derived sub-agents (endpoint/model are overlaid live).
    base: AgentConfig,
    runtime: SharedDelegationRuntime,
    /// Named provider+model profiles selectable via the `agent` argument.
    profiles: Vec<SubagentProfile>,
    /// Description string (leaked once at startup — lists the configured
    /// profiles so the model knows what it can delegate to).
    description: &'static str,
    /// Registry of background-task `JoinHandle`s, shared with the owning
    /// [`Agent`] so it can abort live tasks on `clear()` / session reset.
    pub(crate) bg_handles: BgHandles,
    /// Concurrency caps: `(read-only, write-capable)`.
    caps: (usize, usize),
    /// Slots held by the sub-agents running right now.
    pub(crate) slots: Arc<SubagentSlots>,
    /// The owning agent's session cost counter — every sub-agent spawned here
    /// adds its spend to it, so `/cost` and the `max_cost` budget see the
    /// whole tree, not just the main loop.
    cost_total: Arc<std::sync::Mutex<f64>>,
    /// The owning agent's "cost total is a floor" flag — a sub-agent that runs
    /// an unpriced call (with `allow_unpriced`) sets it, so the whole tree's
    /// reported total admits it excludes unpriced usage.
    cost_partial: Arc<std::sync::atomic::AtomicBool>,
    /// The owning agent's language servers, shared with every sub-agent (the
    /// base config has `lsp = false`, so none builds a registry of its own).
    lsp: Option<Arc<hrdr_tools::LspRegistry>>,
    /// The parent session's transcript dir cell (see
    /// [`AgentConfig::subagent_transcript_dir`]); read at spawn.
    transcript_dir: SubagentDirCell,
    /// Every sub-agent spawned here is registered so the frontend can steer it,
    /// display it, and drive further turns on it. See [`LiveSubagents`].
    live: LiveSubagents,
}

impl SubagentTool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        base: AgentConfig,
        runtime: SharedDelegationRuntime,
        profiles: Vec<SubagentProfile>,
        bg_handles: BgHandles,
        cost_total: Arc<std::sync::Mutex<f64>>,
        cost_partial: Arc<std::sync::atomic::AtomicBool>,
        lsp: Option<Arc<hrdr_tools::LspRegistry>>,
        transcript_dir: SubagentDirCell,
        live: LiveSubagents,
    ) -> Self {
        let caps = (base.max_readonly_subagents, base.max_write_subagents);
        let mut desc = String::from(
            "Delegate a self-contained sub-task to a fresh sub-agent with its own context. It \
             CANNOT see this conversation or anything you know — it gets only its system prompt \
             and the `prompt` you pass — so make `prompt` complete and standalone. Use it to \
             keep the main context clean: broad exploration, or a focused piece of \
             implementation. The sub-agent has the normal tools (read/write/edit/bash/grep/…) \
             but can't itself delegate. Every task runs in the **background**: this call returns \
             immediately with a task id and the sub-agent's result is delivered to you \
             automatically when it finishes — keep working, spawn more, or (if you can't proceed \
             until it's done) tell the user in one line what it's doing and end your turn. Never \
             poll or wait. Issue several `task` calls at once to run sub-agents in **parallel**. \
             A write-capable sub-agent runs in an isolated git worktree — when it reports back, \
             review its changes and merge them before they affect your working dir (if the \
             project is not a git repo it instead edits your dir directly, and only one write \
             sub-agent runs at a time); a read-only sub-agent shares your dir and changes \
             nothing. Run cheaper/faster work on another `model` (see the `model` parameter)",
        );
        if profiles.is_empty() {
            desc.push('.');
        } else {
            desc.push_str(
                ", or delegate to a specialized `agent`. **Proactively** reach for a matching \
                 agent when a sub-task fits its role (don't wait to be asked) — the ★ ones \
                 especially:\n",
            );
            for p in &profiles {
                // ONE key, so ONE label: `provider · model` for a whole identity, the
                // bare model id for a model on the provider in force, and nothing at
                // all when the profile names neither.
                let mut tags = match &p.model {
                    Some(ModelSpec::Full(r)) => format!("{} · {}", r.provider(), r.model()),
                    Some(ModelSpec::ModelOnly(m)) => m.clone(),
                    // The provider, at whatever model it declares — resolved when the
                    // sub-agent actually spawns, so the label names the provider only.
                    Some(ModelSpec::ProviderOnly(p)) => p.to_string(),
                    None => "main provider".to_string(),
                };
                if p.is_read_only() {
                    tags.push_str(" · read-only");
                }
                let star = if p.is_proactive() { "★ " } else { "" };
                desc.push_str(&format!("- {star}{} ({tags})", p.name));
                if let Some(d) = &p.description {
                    desc.push_str(&format!(" — {d}"));
                }
                desc.push('\n');
            }
        }
        Self {
            base,
            runtime,
            profiles,
            description: Box::leak(desc.into_boxed_str()),
            bg_handles,
            caps,
            slots: Arc::new(SubagentSlots::default()),
            cost_total,
            cost_partial,
            lsp,
            transcript_dir,
            live,
        }
    }
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> serde_json::Value {
        let mut props = serde_json::json!({
            "description": {
                "type": "string",
                "description": "A 3-6 word label for the sub-task (shown to the user)."
            },
            "prompt": {
                "type": "string",
                "description": "The complete, standalone task for the sub-agent: what to do and exactly what to report back."
            },
            "model": {
                "type": "string",
                "description": "Optional model override, named as `provider://model` or as a bare model id. A bare id (`gpt-5.5-mini`, `deepseek/deepseek-chat`) is that model on the provider you are already on. A `provider://model` (`openrouter://deepseek/deepseek-chat`) also switches the provider — it must be one that is configured and authenticated (a built-in name or a [providers.*] entry); `provider://` on its own uses that provider's configured default model. Defaults to the profile's / configured subagent model, else the main model."
            }
        });
        if !self.profiles.is_empty() {
            let names: Vec<&str> = self.profiles.iter().map(|p| p.name.as_str()).collect();
            props["agent"] = serde_json::json!({
                "type": "string",
                "enum": names,
                "description": "Optional named sub-agent profile (see this tool's description) — runs on that profile's provider + model."
            });
        }
        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": ["prompt"]
        })
    }

    fn read_only(&self) -> bool {
        false
    }

    // Each sub-agent runs in its own isolated context, so multiple `task` calls
    // in one turn run concurrently (parallel exploration/implementation).
    fn concurrent(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("task needs a non-empty `prompt` argument"))?
            .to_string();

        let mut cfg = self.base.clone();
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        // The parent's LIVE resolved endpoint, whole — identity, endpoint, key,
        // api-version and headers together, exactly as the parent resolved them.
        // Overlaying them one at a time is what let a sub-agent end up on one
        // provider's endpoint with another's model.
        let live = runtime.endpoint.resolved;
        cfg.base_url = live.base_url().to_string();
        cfg.api_key = live.api_key().map(str::to_string);
        cfg.api_version = live.api_version().map(str::to_string);
        cfg.headers = live.headers().to_vec();
        cfg.model = live.reference().clone();
        cfg.effort = runtime.endpoint.effort;
        // The parent's *live* endpoint + key, captured before the configured
        // sub-agent model or an agent profile can repoint `cfg` away from it. This —
        // not `self.base` — is the context an ad-hoc provider switch inherits auth
        // from. `self.base` names the endpoint the session *launched* on, and a
        // `/model` switch since then would leave the gate judging a provider against
        // an endpoint the session left long ago: an ad-hoc delegation back to the
        // provider you are currently using could be rejected as "not configured".
        let live_parent = cfg.clone();
        // The configured sub-agent model (`--subagent-model` / `subagent_model`): a
        // bare id rides on the parent's PROVIDER and never changes which endpoint the
        // request is sent to; a whole `provider://model` moves the endpoint with it.
        if let Some(spec) = &runtime.explicit_subagent_model {
            // Strict, store-free: a `provider://` takes that provider's declared
            // model, or the delegation fails — it never takes whatever a human last
            // picked there, which would make this `task` run a different model on
            // every machine.
            let reference = strict_spec_ref(&cfg, spec, live.reference())?;
            let parent_ctx = AuthContext {
                api_key: live.api_key(),
                base_url: live.base_url(),
            };
            apply_model_ref(&mut cfg, reference, Some(&parent_ctx))?;
        }

        if let Some(name) = args.get("agent").and_then(|v| v.as_str())
            && !name.trim().is_empty()
        {
            let profile = self
                .profiles
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
                .ok_or_else(|| {
                    let known: Vec<&str> = self.profiles.iter().map(|p| p.name.as_str()).collect();
                    anyhow::anyhow!(
                        "unknown subagent '{name}' (configured: {})",
                        known.join(", ")
                    )
                })?;
            // No `last_model_on` escape here, deliberately: a profile-driven
            // delegation is as programmatic as a `task` arg, so its model must come
            // from the profile, the `task` call, or the provider's own default —
            // never from the interactive last-used store, which would make the same
            // sub-agent run a different model for each developer.
            //
            // Worktree isolation is applied to *every* write-capable sub-agent
            // below, by capability — there is no per-profile opt-in/out.
            cfg = config_for_agent_profile(&cfg, profile)
                .map_err(|e| anyhow::anyhow!("subagent '{}': {e:#}", profile.name))?;
        }
        cfg.cwd = ctx.cwd.clone();
        // ONE argument for the one identity: a bare model id (same provider) or a
        // whole `provider://model`.
        let model_arg = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        apply_task_overrides(&mut cfg, &live_parent, model_arg)?;
        if cfg.has_default_model() {
            bail!(
                "no model configured — set `model` in config.toml, $HRDR_MODEL, or pass \
                 `--model` / `--subagent-model` on the CLI"
            );
        }
        // Resolve the window for the sub-agent's OWN (endpoint, model) now that both
        // are final (endpoint overlay, profile, and task overrides all applied). The
        // value inherited from the parent describes the parent's model/provider;
        // carrying it onto a different one is the overflow bug (e.g. a ChatGPT
        // parent's window following a plain delegation onto a smaller model). Runs
        // before both the background and blocking spawns below.
        cfg.context_window = subagent_context_window(
            cfg.context_window,
            Some(cfg.model.provider().as_str()),
            &cfg.base_url,
            cfg.model.model(),
        );
        let label = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("sub-task")
            .to_string();

        // Every task runs **detached**: spawn and return immediately so the
        // sub-agent never blocks the main conversation. The run loop delivers its
        // result when it lands (the frontend shows live progress). There is no
        // foreground mode — if the parent needs the answer before its next step it
        // acknowledges the task and ends its turn; it is woken on completion.
        //
        // Isolation is decided by capability, not a flag. A write-capable
        // sub-agent runs in its OWN git worktree, so concurrent writers never step
        // on each other or on the main tree. When the project is NOT a git repo,
        // there are no worktrees to hand out, so a writer falls back to sharing the
        // main dir — and then only ONE write-capable sub-agent may run at a time,
        // or two of them would race on the same files. A read-only sub-agent
        // always shares the dir; it changes nothing, so there is nothing to race.
        let write_capable = !cfg.read_only;
        let worktrees_available = write_capable && in_git_repo(&ctx.cwd);

        // Bound how many run at once. Read-only agents get the higher cap. A
        // worktree-isolated writer gets the write cap; a shared-dir writer (no git
        // repo) is limited to one at a time so concurrent writers can't collide.
        let (max_readonly, max_write) = self.caps;
        let cap = match (write_capable, worktrees_available) {
            (false, _) => max_readonly,
            (true, true) => max_write,
            (true, false) => 1,
        };
        let kind = if write_capable {
            "write-capable"
        } else {
            "read-only"
        };
        let Some(slot) = self.slots.acquire(write_capable, cap) else {
            let hint = if write_capable && !worktrees_available {
                " (this directory is not a git repo, so write sub-agents run one at a time)"
            } else {
                ""
            };
            bail!(
                "too many sub-agents: {} {kind} already running (limit {cap}){hint}. Wait for one \
                 to finish — you are notified automatically — then try again, or run this work \
                 yourself.",
                self.slots.live(write_capable),
            );
        };

        let worktree = if worktrees_available {
            // Isolate the writer in its own worktree; the parent reviews and merges
            // it when the sub-agent reports back.
            let wt = Worktree::create(&ctx.cwd)
                .await
                .context("creating an isolated worktree for a write-capable sub-agent")?;
            cfg.cwd = wt.path.clone();
            ctx.emit(format!("  · isolated worktree: {}\n", wt.path.display()));
            Some(wt)
        } else {
            // Read-only, or a writer with no git repo to isolate into: share the
            // main dir. A shared-dir writer's edits land directly in the working
            // dir (serialized by the cap of 1 above), so there is no worktree to
            // review — the changes are simply already there.
            if write_capable {
                ctx.emit(
                    "  · no git repo — sub-agent works directly in the working dir\n".to_string(),
                );
            }
            None
        };

        spawn_background(
            cfg,
            prompt,
            label,
            ctx.call_id.clone(),
            slot,
            &ctx.background_tasks,
            &self.bg_handles,
            Arc::clone(&self.cost_total),
            Arc::clone(&self.cost_partial),
            self.lsp.clone(),
            self.transcript_dir.clone(),
            self.live.clone(),
            worktree,
        )
    }
}

/// Render a background sub-agent's live event log into a compact, human-readable
/// peek (for `task_output`): assistant text verbatim, tool activity as one-line
/// markers. Bulky/structural events (usage, history, token deltas) are skipped.
fn render_events_peek(events: &[AgentEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        match ev {
            AgentEvent::Text(t) => out.push_str(t),
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolStart { name, .. } => out.push_str(&format!("\n· {name}")),
            AgentEvent::ToolEnd { name, ok, .. } => {
                out.push_str(&format!(" {} {name}\n", if *ok { "✓" } else { "✗" }))
            }
            AgentEvent::Notice(n) => out.push_str(&format!("\n[{n}]\n")),
            AgentEvent::Steered(s) => out.push_str(&format!("\n» {s}\n")),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Compact human duration for `task_list`: `8s`, `3m12s`, `1h4m`.
fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// `task_list`: report the background sub-agents `task` spawned — id, label,
/// status, model, elapsed, and (for a write-capable one) its worktree — so the
/// parent can check on them without waiting.
pub(crate) struct TaskListTool;

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskListTool {
    fn name(&self) -> &'static str {
        "task_list"
    }
    fn description(&self) -> &'static str {
        "List the background sub-agents you started with `task`: each one's id, label, status \
         (running / done / cancelled) and — for a write-capable sub-agent — the isolated git \
         worktree its changes are in. A finished task's result is delivered to you automatically; \
         use this only to check progress, not to collect results."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let rows: Vec<String> = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            v.iter()
                .map(|t| {
                    let mut row = format!("#{} [{}] {}", t.id, t.status().as_str(), t.label);
                    if !t.model.is_empty() {
                        row.push_str(&format!("  model: {}", t.model));
                    }
                    if let Some(started) = t.started {
                        row.push_str(&format!("  {}", fmt_elapsed(started.elapsed())));
                    }
                    if let Some(wt) = &t.worktree {
                        row.push_str(&format!("  worktree: {}", wt.display()));
                    }
                    row
                })
                .collect()
        };
        if rows.is_empty() {
            return Ok("No background tasks.".to_string());
        }
        Ok(hrdr_tools::truncate(&rows.join("\n"), ctx.max_output))
    }
}

/// `task_output`: peek a background sub-agent's live progress without waiting.
pub(crate) struct TaskOutputTool {
    pub(crate) live: LiveSubagents,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskOutputTool {
    fn name(&self) -> &'static str {
        "task_output"
    }
    fn description(&self) -> &'static str {
        "Peek the current progress of a background sub-agent by its `id` (from `task_list`), \
         without blocking. Returns a snapshot of its output so far — useful if the user asks how \
         a task is going. The final result is still delivered to you automatically when it \
         finishes; you do not need to poll."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| {
            anyhow::anyhow!("task_output needs an integer `id` (see `task_list`)")
        })?;
        // Prefer the live event log; fall back to the registry entry's stored
        // result if the task already finished and its live entry was pruned.
        let peek = self.live.with(|v| {
            v.iter().find(|e| e.bg_id == Some(id)).map(|e| {
                let events = e
                    .events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .since(0)
                    .0;
                render_events_peek(&events)
            })
        });
        if let Some(text) = peek.filter(|t| !t.is_empty()) {
            // Middle-truncate, not head-truncate: this is a *peek at a
            // still-running* task, so the newest output (the tail) is exactly
            // its current progress — head-only truncation would cut that and
            // keep only stale narration from the start of the run.
            return Ok(hrdr_tools::truncate_middle(&text, ctx.max_output));
        }
        let done = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            v.iter().find(|t| t.id == id).map(|t| {
                let body = t
                    .result
                    .clone()
                    .unwrap_or_else(|| format!("(task #{id} is {})", t.status().as_str()));
                (body, t.transcript.clone())
            })
        };
        match done {
            Some((text, transcript)) => {
                // Same reasoning as the live-peek branch above: keep the tail.
                let mut out = hrdr_tools::truncate_middle(&text, ctx.max_output);
                // Point at the durable transcript for the full run — richer than
                // the stored summary, and it outlives the live event log.
                if let Some(p) = transcript {
                    out.push_str(&format!(
                        "\n\n(full transcript: {} — `read` it for the complete run)",
                        p.display()
                    ));
                }
                Ok(out)
            }
            None => anyhow::bail!("no background task #{id} (see `task_list`)"),
        }
    }
}

/// `task_steer`: add instructions to a background sub-agent's in-flight turn.
pub(crate) struct SteerTool {
    pub(crate) live: LiveSubagents,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for SteerTool {
    fn name(&self) -> &'static str {
        "task_steer"
    }
    fn description(&self) -> &'static str {
        "Give additional instructions to a running background sub-agent. The message is queued \
         on the sub-agent's active turn and reaches it before its next model request; if its current \
         response finishes first, the retained sub-agent starts a follow-up turn with the message. \
         Use the task id from `task` / `task_list`; finished or unknown tasks cannot be steered."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The running task id (see `task_list`)." },
                "prompt": { "type": "string", "description": "Additional instructions for the sub-agent." }
            },
            "required": ["id", "prompt"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("task_steer needs an integer `id` (see `task_list`)"))?;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("task_steer needs a non-empty `prompt`"))?;
        let queued = self.live.with(|entries| {
            let entry = entries.iter().find(|e| e.bg_id == Some(id))?;
            if !entry.running {
                return Some(false);
            }
            entry
                .steering
                .lock()
                .ok()
                .map(|mut queue| queue.push_back(Steer::plain(prompt)))?;
            Some(true)
        });
        match queued {
            Some(true) => Ok(format!("Steered background task #{id}.")),
            Some(false) => anyhow::bail!("background task #{id} is no longer running"),
            None => anyhow::bail!("no running background task #{id} (see `task_list`)"),
        }
    }
}

/// `task_cancel`: abort one background sub-agent and discard its (unreviewed)
/// worktree.
pub(crate) struct TaskCancelTool {
    pub(crate) bg_handles: BgHandles,
    pub(crate) live: LiveSubagents,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskCancelTool {
    fn name(&self) -> &'static str {
        "task_cancel"
    }
    fn description(&self) -> &'static str {
        "Cancel a running background sub-agent by its `id` (from `task_list`). Its work is \
         abandoned. If it was write-capable and its isolated worktree has no changes, the worktree \
         is removed; if it has changes — uncommitted files OR commits the sub-agent made on its \
         branch — they are NOT thrown away: the worktree is kept and this call tells you where it \
         is so you can review and merge or discard it. Use when the user asks to stop a task or it \
         is no longer needed."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| {
            anyhow::anyhow!("task_cancel needs an integer `id` (see `task_list`)")
        })?;
        // Abort the worker if it is still running, and AWAIT the aborted task so
        // its future is fully dropped before we touch the worktree — otherwise the
        // worker could still be mid-write when we assess and remove it. Bounded so
        // a wedged task can't hang the cancel; abort resolves promptly for the
        // I/O-bound sub-agent in the common case.
        let handle = {
            let mut handles = self
                .bg_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handles
                .iter()
                .position(|(hid, _)| *hid == id)
                .map(|pos| handles.remove(pos).1)
        };
        let aborted = handle.is_some();
        if let Some(h) = handle {
            h.abort();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), h).await;
        }
        // Mark the registry entry cancelled and take its worktree for discard.
        let worktree = {
            let mut v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match v.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    t.cancelled = true;
                    t.done = true;
                    t.worktree.clone().zip(t.branch.clone())
                }
                None if !aborted => anyhow::bail!("no background task #{id} (see `task_list`)"),
                None => None,
            }
        };
        // A worktree with uncommitted work is NOT thrown away: keep it and tell
        // the caller where it is, so the (possibly partial) changes aren't lost.
        // Only a clean worktree is removed.
        let kept_note = match worktree {
            Some((path, branch)) => {
                if worktree_has_changes(&ctx.cwd, &path, &branch).await {
                    Some(format!(
                        " Its worktree has changes (uncommitted files or commits on its branch), \
                         so it was kept for you to review — review (`git -C {} diff` and `git -C \
                         {} log`) and merge or discard it yourself:\n  worktree: {}\n  branch:   \
                         {branch}",
                        path.display(),
                        path.display(),
                        path.display(),
                    ))
                } else {
                    remove_worktree(&ctx.cwd, &path, &branch);
                    None
                }
            }
            None => None,
        };
        // Clear its live panel entry.
        self.live.with(|v| {
            for e in v.iter_mut().filter(|e| e.bg_id == Some(id)) {
                e.running = false;
                e.done = true;
                e.delivered = true;
            }
        });
        Ok(format!(
            "Cancelled background task #{id}.{}",
            kept_note.unwrap_or_default()
        ))
    }
}

/// `task_cleanup`: after the parent has merged a finished write sub-agent's work
/// into its own working dir, remove that worktree and drop the task. This is the
/// explicit "merged, done with it" signal — it does NOT perform the merge.
pub(crate) struct TaskCleanupTool {
    pub(crate) live: LiveSubagents,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskCleanupTool {
    fn name(&self) -> &'static str {
        "task_cleanup"
    }
    fn description(&self) -> &'static str {
        "Remove a finished write sub-agent's worktree once you have merged its work into your \
         own working dir. Use it as the 'I'm done with this one' signal AFTER you have brought \
         the changes over (via git merge / cherry-pick / apply — this tool does NOT merge for \
         you). Pass the task `id` (from `task_list`). It refuses if the worktree still has \
         uncommitted changes, or if its branch has commits not yet reachable from your HEAD (it \
         looks unmerged) — so you can't delete work by accident. If you DID bring those commits \
         over by cherry-pick or squash (so the originals aren't reachable), pass `force: true` to \
         confirm and remove anyway."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." },
                "force": {
                    "type": "boolean",
                    "description": "Remove even if the branch has commits not reachable from HEAD — confirm you already merged them (e.g. cherry-pick / squash). Default false."
                }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| {
            anyhow::anyhow!("task_cleanup needs an integer `id` (see `task_list`)")
        })?;
        // Look up the task's worktree. The entry persists after delivery exactly
        // so this is addressable by id.
        let worktree = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match v.iter().find(|t| t.id == id) {
                Some(t) => t.worktree.clone().zip(t.branch.clone()),
                None => anyhow::bail!("no background task #{id} (see `task_list`)"),
            }
        };
        let Some((path, branch)) = worktree else {
            // No worktree (a read-only or shared-dir task) — nothing to remove;
            // just drop the entry.
            ctx.background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|t| t.id != id);
            return Ok(format!("Task #{id} has no worktree to clean up."));
        };
        // Refuse while the working tree is dirty — uncommitted changes are
        // definitely NOT merged, so removing the worktree would lose them.
        let dirty = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["status", "--porcelain"])
            .output()
            .await
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(true);
        if dirty {
            anyhow::bail!(
                "worktree for task #{id} has uncommitted changes ({}) — commit or bring them \
                 over first, then clean up. Nothing was removed.",
                path.display()
            );
        }
        // Guard against removing work that was never merged. Count the branch's
        // commits not reachable from HEAD; if any, the branch looks unmerged, so
        // refuse unless `force` (the parent brought them over via cherry-pick /
        // squash, which leaves the originals unreachable). Biased to refuse: a
        // failed count is treated as "unmerged" so nothing is deleted on a bad read.
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        if !force {
            let unmerged = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&ctx.cwd)
                .args(["rev-list", "--count", &format!("HEAD..{branch}")])
                .output()
                .await
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "?".to_string());
            if unmerged != "0" {
                anyhow::bail!(
                    "branch `{branch}` (task #{id}) has {unmerged} commit(s) not reachable from \
                     your HEAD — it looks unmerged. Merge it first, or if you already brought the \
                     work over (cherry-pick / squash) call task_cleanup again with force:true. \
                     Nothing was removed."
                );
            }
        }
        remove_worktree(&ctx.cwd, &path, &branch);
        ctx.background_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|t| t.id != id);
        self.live.with(|v| {
            for e in v.iter_mut().filter(|e| e.bg_id == Some(id)) {
                e.running = false;
                e.done = true;
                e.delivered = true;
            }
        });
        Ok(format!("Cleaned up the worktree for task #{id}."))
    }
}

/// `task_diff`: review a finished write sub-agent's work in one call, instead of
/// the parent hand-rolling the 3-command git recipe (`status --porcelain`,
/// `log --oneline HEAD..<branch>`, `diff HEAD...<branch>`) every time.
pub(crate) struct TaskDiffTool;

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskDiffTool {
    fn name(&self) -> &'static str {
        "task_diff"
    }
    fn description(&self) -> &'static str {
        "Review a finished write sub-agent's work: any uncommitted/untracked leftovers in its \
         worktree, its commits, and either the full merge-base diff (`git diff \
         HEAD...<branch>`) or — pass `commit` — just one commit's diff (`git show`), for \
         reviewing a large result commit-by-commit instead of one unreviewable blob. Use it when \
         a write task reports back, before you merge its work into your own working dir. Pass \
         the task `id` (from `task_list` or the delivery message)."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." },
                "commit": {
                    "type": "string",
                    "description": "Optional: review just one commit instead of the full diff. \
                        Either a 1-based index into the commit list this tool prints (newest \
                        first — so you can read the list, then pass e.g. \"2\"), or a git commit \
                        hash. Must be one of this task's own commits (HEAD..<branch>); a rev \
                        outside the task is refused."
                }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("task_diff needs an integer `id` (see `task_list`)"))?;
        // Same lookup as `task_cleanup`: the entry persists after delivery, so
        // this is addressable by id whether or not the task has been reviewed yet.
        let worktree = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match v.iter().find(|t| t.id == id) {
                Some(t) => t.worktree.clone().zip(t.branch.clone()),
                None => anyhow::bail!("no background task #{id} (see `task_list`)"),
            }
        };
        let Some((path, branch)) = worktree else {
            anyhow::bail!(
                "task #{id} has no changes to diff — it was read-only or shared your working \
                 dir, so nothing landed in an isolated worktree."
            );
        };

        // 1. Uncommitted/untracked leftovers in the worktree itself.
        let status_out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["status", "--porcelain"])
            .output()
            .await
            .context("running `git status` in the task's worktree")?;
        let dirty = String::from_utf8_lossy(&status_out.stdout)
            .trim()
            .to_string();

        // 2. The commits under review, run against the PARENT's cwd (the branch
        // is visible there too — worktrees share one object store). Also the
        // list `commit` indexes into below, so its numbering matches what the
        // model just read.
        let log_out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&ctx.cwd)
            .args(["log", "--oneline", &format!("HEAD..{branch}")])
            .output()
            .await
            .context("running `git log` for the task's branch")?;
        let commits = String::from_utf8_lossy(&log_out.stdout).trim().to_string();

        let mut report = String::new();
        if !dirty.is_empty() {
            report.push_str(&format!(
                "WARNING: the sub-agent left uncommitted/untracked changes in its worktree — \
                 these are NOT included in the diff below and must be handled (reviewed and \
                 committed, or discarded) before you merge or run `task_cleanup`:\n{dirty}\n\n"
            ));
        }
        if commits.is_empty() {
            report.push_str(&format!(
                "Commits: branch `{branch}` has no commits beyond your HEAD — nothing to \
                 merge.\n\n"
            ));
        } else {
            report.push_str(&format!("Commits:\n{commits}\n\n"));
        }

        let commit_arg = args
            .get("commit")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match commit_arg {
            // 3a. One commit only: resolve the selector against THIS task's
            // commits (never arbitrary repo history), then `git show` it — a
            // reviewable slice of a result too big for the full diff.
            Some(selector) => {
                let (hash, index, total) =
                    resolve_task_commit(ctx, id, &branch, &commits, selector).await?;
                let show_out = tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&ctx.cwd)
                    .args(["show", &hash])
                    .output()
                    .await
                    .context("running `git show` for the selected commit")?;
                let diff =
                    hrdr_tools::redact_secret_diffs(&String::from_utf8_lossy(&show_out.stdout));
                report.push_str(&format!(
                    "Showing commit {index}/{total} (`git show {hash}`):\n"
                ));
                report.push_str(if diff.trim().is_empty() {
                    "(no diff)\n"
                } else {
                    &diff
                });
            }
            // 3b. The full merge-base diff, also from the parent's cwd (three-dot:
            // everything since the merge-base, not just what's uncommitted in the
            // worktree — which is clean once the sub-agent has committed).
            None => {
                let diff_out = tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&ctx.cwd)
                    .args(["diff", &format!("HEAD...{branch}")])
                    .output()
                    .await
                    .context("running `git diff` against the task's branch")?;
                let diff =
                    hrdr_tools::redact_secret_diffs(&String::from_utf8_lossy(&diff_out.stdout));
                report.push_str(&format!("Diff (git diff HEAD...{branch}):\n"));
                report.push_str(if diff.trim().is_empty() {
                    "(no diff)\n"
                } else {
                    &diff
                });
            }
        }

        Ok(hrdr_tools::truncate_saved(
            &report,
            ctx.max_output,
            ctx.max_output_lines,
            hrdr_tools::TruncateSide::Head,
            "task_diff",
        ))
    }
}

/// Resolve a `task_diff` `commit` selector to `(hash, 1-based index, total)`
/// within task `id`'s own commits — never arbitrary repo history. `commits` is
/// the trimmed `git log --oneline HEAD..<branch>` output already computed by
/// the caller (newest first), so a numeric `selector` indexes into exactly the
/// list the tool just printed. Anything else is a git rev, verified to
/// actually be one of `HEAD..<branch>` via `git rev-list` before use — this
/// is what stops the model from pulling up a diff from outside the task.
///
/// A selector can be BOTH: an abbreviated git hash may be all digits
/// (`1234567`). Only a number that fits the printed list (`1..=total`) is an
/// index — anything larger falls through to rev resolution, so an all-digit
/// hash resolves as the hash it is instead of erroring as an out-of-range
/// index. (An all-digit hash abbreviation short enough to also be a valid
/// index reads as the index — same as what the model just saw printed.)
async fn resolve_task_commit(
    ctx: &hrdr_tools::ToolContext,
    id: u64,
    branch: &str,
    commits: &str,
    selector: &str,
) -> anyhow::Result<(String, usize, usize)> {
    let lines: Vec<&str> = commits.lines().collect();
    let total = lines.len();
    let as_index = selector.parse::<usize>().ok();
    if let Some(n) = as_index.filter(|n| (1..=total).contains(n)) {
        let hash = lines[n - 1]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        return Ok((hash, n, total));
    }
    // Not a usable index — a git rev. Resolve it to a full hash, then require
    // it actually be reachable as one of the task's own commits before showing
    // it.
    let rev_parse = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&ctx.cwd)
        .args(["rev-parse", "--verify", &format!("{selector}^{{commit}}")])
        .output()
        .await
        .context("resolving `commit` as a git rev")?;
    if !rev_parse.status.success() {
        // A short number that isn't a rev either was almost certainly meant as
        // an index into the printed list — say so instead of "not a rev".
        if as_index.is_some() && selector.len() < 4 {
            anyhow::bail!(
                "task #{id} commit index {selector} is out of range — its commit list has \
                 {total} commit(s) (see the `Commits:` list `task_diff` just printed; it's \
                 1-based, newest first)"
            );
        }
        anyhow::bail!("`{selector}` is not a valid git commit rev");
    }
    let hash = String::from_utf8_lossy(&rev_parse.stdout)
        .trim()
        .to_string();
    let rev_list_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&ctx.cwd)
        .args(["rev-list", &format!("HEAD..{branch}")])
        .output()
        .await
        .context("listing task's commits to verify `commit` belongs to it")?;
    let rev_list = String::from_utf8_lossy(&rev_list_out.stdout);
    let Some(pos) = rev_list.lines().position(|l| l == hash) else {
        anyhow::bail!(
            "`{selector}` is not one of task #{id}'s commits (`git rev-list HEAD..{branch}`) — \
             refusing to show history outside this task"
        );
    };
    Ok((hash, pos + 1, total))
}

/// The full agent-profile set for `config`, layered by precedence — each source
/// overriding a same-named agent from the one before it:
/// built-ins < discovered files (`.claude`/`.opencode`/`.hrdr`) < `[[subagent]]`
/// config. Used both to populate the `task` tool and to resolve `--agent`.
///
/// Discovered profiles are **untrusted, repo-local** content — arbitrary
/// `.claude`/`.opencode`/`.hrdr` Markdown files that ship inside a cloned repo,
/// as opposed to `[[subagent]]` config, which is the user's own trusted config
/// file. Two trust-boundary rules apply only to discovered profiles:
/// - a discovered profile can never overlay a built-in's name (`explore`,
///   `review`, `plan`, `general`) — the built-in always wins, so a malicious
///   repo can't silently swap out `explore`'s instructions. The collision is
///   logged (to stderr; profile resolution runs before this agent has an event
///   channel to post an [`AgentEvent::Notice`] on) and the file is otherwise
///   ignored;
/// - a discovered profile can never set `proactive` (which nudges the main
///   agent to delegate to it **unprompted**) — it's forced to `false` even for
///   a non-colliding name, since prompting the model to reach for
///   attacker-controlled instructions without being asked is itself the risk.
pub fn resolve_agent_profiles(config: &AgentConfig) -> Result<Vec<SubagentProfile>> {
    // Field-level merge: when `incoming` names an existing profile, each field it
    // leaves unset (`None`) inherits the one already in the slot, so pinning e.g.
    // just `model` on a built-in doesn't blow away its prompt/read_only/description.
    // A non-matching name is pushed whole, as a brand-new profile. `name` keeps the
    // existing slot's casing.
    fn overlay(profiles: &mut Vec<SubagentProfile>, incoming: SubagentProfile) {
        match profiles
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&incoming.name))
        {
            Some(slot) => {
                let SubagentProfile {
                    name: _,
                    model,
                    description,
                    prompt,
                    read_only,
                    tools,
                    temperature,
                    effort,
                    max_steps,
                    proactive,
                } = incoming;
                if model.is_some() {
                    slot.model = model;
                }
                if description.is_some() {
                    slot.description = description;
                }
                if prompt.is_some() {
                    slot.prompt = prompt;
                }
                if read_only.is_some() {
                    slot.read_only = read_only;
                }
                if tools.is_some() {
                    slot.tools = tools;
                }
                if temperature.is_some() {
                    slot.temperature = temperature;
                }
                if effort.is_some() {
                    slot.effort = effort;
                }
                if max_steps.is_some() {
                    slot.max_steps = max_steps;
                }
                if proactive.is_some() {
                    slot.proactive = proactive;
                }
            }
            None => profiles.push(incoming),
        }
    }
    let mut profiles = builtin_subagent_profiles();
    let builtin_names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
    for mut p in discover_agent_profiles(&config.cwd)? {
        if builtin_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&p.name))
        {
            eprintln!(
                "hrdr: ignoring repo-local agent profile '{}' from {:?} — it collides with a \
                 built-in agent name; built-ins cannot be overridden by discovered files",
                p.name, config.cwd
            );
            continue;
        }
        p.proactive = Some(false);
        overlay(&mut profiles, p);
    }
    for up in config.subagent_profiles.clone() {
        overlay(&mut profiles, up);
    }
    Ok(profiles)
}

/// The always-available built-in sub-agents: read-only `explore` and `review`
/// personas. Merged with the user's `[[subagent]]` profiles in [`Agent::new`]
/// (a user profile of the same name overrides the built-in).
pub fn builtin_subagent_profiles() -> Vec<SubagentProfile> {
    vec![
        SubagentProfile {
            name: "explore".to_string(),
            model: None,
            description: Some(
                "Read-only codebase investigator — trace files, types, and call \
                 paths and report back. Use proactively when a question needs \
                 broad exploration, to keep the main context lean."
                    .to_string(),
            ),
            prompt: Some(EXPLORE_PROMPT.to_string()),
            read_only: Some(true),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(true),
        },
        SubagentProfile {
            name: "review".to_string(),
            model: None,
            description: Some(
                "Read-only code reviewer — audit code or a change for bugs, edge \
                 cases, and security issues. Use proactively after writing or \
                 changing non-trivial code, before finalizing."
                    .to_string(),
            ),
            prompt: Some(REVIEW_PROMPT.to_string()),
            read_only: Some(true),
            tools: None,
            temperature: None,
            // A careful reviewer default: think harder before flagging.
            effort: Some("high".to_string()),
            max_steps: None,
            proactive: Some(true),
        },
        SubagentProfile {
            name: "plan".to_string(),
            model: None,
            description: Some(
                "Planner — investigates read-only and returns a concrete, \
                 step-by-step implementation plan in its report. Changes nothing; \
                 use it to design the work before delegating the change."
                    .to_string(),
            ),
            prompt: Some(PLAN_PROMPT.to_string()),
            read_only: Some(true),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(false),
        },
        SubagentProfile {
            name: "coder".to_string(),
            model: None,
            description: Some(
                "Write-capable implementer — hand it a precise, self-contained \
                 spec (exact files, symbols, before→after) and it implements \
                 exactly that, verifies, and commits. Use proactively for \
                 well-scoped implementation and mechanical changes; scope the \
                 work first."
                    .to_string(),
            ),
            prompt: Some(CODER_PROMPT.to_string()),
            read_only: Some(false),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(true),
        },
        SubagentProfile {
            name: "general".to_string(),
            model: None,
            description: Some(
                "General-purpose agent — full tool access for open-ended, \
                 multi-step tasks (explore and modify). Same as `task` with no \
                 `agent`."
                    .to_string(),
            ),
            prompt: None,
            read_only: Some(false),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(false),
        },
    ]
}

const EXPLORE_PROMPT: &str = "\
You are an EXPLORE sub-agent: a read-only code investigator. You have read and \
search tools only — you cannot modify files or run mutating commands. Investigate \
the area described and report back so the parent agent can act on your findings.

- Search from more than one angle — by symbol, by string/error text, and by the \
  project's file/directory conventions — so you don't miss a second definition or \
  an alternate code path.
- Trace the relevant files, types, and call paths; quote key code with `path:line`.
- Answer the question directly. Lead with the conclusion, then the evidence.
- Don't speculate past what the code shows; if something is missing or you could \
  not find it, say so explicitly rather than guessing.
- Return a tight, structured summary — not a narrative of your search. Lead with \
  a 1-3 line answer, then findings as `path:line` bullets; keep it short unless \
  the task genuinely needs more.";

pub(crate) const REVIEW_PROMPT: &str = "\
You are a REVIEW sub-agent: a read-only code reviewer. You have read and search \
tools only — you cannot modify files. Review the code or change described and \
report your findings.

- Check, in order: correctness and logic errors; edge cases and error handling; \
  concurrency, races, and resource leaks; security (injection, secrets, SSRF, \
  auth, unvalidated input); API/contract misuse; and missing or wrong tests. \
  Weigh real bugs over style nits.
- Verify every finding against the actual code — read the lines you cite. Never \
  invent a bug that isn't there or a line you didn't read; a false positive costs \
  the caller more than a missed nit.
- For each finding give: severity, `path:line`, what's wrong (a concrete failing \
  input or scenario), and a concrete fix.
- Lead with the most serious issues, grouped by severity. Skip pure style.
- End with a one-line verdict: safe to ship as-is, or what must change first. If \
  it's clean, say so plainly.";

const PLAN_PROMPT: &str = "\
You are a PLAN sub-agent: a read-only planner. Investigate the task with your \
read and search tools, then return a concrete implementation plan in your report. \
You cannot modify files or run mutating commands. Plan the work; do NOT implement \
it.

- First understand the task: trace the relevant code with your read/search tools, \
  and note how the project already does similar things so the plan fits in.
- Build the plan with: the goal in one line; the approach and why; the exact \
  files/functions/types to change; ordered steps, each sized as an independently \
  implementable — and independently reviewable — chunk: a step names the \
  files/functions it changes, its constraints, and a done-criterion, so the \
  caller can hand any single step to a coder sub-agent as a self-contained \
  brief; edge cases and risks; and how to verify (build/test/lint). Be concrete \
  enough that another agent can execute it without re-investigating — name real \
  paths and symbols, not placeholders.
- Return the full plan in your report — that report is your entire hand-off, and \
  the caller acts on it directly. Do not depend on writing anything to disk.";

const CODER_PROMPT: &str = "\
You are a CODER sub-agent: implement the task you were given, exactly and \
narrowly. The spec is your contract: build what it says, all of it, nothing \
beyond it.

- No drive-by refactors, renames, or reformatting beyond the task; no new \
  files/docs/helpers the task didn't call for; don't over-engineer (no \
  flexibility nothing uses).
- Follow the codebase's existing patterns — find how it already does this kind \
  of thing and match it.
- Verify before reporting: build/test/lint scoped to what you touched; fix what \
  your change broke. Never weaken a test to get green.
- You cannot ask questions. If part of the spec is ambiguous or turns out wrong \
  against the real code, do the unambiguous part, and report exactly what you \
  skipped or adapted and why — an honest partial beats an improvised whole.
- If faithful implementation balloons far past what the spec implies — many more \
  files or far more churn than the brief names — stop rather than deliver a \
  monster: implement the coherent core, commit it, and report the remainder as \
  proposed follow-up chunks. A reviewable partial beats an unreviewable whole.
- Commit each coherent unit as you go (Conventional Commits) and leave a clean \
  tree; your commits and report are the entire hand-off.";

/// List the model ids available for `config`'s provider.
///
/// The trusted ChatGPT OAuth provider does not expose the OpenAI-compatible
/// `/v1/models` endpoint (a plain `GET` there returns `401 Unauthorized`), so it
/// is discovered through the account model catalog behind a coordinated —
/// refreshing — OAuth access token, the same source the agent's `models`
/// tool uses. Every other provider falls back to the OpenAI-compatible
/// `/v1/models` listing.
pub async fn list_provider_models(config: &AgentConfig) -> Result<Vec<String>> {
    // The identity resolved against this config, with the auth-derived switch
    // applied (`oauth_derived` reads the OAuth store) so a keyless built-in
    // `openai` with a stored OAuth credential reports the Codex endpoint here —
    // otherwise this would list `/v1/models` off `api.openai.com` (401, no key)
    // instead of the account catalog.
    let resolved = crate::oauth_derived(ResolvedModel::from_config(config));
    if resolved.is_codex_oauth() {
        let access = coordinated_oauth_access(resolved.kind(), resolved.base_url()).await?;
        let catalog = chatgpt_model_catalog(&access, false).await;
        let mut ids: Vec<String> = catalog.models.into_iter().map(|m| m.slug).collect();
        ids.sort();
        return Ok(ids);
    }
    let client = Client::new(
        config.base_url.clone(),
        config.api_key.clone(),
        config.model.model().to_string(),
    );
    client.list_models().await
}

/// Whether `cwd` (or an ancestor) is inside a git repo. `.git` may be a
/// directory (normal) or a file (worktrees/submodules).
pub fn in_git_repo(cwd: &std::path::Path) -> bool {
    cwd.ancestors().any(|d| d.join(".git").exists())
}

/// A throwaway git worktree for an isolated sub-agent (every write-capable
/// sub-agent, when the project is a git repo). Created on a scratch branch off
/// the current `HEAD`; [`finish`](Self::finish)
/// removes it if the sub-agent made no changes, else leaves it with a pointer.
///
/// Implements [`Drop`] for best-effort cleanup when the owning future is
/// cancelled before [`finish`](Self::finish) is called.
pub(crate) struct Worktree {
    /// The repo the worktree belongs to (the sub-agent's original cwd).
    repo: PathBuf,
    /// The worktree checkout (the sub-agent's cwd while it runs).
    pub(crate) path: PathBuf,
    /// The scratch branch the worktree is on.
    branch: String,
    /// Set to `true` by `finish()` so `Drop` knows cleanup already happened
    /// and should not run again.
    cleaned: bool,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.cleaned {
            return; // already handled by keep() or a previous drop
        }
        // Best-effort synchronous cleanup for a worktree abandoned before it was
        // kept (a cancelled future). `remove_worktree` double-forces, so it clears
        // the lock this process placed at creation.
        remove_worktree(&self.repo, &self.path, &self.branch);
    }
}

impl Worktree {
    /// Create a worktree off `repo`'s current HEAD. Errors if `repo` isn't a git
    /// repository (or git isn't available).
    pub(crate) async fn create(repo: &std::path::Path) -> Result<Self> {
        if !in_git_repo(repo) {
            bail!("worktree isolation requires a git repository");
        }
        // Best-effort prune of any stale worktrees from previously aborted runs.
        prune_stale_worktrees(repo).await;
        // A unique name per worktree: the timestamp alone collides when two are
        // created within the clock's resolution (macOS `SystemTime` is only
        // ~microsecond-grained), so a same-instant pair — parallel `task` calls,
        // or parallel tests — both tried `git worktree add hrdr/task-<same>` and
        // one failed. The process id plus a monotonic counter make it
        // collision-free within and across processes.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let uniq = format!(
            "{stamp}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let branch = format!("hrdr/task-{uniq}");
        // Worktrees live under `<repo>/.hrdr/worktrees/` — inside the working tree
        // (so the parent agent can review them with its cwd-confined file tools and
        // on the same filesystem, no cross-device rename), but ignored via the
        // repo's local `info/exclude` so a fresh checkout never shows in
        // `git status` or gets staged by `git add -A`. `.hrdr/` is git-ignored, so
        // it is also skipped by `grep`/`tree` and won't pollute searches.
        let top = git_toplevel(repo).await;
        let worktrees_dir = top.join(".hrdr").join("worktrees");
        ensure_worktree_ignored(repo, &worktrees_dir).await;
        let path = worktrees_dir.join(format!("wt-{uniq}"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&branch)
            .arg(&path)
            .arg("HEAD")
            .output()
            .await
            .context("running `git worktree add`")?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // Lock the worktree, tagging the lock with this process's pid. A lock stops
        // any sweep (`git worktree prune`, this or another hrdr instance's startup
        // GC) from removing it while it is live; the pid lets a later sweep tell a
        // still-running owner (skip it) from a crashed one (stale lock → reclaim).
        // Best-effort — a failed lock just means less protection.
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "lock", "--reason"])
            .arg(format!("hrdr:pid={}", std::process::id()))
            .arg(&path)
            .output()
            .await;
        Ok(Self {
            repo: repo.to_path_buf(),
            path,
            branch,
            cleaned: false,
        })
    }

    /// Detach automatic cleanup and hand back the worktree's location. Used when
    /// a background write sub-agent's worktree must **outlive** its run — the
    /// changes stay for the parent to review and merge, and are removed only by
    /// an explicit `task_cancel` or a session reset (see [`remove_worktree`]).
    pub(crate) fn keep(mut self) -> KeptWorktree {
        self.cleaned = true; // Drop becomes a no-op
        KeptWorktree {
            path: self.path.clone(),
            branch: self.branch.clone(),
        }
    }
}

/// The surviving location of a [`Worktree`] whose cleanup was detached via
/// [`Worktree::keep`]. Removal is later done explicitly with [`remove_worktree`].
pub(crate) struct KeptWorktree {
    pub(crate) path: PathBuf,
    pub(crate) branch: String,
}

/// Whether a sub-agent's worktree holds work that cancelling its task must NOT
/// silently throw away: either uncommitted files in the checkout, OR commits the
/// sub-agent made on its scratch `branch` that are not yet on the main line
/// (`HEAD..branch`). Checking only `git status` misses the latter — a sub-agent
/// that ran `git commit` leaves a clean working tree, and removing the branch
/// with `git branch -D` would then delete the commits. Best-effort and biased
/// toward keeping: any git failure reports "has changes" so cleanup can't destroy
/// unreviewed work on a bad read.
async fn worktree_has_changes(
    repo: &std::path::Path,
    path: &std::path::Path,
    branch: &str,
) -> bool {
    // Uncommitted work in the checkout (modified/staged/untracked).
    let dirty = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true); // a failed read → assume changes, never delete blindly
    if dirty {
        return true;
    }
    // Commits on the scratch branch that aren't reachable from the main HEAD.
    let range = format!("HEAD..{branch}");
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count", &range])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() != "0")
        // A failed count (e.g. branch/HEAD unresolvable) → assume commits exist.
        .unwrap_or(true)
}

/// Blocking twin of [`worktree_has_changes`] for the synchronous reset path
/// ([`Agent::abort_background_tasks`]), which cannot `.await`. Same semantics:
/// true if the checkout is dirty OR the branch has commits not on the main line,
/// biased toward "has changes" on any git failure so a reset never deletes
/// unreviewed work.
pub(crate) fn worktree_has_changes_sync(
    repo: &std::path::Path,
    path: &std::path::Path,
    branch: &str,
) -> bool {
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true);
    if dirty {
        return true;
    }
    let range = format!("HEAD..{branch}");
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count", &range])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() != "0")
        .unwrap_or(true)
}

/// Force-remove a background sub-agent's worktree and its scratch branch. Run
/// against the main repo (`repo`), best-effort — used by `task_cancel` and by
/// [`Agent::abort_background_tasks`] for a worktree with no unreviewed work.
/// Synchronous so it can run from non-async cleanup paths.
pub(crate) fn remove_worktree(repo: &std::path::Path, path: &std::path::Path, branch: &str) {
    // `--force --force`: the first overrides the "has modifications" guard, the
    // second overrides the lock (this is an explicit, owner-driven removal, so the
    // lock has served its purpose). A single `--force` refuses a locked worktree.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force", "--force"])
        .arg(path)
        .output();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "-D", branch])
        .output();
}

/// Run `git worktree prune` in `repo` to clean up leftover worktree entries
/// from previously aborted agents. This is the safest possible prune — git
/// only removes entries whose checkout directory no longer exists. Branch
/// cleanup is intentionally skipped here: task branches may contain committed
/// work that a user hasn't reviewed yet.
async fn prune_stale_worktrees(repo: &std::path::Path) {
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output()
        .await;
}

/// Whether process `pid` is still alive, so a lock tagged with it can be told
/// apart from a crashed owner's stale lock. `kill -0` on unix (0 = alive);
/// conservative `true` elsewhere and on any error, so a live worktree is never
/// reclaimed by mistake — the cost is only that a crash orphan lingers.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(true)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Sweep leftover sub-agent worktrees under `.hrdr/worktrees/`: remove the CLEAN
/// ones — a finished/merged task, or an orphan left when a previous run exited —
/// and KEEP any with unreviewed changes (uncommitted files or unmerged commits),
/// same rule as `task_cancel`/reset. Only touches worktrees on the
/// `.hrdr/worktrees` path; other worktrees the user made are left alone.
///
/// A LIVE worktree is protected by its `git worktree lock`: if the lock's
/// `hrdr:pid=N` owner is still running, the entry is skipped, so a second hrdr
/// instance (or this one, later, if ever run in-session) never deletes a
/// worktree out from under a running sub-agent. Only a stale lock (dead owner) or
/// an unlocked entry is eligible. Runs `git worktree prune` and reaps orphaned
/// `hrdr/task-*` branches too. Synchronous + best-effort.
pub(crate) fn gc_worktrees(repo: &std::path::Path) {
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut lock: Option<Option<u32>> = None; // Some(pid?) once a `locked` line is seen
    // `--porcelain` emits one block per worktree, blocks separated by a blank
    // line: `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` (or
    // `detached`) / optional `locked [<reason>]`. Flush on each blank line, plus a
    // final sentinel for the last block.
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let (Some(p), Some(b)) = (path.take(), branch.take()) {
                let ours = p.components().any(|c| c.as_os_str() == ".hrdr")
                    && p.components().any(|c| c.as_os_str() == "worktrees");
                // A lock whose owning pid is still alive means the worktree is
                // live in some hrdr process — never touch it.
                let live_lock = matches!(lock, Some(Some(pid)) if pid_alive(pid));
                if ours && p.exists() && !live_lock && !worktree_has_changes_sync(repo, &p, &b) {
                    remove_worktree(repo, &p, &b);
                }
            }
            path = None;
            branch = None;
            lock = None;
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.trim_start_matches("refs/heads/").to_string());
        } else if let Some(rest) = line.strip_prefix("locked") {
            // `locked hrdr:pid=N` (or bare `locked`). Extract the pid if present.
            let pid = rest
                .trim()
                .strip_prefix("hrdr:pid=")
                .and_then(|s| s.trim().parse::<u32>().ok());
            lock = Some(pid);
        }
    }
    // Drop worktree metadata for checkouts whose directory is gone, then reap
    // orphaned scratch branches. `git branch -d` (lowercase) only deletes a fully
    // MERGED branch and refuses one with unreviewed commits, and it can't touch a
    // branch still checked out by a kept worktree — so this never loses work.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
    reap_orphan_task_branches(repo);
}

/// `git branch -d` every `hrdr/task-*` branch: git deletes only the fully-merged,
/// not-checked-out ones and safely refuses the rest, so crash-orphaned scratch
/// branches don't pile up. Best-effort.
pub(crate) fn reap_orphan_task_branches(repo: &std::path::Path) {
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/hrdr/task-*",
        ])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    for b in String::from_utf8_lossy(&out.stdout).lines() {
        let b = b.trim();
        if b.is_empty() {
            continue;
        }
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["branch", "-d", b])
            .output();
    }
}

/// The working-tree root of `repo` (`git rev-parse --show-toplevel`), so a
/// worktree checkout sits at the repo root even when `repo` is a subdirectory.
/// Falls back to `repo` itself if git can't answer.
async fn git_toplevel(repo: &std::path::Path) -> std::path::PathBuf {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| std::path::PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| repo.to_path_buf())
}

/// The shared git dir (`--git-common-dir`, made absolute). Its `info/exclude`
/// holds local, per-clone ignore rules that are never committed. Resolves
/// correctly for a normal repo, a linked worktree, and a submodule.
async fn git_common_dir(repo: &std::path::Path) -> std::path::PathBuf {
    let raw = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| std::path::PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| repo.join(".git"));
    if raw.is_absolute() {
        raw
    } else {
        repo.join(raw)
    }
}

/// Make sure the sub-agent worktree dir is git-ignored, WITHOUT touching the
/// repo's tracked `.gitignore`.
///
/// If `worktrees_dir` is already ignored — by the repo's `.gitignore`, a global
/// gitignore, or a previous `info/exclude` entry (`git check-ignore` sees all of
/// these) — nothing is done. Otherwise a `/.hrdr/worktrees/` rule is appended to
/// the clone-local `<git-common-dir>/info/exclude`, which is never committed, so
/// the worktree checkouts stay out of `git status` and `git add -A` without
/// touching a tracked file — and without ignoring the rest of `.hrdr/` (e.g. a
/// tracked `.hrdr/skills/`). Idempotent and best-effort.
async fn ensure_worktree_ignored(repo: &std::path::Path, worktrees_dir: &std::path::Path) {
    // Already ignored by any mechanism → leave it alone.
    let already_ignored = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", "--"])
        .arg(worktrees_dir)
        .output()
        .await
        .map(|o| o.status.success()) // exit 0 = the path is ignored
        .unwrap_or(false);
    if already_ignored {
        return;
    }
    // Not ignored: add a clone-local exclude rule (not the tracked `.gitignore`).
    // Scoped to `.hrdr/worktrees/` specifically — NOT all of `.hrdr/`, which also
    // holds user-authored `.hrdr/skills/` that the repo may legitimately track.
    let exclude = git_common_dir(repo).await.join("info").join("exclude");
    const ENTRY: &str = "/.hrdr/worktrees/";
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == ENTRY) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
    {
        let lead = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let _ = write!(
            f,
            "{lead}# hrdr sub-agent worktrees (local, not committed)\n{ENTRY}\n"
        );
    }
}

impl Agent {
    /// Abort all running background sub-agent tasks and remove every background
    /// registry/live entry. Finished-but-undelivered tasks are discarded too.
    ///
    /// Clean worktrees are removed, but a worktree with **changes** (uncommitted
    /// files or commits on its branch) is **kept** — a `/new` or `/clear` must not
    /// silently throw away a sub-agent's unreviewed work. It stays on disk under
    /// `.hrdr/worktrees/` (recoverable via `git worktree list`).
    pub fn abort_background_tasks(&mut self) {
        let mut handles = self
            .bg_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, handle) in handles.drain(..) {
            handle.abort();
        }
        drop(handles);

        // Workers publish only by finding their pre-existing registry/live entry.
        // Clearing both stores while holding their locks means a worker either
        // publishes before this cleanup (and is then removed) or finds no entry
        // afterward; no stale result can be recreated. Collect the worktrees
        // first so we can tear down the clean ones after releasing the lock.
        let worktrees: Vec<(std::path::PathBuf, String)> = {
            let mut reg = self
                .ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let wts = reg
                .iter()
                .filter_map(|t| t.worktree.clone().zip(t.branch.clone()))
                .collect();
            reg.clear();
            wts
        };
        let repo = self.ctx.cwd.clone();
        for (path, branch) in worktrees {
            // Keep a worktree that holds unreviewed work — a reset must not throw
            // it away. Only remove the clean ones.
            if !worktree_has_changes_sync(&repo, &path, &branch) {
                remove_worktree(&repo, &path, &branch);
            }
        }
        self.live_subagents.with(|v| {
            v.retain(|e| e.key == 0 || e.kind != SubagentKind::Background);
        });
    }

    /// Number of background sub-agent tasks currently tracked (running or
    /// recently finished but not yet reaped). Finished handles are reaped
    /// lazily here and in [`spawn_background`], so the count reflects live
    /// tasks after the reap.
    pub fn bg_handle_count(&self) -> usize {
        if let Ok(mut v) = self.bg_handles.lock() {
            // Best-effort reaping (see spawn_background).
            v.retain(|(_, h)| !h.is_finished());
            v.len()
        } else {
            0
        }
    }
}

//! Turn state management — extracted from [`Agent`] into its own module to keep
//! `lib.rs` manageable.

use crate::{
    Agent, AgentEvent, ChatMessage, MessageOrigin, Steer, SteeringQueue, timestamped_user_message,
};

impl Agent {
    /// Add a user-role message to history — the single place a user-role turn
    /// enters the conversation. Stamps it with an immutable entry-time timestamp
    /// (see [`timestamped_user_message`]) and tags its `origin`, so the
    /// turn-initiating message, a mid-turn steering correction, and a folded-in
    /// background result all get the same treatment. On the wire these are all
    /// just `Role::User` messages; what differs is *when and how they arrive*
    /// (a `run` argument vs. a mid-turn queue drain), not what they are — so the
    /// construction lives in one place and nothing that applies to "a user
    /// message entering history" can be applied to one arrival path but not
    /// another.
    pub(crate) fn push_user_message(&mut self, text: impl Into<String>, origin: MessageOrigin) {
        self.messages.push(ChatMessage {
            origin,
            ..timestamped_user_message(text)
        });
    }

    /// Drain any steering messages submitted since the last request into the
    /// conversation as user messages, emitting [`AgentEvent::Steered`] for each
    /// so the frontend can display it at delivery time.
    ///
    /// Each drained message goes through [`Agent::deliver_user_message`] — the
    /// same helper the turn opener uses — so a mid-turn steer and an opening
    /// message travel one code path. A mid-turn steer runs no `user_prompt` hook,
    /// so delivery cannot block: the returned `Result` is always `Ok` here.
    pub(crate) async fn drain_steering<F: FnMut(AgentEvent)>(
        &mut self,
        steering: &SteeringQueue,
        on_event: &mut F,
    ) {
        let pending: Vec<Steer> = steering
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        for msg in pending {
            // `opening = false`: no hook, so this never returns `Err`.
            let _ = self
                .deliver_user_message(msg, /*opening*/ false, on_event)
                .await;
        }
    }

    /// Whether the steering queue has any undelivered messages.
    #[cfg(test)]
    pub(crate) fn has_steering(steering: &SteeringQueue) -> bool {
        steering.lock().map(|q| !q.is_empty()).unwrap_or(false)
    }

    /// Deliver any finished **detached background sub-agents** (`task` with
    /// `background: true`) into the conversation as user-role context messages,
    /// pruning them from the shared registry — so a background result folds in
    /// mid-turn (before the next model request) or at the next turn. Emits a
    /// [`AgentEvent::Notice`] per delivery.
    pub(crate) fn drain_background<F: FnMut(AgentEvent)>(&mut self, on_event: &mut F) {
        struct Finished {
            id: u64,
            label: String,
            result: String,
            worktree: Option<(std::path::PathBuf, String)>,
            size_summary: Option<String>,
        }
        let finished: Vec<Finished> = {
            let mut v = self
                .ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut out = Vec::new();
            // A cancelled task's result is discarded, never delivered.
            for t in v
                .iter_mut()
                .filter(|t| t.done && !t.delivered && !t.cancelled)
            {
                t.delivered = true;
                out.push(Finished {
                    id: t.id,
                    label: t.label.clone(),
                    result: t.result.clone().unwrap_or_default(),
                    worktree: t.worktree.clone().zip(t.branch.clone()),
                    size_summary: t.size_summary.clone(),
                });
            }
            // Prune cancelled entries and delivered ones that have no worktree
            // (read-only / shared-dir tasks — nothing left to manage). A delivered
            // WRITE task is retained: its worktree is still on disk awaiting the
            // parent's review + merge, and it stays addressable in `task_list` /
            // `task_cleanup` / `task_cancel` until the parent resolves it.
            v.retain(|t| !t.cancelled && !(t.delivered && t.worktree.is_none()));
            out
        };
        // The main agent now has these answers, so the live entries are no longer
        // owed. They stay only while pinned (the user is viewing one); the prune
        // at turn end drops the rest.
        self.live_subagents.with(|v| {
            for e in v.iter_mut() {
                if e.bg_id.is_some_and(|b| finished.iter().any(|f| f.id == b)) {
                    e.delivered = true;
                }
            }
        });
        for f in finished {
            let Finished {
                id,
                label,
                result,
                worktree,
                size_summary,
            } = f;
            on_event(AgentEvent::Notice(format!(
                "background task #{id} ({label}) finished"
            )));
            // A write-capable sub-agent's changes are in its worktree — nothing has
            // touched the main working dir. Give the parent the full picture and
            // tell it to trust-but-verify the branch before merging.
            let body = match worktree {
                Some((path, branch)) => {
                    let p = path.display();
                    // The size summary (file count, +ins -del, commit subjects) is
                    // computed at completion time (see `task_size_summary`) — a
                    // best-effort extra, so its absence just collapses back to the
                    // plain blank line the message always had here.
                    let size_block = size_summary
                        .as_deref()
                        .map(|s| format!("{s}\n"))
                        .unwrap_or_default();
                    format!(
                        "[Background task #{id} ({label}) finished. Its changes are on branch \
                         `{branch}` in an isolated git worktree — NOTHING has landed in your \
                         working dir yet.\n  worktree: {p}\n  branch:   {branch}\n{size_block}\n\
                         Read the whole diff yourself before merging. The sub-agent was told to commit all \
                         its work and leave a clean tree, but confirm it actually did:\n  1. Call \
                         `task_diff {id}` — shows any uncommitted/untracked leftovers (must be \
                         none), its commits, and the full diff (`git diff \
                         HEAD...{branch}`). For a large result, pass `commit` (an index from the \
                         listed commits) to review it commit-by-commit instead.\n  2. If it flags \
                         uncommitted/untracked changes, do not re-delegate to another sub-agent — \
                         review those changes and commit them YOURSELF (a proper Conventional \
                         Commits message) before merging, or that work is lost. Then review the \
                         diff like a PR — every hunk, not just that commits exist — and fix \
                         anything off before bringing it over.\nThen merge the branch into your \
                         working dir (rebase it onto HEAD in the worktree first if that'd conflict \
                         — `git -C {p} rebase <your-branch>`), and once its work is in, call \
                         `task_cleanup` with id {id} to remove the worktree. Its report:]\n{result}"
                    )
                }
                None => {
                    format!("[Background task #{id} ({label}) finished — its result:]\n{result}")
                }
            };
            self.push_user_message(body, MessageOrigin::BackgroundResult);
        }
    }
}

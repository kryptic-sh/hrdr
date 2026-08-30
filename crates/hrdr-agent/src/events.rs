use super::*;

/// Events emitted as a turn progresses.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A streamed delta of model "thinking" (reasoning channel).
    Reasoning(String),
    /// A streamed delta of assistant text.
    Text(String),
    /// A tool call is about to run.
    ToolStart {
        id: String,
        name: String,
        args: String,
    },
    /// A chunk of live output streamed by a running tool (e.g. `bash`).
    ToolOutput { id: String, chunk: String },
    /// A tool call finished.
    ToolEnd {
        id: String,
        name: String,
        result: String,
        ok: bool,
    },
    /// Token usage and timing for the model call that just finished — one per
    /// round, emitted the instant its stream drains. Token counts are the
    /// server's when it reports any, an estimate otherwise.
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        /// Milliseconds this round spent *generating*: from its first streamed
        /// byte of any kind — text, reasoning, or tool-call arguments — to the
        /// end of its stream. Measured where the stream is drained, because
        /// that is the only place the tool-call-only rounds are visible: they
        /// emit no `Text`/`Reasoning` event at all, so a clock driven by events
        /// alone would count their tokens with none of their time.
        ///
        /// The prefill before that first byte is deliberately excluded — it is
        /// the wait that grows with context and it produces nothing, so leaving
        /// it in is what makes a long turn look like a slowing model.
        decode_ms: u32,
        /// Prompt tokens served from the prompt cache (a cache hit), if reported.
        cached_prompt_tokens: Option<u32>,
        /// Prompt tokens *written* into the cache on this call, if reported.
        /// Travels alongside the read count because the counters need both: a
        /// turn that writes the cache and reads nothing is the first turn of a
        /// session, not a broken cache.
        cache_creation_tokens: Option<u32>,
        /// Completion tokens spent on reasoning/thinking, if reported.
        reasoning_tokens: Option<u32>,
        /// Estimated USD for this call, when the models.dev catalog prices the
        /// model (cached prompt tokens get the cache-read discount). `None`
        /// for an unpriced model (e.g. a local server).
        cost_usd: Option<f64>,
        /// Estimated USD spent this session so far — this agent's calls plus
        /// every delegated sub-agent's (they share the counter). `None` until
        /// any call has been priced.
        session_cost_usd: Option<f64>,
        /// `true` once some call this session ran on an unpriced model and was
        /// excluded from `session_cost_usd` (only under `allow_unpriced`). A
        /// frontend showing the total must then flag it a floor (`≥ $X`), never
        /// a complete-looking figure.
        cost_partial: bool,
    },
    /// The durable chat history right after a completed tool round — every
    /// result committed, no dangling `tool_calls`. Emitted so a frontend can
    /// persist mid-turn (the turn task holds the agent lock for its whole
    /// duration, so the frontend can't read the history itself). With this
    /// saved, a crash mid-turn loses at most the round in flight; the resume
    /// path's `repair_dangling_tool_calls` covers the rest.
    History(Arc<Vec<ChatMessage>>),
    /// An out-of-band notice from the agent (e.g. a retry or auto-compaction),
    /// surfaced to the user as a system line.
    Notice(String),
    /// A steering message (submitted mid-turn) was just delivered into the
    /// conversation — the frontend shows it as a user message at this point, so
    /// display order matches the model's view.
    Steered(String),
    /// The agent's TODO list was updated by the `todo` tool. Carries the full
    /// new list so a frontend or event log reader can see the state without
    /// reaching into the shared Arc.
    TodoUpdated(Vec<hrdr_tools::TodoItem>),
    /// The model produced a final answer with no further tool calls.
    TurnDone,
}

/// A shared FIFO of user messages submitted *during* a running turn ("steering").
///
/// The frontend pushes to it while a turn runs; [`Agent::run`] drains it before
/// each model request. Since a request is only issued after the previous round's
/// tool results were appended, a steering message lands **immediately after
/// those results** — the model reads its tool output and the correction in the
/// same context, and can change course.
///
/// A message still pending when the model answers without calling a tool is
/// *not* delivered: that turn is over, and the frontend re-sends it as a turn of
/// its own. Whatever it leaves behind is the frontend's to clear.
pub type SteeringQueue = Arc<Mutex<std::collections::VecDeque<Steer>>>;

/// One message waiting to reach an agent: what the model will read, and what the
/// user actually typed.
///
/// They differ — `@file` mentions are expanded for the model, and the expansion can
/// be an entire file. The reader must see what they wrote, not the blob.
///
/// Both live on the *queue*, because the queue is the agent's: a frontend used to
/// keep a second, parallel queue of the display strings and pop the two in lockstep
/// by hand, which is a drift waiting to happen (and left the displayed text
/// depending on which side consumed first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Steer {
    /// What is pushed into the conversation — `@file`-expanded.
    pub sent: String,
    /// What the user typed, for display.
    pub display: String,
    /// Images/PDFs that ride **beside** `sent` rather than inside it: an `@shot.png`
    /// mention becomes bytes on the user message, not text. Empty for every
    /// text-only message, which is all of them until a frontend attaches one.
    pub attachments: Vec<hrdr_llm::media::Attachment>,
}

impl Steer {
    /// A message whose sent and displayed forms are the same.
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            display: text.clone(),
            sent: text,
            attachments: Vec::new(),
        }
    }

    pub fn new(sent: impl Into<String>, display: impl Into<String>) -> Self {
        Self {
            sent: sent.into(),
            display: display.into(),
            attachments: Vec::new(),
        }
    }

    /// The same message, carrying `attachments` to the model.
    pub fn with_attachments(mut self, attachments: Vec<hrdr_llm::media::Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// The same message carrying `attachments`, with the block that NAMES them
    /// appended to `sent` — for a caller that has bytes and no text mentioning
    /// them.
    ///
    /// Every dialect renders attachments *before* the message text, so without
    /// this block the receiving model is shown pictures with nothing tying them
    /// to file names, and "the screenshot" in the brief has no referent it can
    /// resolve. Numbered per kind ("Image 1", "Document 1"), matching the order
    /// the blocks render in.
    ///
    /// `display` is left alone: it is what a human wrote (or, for a delegation,
    /// what the model wrote), and a frontend showing them the label block would
    /// be showing them text they did not write.
    ///
    /// **This is the only renderer of that block.** The user's own `@shot.png`
    /// path arrives here too, through `hrdr_app::Outgoing::into_steer` a crate
    /// above — a message the main agent sends to a sub-agent is a message from
    /// the user in lieu of the user, so it is built by this, not by a second
    /// implementation that happens to agree today.
    pub fn with_labelled_attachments(
        mut self,
        attachments: Vec<hrdr_llm::media::Attachment>,
    ) -> Self {
        if attachments.is_empty() {
            return self;
        }
        self.sent.push_str("\n\n--- Attached files ---\n");
        let (mut images, mut docs) = (0, 0);
        for a in &attachments {
            let label = if a.media_type().is_image() {
                images += 1;
                format!("Image {images}")
            } else {
                docs += 1;
                format!("Document {docs}")
            };
            // The filename is the basename of an attached file — attacker-
            // controlled on a repo the user cloned or audited, and `\n`/`\r`
            // are legal in POSIX filenames. Rendered raw it could smuggle a
            // fake turn boundary or an instruction paragraph into the sub-
            // agent's opening message, framed as a fact (a name), not as file
            // content. Escaping keeps the label a single opaque line: control
            // characters become their visible `\n`-style spellings (no real
            // newline survives to open a boundary), and a backtick is doubled
            // so the name cannot break out of its quote.
            let name = a
                .filename()
                .chars()
                .map(|c| match c {
                    '\n' => "\\n".to_string(),
                    '\r' => "\\r".to_string(),
                    '\t' => "\\t".to_string(),
                    '`' => "``".to_string(),
                    c if c.is_control() => {
                        format!("\\x{:02x}", c as u32)
                    }
                    c => c.to_string(),
                })
                .collect::<String>();
            self.sent.push_str(&format!("{label}: `{name}`\n"));
        }
        self.attachments = attachments;
        self
    }
}

/// Create an empty [`SteeringQueue`].
pub fn steering_queue() -> SteeringQueue {
    Arc::new(Mutex::new(std::collections::VecDeque::new()))
}

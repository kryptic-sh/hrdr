use super::*;

impl Agent {
    /// Construct an agent, seeding the system prompt for the default tool set.
    pub fn new(config: AgentConfig) -> Result<Self> {
        if let Some(cap) = config.max_cost
            && (!cap.is_finite() || cap < 0.0)
        {
            bail!("max_cost must be finite and non-negative");
        }
        let mut tools = ToolRegistry::with_defaults();
        // The identity's endpoint is ADOPTED from the config, not re-derived: those
        // fields are what an earlier `resolve()` produced for this identity — at the
        // CLI edge, in a `task` override, in a sub-agent's inherited live endpoint —
        // possibly against a `[providers.*]` table this agent's config no longer
        // carries. Adopting keeps the agent talking to the endpoint it was handed;
        // it can no longer be a *different* provider's, because nothing but a
        // provider definition can name an endpoint.
        // The auth-derived endpoint switch is applied HERE, at the layer that can
        // read the OAuth store (`resolve`/`from_config` are pure and cannot): a
        // built-in `openai` with no resolved key but a stored OpenAI OAuth
        // credential becomes the ChatGPT/Codex endpoint (base_url + kind). The
        // client below is configured from this resolved value, not the raw config
        // fields, so it and `self.resolved` can never disagree.
        let resolved = oauth_derived(ResolvedModel::from_config(&config));
        let delegation_runtime = new_delegation_runtime(&config, &resolved);
        let registry = AgentRegistry::new();
        tools.register(Arc::new(ModelsTool {
            runtime: Arc::clone(&delegation_runtime),
            available: available_models(&config, Some(config.model.provider().as_str())),
        }));
        // Expose the `task` delegation tool unless disabled (or this *is* a
        // sub-agent). Registered before the system prompt is rendered so it's
        // listed for the model. The profile set (built-ins + discovered files +
        // config) is resolved by [`resolve_agent_profiles`].
        let mut agent_names: Vec<String> = Vec::new();
        let bg_handles: BgHandles = bg_handles();
        let cost_total: Arc<std::sync::Mutex<f64>> = Arc::new(std::sync::Mutex::new(0.0));
        let cost_partial: Arc<std::sync::atomic::AtomicBool> =
            Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Post-edit diagnostics: the session's language servers. Custom
        // `[[lsp.servers]]` are consulted before the built-ins so they win for
        // their extensions. Built before the `task` tool so sub-agents share
        // the same warm set instead of spawning their own.
        let lsp: Option<Arc<hrdr_tools::LspRegistry>> = config.lsp.then(|| {
            let mut servers: Vec<hrdr_tools::LspServerConfig> = config
                .lsp_servers
                .iter()
                .map(|s| hrdr_tools::LspServerConfig {
                    command: s.command.clone(),
                    args: s.args.clone(),
                    extensions: s.extensions.iter().map(|e| e.to_lowercase()).collect(),
                    initialization_options: s.initialization_options.clone(),
                })
                .collect();
            servers.extend(hrdr_tools::default_lsp_servers());
            Arc::new(hrdr_tools::LspRegistry::new(
                config.cwd.clone(),
                servers,
                config.lsp_wait_secs,
            ))
        });
        // Pre-warm the project's language server(s) in the background so
        // indexing-heavy servers (rust-analyzer) overlap their warm-up with
        // the first prompt instead of missing the first edit's diagnostics.
        // `try_current` keeps this a no-op outside a runtime (sync tests).
        if let Some(lsp) = &lsp
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let exts = project_lsp_extensions(&config.cwd);
            if !exts.is_empty() {
                let lsp = Arc::clone(lsp);
                handle.spawn(async move { lsp.pre_warm(&exts).await });
            }
        }
        // Warm the models.dev catalog so the `/model` selector has something to
        // list. It reads the cache synchronously (it builds its list on a
        // keypress) and never fetches, and every other consumer fetches only as a
        // side effect of needing one model's window — so on a fresh install the
        // cache could stay empty forever and the selector offer nothing but the
        // configured model. Session agent only (a delegated one shares the same
        // cache), backgrounded so it never delays first paint, and skipped without
        // a runtime (sync tests) so it never races the sync test suite.
        //
        // The same pass refreshes what each provider actually SERVES
        // (`provider_catalog`) — for every provider this machine is set up for,
        // not just the one in use, since `/model` offers them all. models.dev is
        // warmed unconditionally: it needs no credential, so it is the one list
        // that must land even for a user logged in to nothing.
        if !config.delegated && tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(provider_catalog::refresh_all(config.clone()));
        }
        if config.subagents {
            let profiles = resolve_agent_profiles(&config)?;
            agent_names = profiles.iter().map(|p| p.name.clone()).collect();
            let subagent_tool = SubagentTool::new(
                subagent_base_config(&config),
                Arc::clone(&delegation_runtime),
                profiles,
                Arc::clone(&bg_handles),
                Arc::clone(&cost_total),
                Arc::clone(&cost_partial),
                lsp.clone(),
                config.child_transcript_dir.clone(),
                registry.clone(),
            );
            tools.register(Arc::new(subagent_tool));
            // Two management tools, and only two: the ones the MODEL needs and
            // nothing substitutes for. `task_steer` redirects a running sub-agent on
            // something the model just learned (the spec was wrong, a sibling's
            // finding changed the brief); `task_cancel` stops one that became
            // redundant. `@agent` is the *user* steering — a different actor, not a
            // replacement.
            //
            // What used to be here answered questions nobody asked. `task_list` and
            // `task_output`: the user watches each sub-agent's own pane live, and the
            // model gets results delivered automatically — `task_output`'s own
            // description said "you never need to poll". `task_transcript`: a finished
            // task reports back and its changes are in the tree, so the report says
            // what it claims and `git diff` says what it did; the delta between those
            // two IS the diagnosis signal. `task_revive`: a run that went wrong is
            // exactly a run whose context holds the wrong reasoning, and models anchor
            // on their own prior output — starting fresh with a better brief is
            // cheaper to reason about and likelier to work.
            tools.register(Arc::new(SteerTool {
                live: registry.clone(),
                max_attachment_bytes: config.max_attachment_bytes,
            }));
            tools.register(Arc::new(TaskCancelTool {
                bg_handles: Arc::clone(&bg_handles),
                live: registry.clone(),
            }));
        }
        // Memory: expose the `memory` tool (registered before scoping so a
        // read-only sub-agent drops the writer) and resolve its storage roots
        // (used for the `ctx` below and the auto-loaded index).
        // Prefer explicit roots (a delegated sub-agent inherits the parent's, so
        // it shares the repo's project memory instead of keying by its worktree
        // cwd); otherwise derive from cwd (the main agent's path).
        let mem_dirs = config
            .memory
            .then(|| {
                config
                    .memory_roots
                    .clone()
                    .or_else(|| memory_dirs(&config.cwd, config.memory_dir.as_deref()))
            })
            .flatten();
        // Any agent may keep memories — a sub-agent is still an agent. What it may
        // *do* is bounded by its type and permissions, not by whether it was
        // delegated: `memory` is a write tool, so the read-only scoping below
        // already withholds it from a read-only agent.
        if config.memory {
            tools.register(Arc::new(hrdr_tools::MemoryTool));
        }
        // Filesystem confinement, derived once here for every agent — main, sub,
        // and revived alike all come through this constructor, so there is no
        // second place a mode could be decided (see `effective_sandbox`). Resolved
        // this early because two things below depend on it: the tool set, and
        // whether the working tree's own instruction files are read at all.
        let sandbox_mode = crate::config::effective_sandbox(config.sandbox, config.read_only);
        // **A jailed agent loads no instruction from the working tree.** Built-ins
        // plus the operator's own global config, nothing else. The trade that keeps
        // `AGENTS.md` loadable everywhere else — a project legitimately carries
        // instructions in it — evaporates here: jail's premise is that the repo's
        // authors are not trusted, so loading a file they wrote into the system
        // prompt hands the adversary the system prompt, and there is no second use
        // left to protect.
        //
        // Kept on the agent, because `refresh_system` re-runs both discoveries on
        // `/clear` and `set_cwd` — gate this in the constructor alone and a `set_cwd`
        // re-seeds exactly what construction excluded.
        let project_instructions = if sandbox_mode == hrdr_tools::SandboxMode::Jail {
            prompt::ProjectInstructions::Skip
        } else {
            prompt::ProjectInstructions::Load
        };
        // Commands: discovered here so the model can load one itself. The cell is
        // shared with the tool, so a `set_cwd` that finds a different project's
        // commands updates both the listing in the prompt and what the tool serves.
        // Registered before the read-only scoping below — `command` is read-only, so
        // an explorer keeps it; a profile with an explicit `tools:` allow-list that
        // omits it loses both the tool and the prompt section together.
        let commands: commands::SharedCommands = Arc::new(Mutex::new(discover_commands(
            &config.cwd,
            project_instructions,
        )));
        tools.register(Arc::new(commands::CommandTool {
            commands: Arc::clone(&commands),
        }));
        // Skills: the same arrangement for the `SKILL.md` bundles — one shared cell
        // behind both the prompt listing and the `skill` tool, and read-only, so a
        // read-only explorer keeps it. The invalid bundles discovery reports are for
        // the frontends' picker; the agent carries only what it can actually load.
        let skills: skills::SharedSkills = Arc::new(Mutex::new(
            skills::discover_skills(&config.cwd, project_instructions).skills,
        ));
        tools.register(Arc::new(skills::SkillTool {
            skills: Arc::clone(&skills),
        }));
        // Scope the tool set for a restricted sub-agent: an explicit allow-list
        // wins; else, for a read-only agent, the plain read-only set.
        if let Some(allow) = &config.allowed_tools {
            tools.retain_only(allow);
        } else if config.read_only {
            let mut keep = tools.read_only_names();
            // …plus a SHELL. A read-only agent is confined by
            // `effective_sandbox` (`SandboxMode::Read`), not by the absence of a
            // shell, and without one an `explore` or `review` agent cannot run
            // the one thing reviewing a change is mostly made of — `git log`,
            // `git blame`, `git diff` — nor a test, a linter, or any other
            // read-only command. It read whole files where a diff would have
            // done.
            //
            // The trade this makes, stated plainly because it is real: the
            // read-only guarantee now rests on the OS sandbox rather than on the
            // tool set. Where no OS sandbox is available — Windows, a macOS
            // without `sandbox-exec`, a Linux kernel without Landlock —
            // `NO_OS_SANDBOX_NOTICE` already says shell commands are not
            // confined, and on Landlock a read-mode agent degrades to
            // write-confinement. On those systems a read-only agent's shell can
            // write. The notices fire; nothing here silences them.
            keep.extend(shell_tool_names(&tools));
            tools.retain_only(&keep);
        }
        // …and `jail` caps whatever the above produced, LAST, because it is a
        // boundary rather than a preference: a profile's `tools:` list, a persona,
        // any future knob can narrow the set but none of them may widen it back.
        // The tools it removes are the ones that would make the confinement a
        // fiction — `web_fetch`/`web_search`/MCP run outside the sandbox entirely,
        // `task` launders work through a laxer child, `shell` spawns children the
        // in-process read guard cannot see into. See `JAIL_TOOLS`.
        if sandbox_mode == hrdr_tools::SandboxMode::Jail {
            tools.cap_to_jail_set();
        } else {
            // …and every other mode drops the tools that exist only for jail. They
            // are `shell`'s job everywhere a shell exists, and one call to it does
            // all of them better; carrying them costs a decision on every turn.
            tools.drop_jail_only_tools();
        }
        let delegation_enabled = tools.defs().iter().any(|d| d.function.name == "task");
        if let Ok(mut runtime) = delegation_runtime.lock() {
            runtime.public.delegation_enabled = delegation_enabled;
        }
        let mut ctx = ToolContext::new(config.cwd.clone());
        ctx.lsp = lsp;
        let mut sandbox = hrdr_tools::SandboxPolicy::for_agent(
            sandbox_mode,
            &config.cwd,
            &config.sandbox_writable_roots,
        );
        // Config can turn the untrusted-content envelope on outside jail, but never
        // off inside it: `for_agent` already set it for the mode, and this only ever
        // ORs. A knob that could switch it off in jail would let a config file
        // silently remove the marking from the one mode whose whole premise is that
        // the content is hostile.
        sandbox.wrap_tool_results |= config.wrap_tool_results;
        // The skill roots stay readable in EVERY mode, jail included. Jail is the
        // only mode that confines reads, and there a listing the agent cannot open
        // is worse than no listing at all: it names procedures, then refuses them.
        // Exactly the roots `discover_skills` walks (one definition, so the grant
        // cannot drift from the discovery), and read access only — a bundled
        // `scripts/` gets no execution privilege from this.
        sandbox.allow_read(skills::skill_dirs(&config.cwd, project_instructions));
        // No git lock and no network confinement, for anybody. An agent working in
        // the user's project — main or delegated — is assumed to have authority
        // over that project: it commits, it pushes, it fetches dependencies. The
        // sandbox stops it reaching *outside* the project, and nothing else.
        //
        // The lock that used to sit here refused the *file tools* a write that
        // `shell` walked straight around, so it stopped the honest path only.
        // Coordination between concurrent writers is a prompt rule (and the default
        // cap of one write sub-agent), not a mount.
        //
        // The network denial that used to sit here was never a boundary either: a
        // delegated agent reports to an agent that *does* have a network, so
        // injected text reaching a sub-agent propagates to the parent through its
        // report and the parent can curl. It bought one hop of latency, not
        // containment.
        ctx.sandbox = Arc::new(sandbox);
        ctx.max_output = config.tool_max_bytes;
        ctx.max_output_lines = config.tool_max_lines;
        if let Some((proj, glob)) = &mem_dirs {
            ctx.memory_project = Some(proj.clone());
            ctx.memory_global = Some(glob.clone());
        }
        let mut event_hooks = Vec::new();
        if !config.hooks.is_empty() {
            // Entries with an `event` are lifecycle hooks; the rest are
            // post-edit file hooks. Invalid globs and unknown event names are
            // skipped (lenient, like the rest of config).
            let mut file_hooks = Vec::new();
            for h in &config.hooks {
                if let Some(event) = &h.event {
                    if let Some(event) = hrdr_tools::HookEvent::parse(event) {
                        event_hooks.push(hrdr_tools::EventHook {
                            event,
                            on: h.on.clone(),
                            run: h.run.clone(),
                            timeout_secs: h
                                .timeout_secs
                                .unwrap_or(hrdr_tools::DEFAULT_HOOK_TIMEOUT_SECS),
                        });
                    }
                    continue;
                }
                let glob = match &h.glob {
                    Some(g) => match glob::Pattern::new(g) {
                        Ok(p) => Some(p),
                        Err(_) => continue,
                    },
                    None => None,
                };
                file_hooks.push(hrdr_tools::Hook {
                    on: h.on.clone(),
                    glob,
                    run: h.run.clone(),
                    timeout_secs: h
                        .timeout_secs
                        .unwrap_or(hrdr_tools::DEFAULT_HOOK_TIMEOUT_SECS),
                });
            }
            if !file_hooks.is_empty() {
                ctx.hooks = Arc::new(file_hooks);
            }
        }
        let event_hooks = Arc::new(event_hooks);
        // User guardrails layer on top of the built-in set. An invalid regex is
        // skipped rather than fatal (lenient, like the rest of config parsing) —
        // but never silently: a rule the user believes is enforcing something,
        // and which is not there, is worse than no rule at all. Each rejected
        // pattern is kept, queued below as a startup notice and listed by
        // `/guardrails` (see [`Agent::guardrail_specs`]) as not active.
        let mut invalid_guardrails: Vec<(String, String)> = Vec::new();
        if !config.guardrails.is_empty() {
            let mut rails = hrdr_tools::default_guardrails();
            for g in &config.guardrails {
                match hrdr_tools::Guardrail::new(&g.pattern, &g.message) {
                    Ok(rail) => rails.push(rail),
                    Err(e) => invalid_guardrails.push((
                        g.pattern.clone(),
                        // `regex::Error` renders as several lines with a caret
                        // under the offending byte; flattened it still names the
                        // pattern and the syntax problem, and fits a notice line.
                        e.to_string()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" "),
                    )),
                }
            }
            ctx.guardrails = Arc::new(rails);
        }
        let project_docs = gather_agent_docs(&config.cwd, project_instructions);
        let project_docs_changed = false;
        let memory = mem_dirs
            .as_ref()
            .map(|(p, g)| gather_memory(p, g))
            .unwrap_or_default();
        let subagent_limits = prompt::SubagentLimits {
            read_only: config.max_readonly_subagents,
            write: config.max_write_subagents,
        };
        // Discover the project's gate once, here, and hand the same value to
        // both consumers: the prompt section that tells the model what to run,
        // and the ledger that notices when it did not. Two discoveries could
        // disagree, and a prompt naming one bar while the note measured another
        // would be worse than having neither.
        let gate = Arc::new(hrdr_tools::Gate::detect(&config.cwd));
        ctx.verification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_gate((*gate).clone());
        let (system, system_cache_split) = build_system_prompt(
            &tools,
            &config.cwd,
            &project_docs,
            &memory,
            &commands.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            &skills.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            config.agent_prompt.as_deref(),
            config.delegated,
            &ctx.sandbox,
            subagent_limits,
            &gate,
        )?;

        // Configure the client from the (possibly auth-switched) resolved model,
        // not the raw config fields — so an OAuth `openai` talks to the Codex
        // endpoint, and the client's endpoint/headers match `self.resolved`.
        let cache_mode = resolve_cache_mode(config.prompt_cache.as_deref(), resolved.base_url());
        let mut client = Client::new(
            resolved.base_url().to_string(),
            resolved.api_key().map(str::to_string),
            resolved.reference().model().to_string(),
        )
        .with_cache(cache_mode);
        if let Some(t) = config.temperature {
            client = client.with_temperature(t);
        }
        client.set_effort(config.effort.clone());
        client.set_params(hrdr_llm::RequestParams {
            max_tokens: config.max_tokens,
            top_p: config.top_p,
            seed: config.seed,
            stop: config.stop.clone(),
            include_usage: config.stream_usage,
        });
        client.set_headers(resolved.headers().to_vec());
        client.set_system_cache_split(system_cache_split);
        // The per-attachment ceiling belongs to the user, not to the identity:
        // it stays put across a `/model` switch (unlike the endpoint, key and
        // headers reset in `set_model_ref`), so it is set once, here.
        client.set_max_attachment_bytes(config.max_attachment_bytes);
        // One key for this agent's whole conversation — see the field docs and
        // `new_prompt_cache_key`. Set unconditionally: the client only puts it on
        // the wire for the two OpenAI-shaped backends, so there is nothing to gate
        // on here, and gating would just be another way to forget it.
        let prompt_cache_key = new_prompt_cache_key();
        client.set_prompt_cache_key(Some(prompt_cache_key.clone()));
        // One OpenCode session id per conversation, mirroring the prompt-cache
        // key just above: minted here so every agent — headless or delegated,
        // or a main agent before its frontend has reserved a durable session —
        // still sends the `x-opencode-session` header the OpenCode gateway
        // requires. The frontend overwrites it with the durable id once one is
        // reserved (see `Agent::set_session_id`).
        let session_id = new_prompt_cache_key();
        client.set_session_id(Some(session_id.clone()));
        client.set_api_version(resolved.api_version().map(str::to_string));
        client.set_cache_ttl_1h(config.prompt_cache_ttl.as_deref().map(str::trim) == Some("1h"));
        client.set_timeout(
            config
                .request_timeout
                .filter(|seconds| *seconds > 0)
                .map(std::time::Duration::from_secs),
        );

        // Is the model we are about to run on even a model? Network-free, from the
        // catalogs already on disk — see `preflight_notices`. Queued rather than
        // printed: a line on stderr is invisible under a TUI, and a sub-agent has no
        // stderr anyone reads.
        let mut pending_notices = preflight_notices(&config.providers, &resolved);
        // An `AGENTS.md` hrdr found and did not load is a user instruction silently
        // missing from the prompt — the same channel carries it, for the same reason.
        pending_notices.extend(project_docs.skipped.iter().map(|s| s.notice()));
        // A `[[guardrails]]` entry whose regex does not compile blocks nothing,
        // and from the outside that is indistinguishable from a rule that is
        // working — the same channel, for the same reason.
        pending_notices.extend(invalid_guardrails.iter().map(|(pattern, err)| {
            format!(
                "guardrail: `{pattern}` is not a valid regex, so this `[[guardrails]]` rule is \
                 NOT in effect and the commands you meant it to block are allowed — {err}"
            )
        }));
        // `--sandbox jail` asked for the audit posture and this agent cannot have it:
        // jail's tool set has no shell, no writers and no network, so a
        // write-capable agent under it could not work at all, and `effective_sandbox`
        // floors it at `write`. That floor is right — but silently handing someone
        // who typed `jail` a full-write session is the opposite of what they asked
        // for, and they would not find out. So say it, and name the way to actually
        // get jail.
        if config.sandbox == hrdr_tools::SandboxMode::Jail
            && sandbox_mode != hrdr_tools::SandboxMode::Jail
        {
            pending_notices.push(format!(
                "sandbox: `jail` needs a read-only agent — it has no shell and no writers, so a \
                 write-capable agent cannot run under it. This session is confined to `{}` \
                 instead. To audit untrusted code, delegate it: `task` with the `prisoner` \
                 agent, which is jailed whatever the session says. `--agent explore` (or any \
                 read-only agent) honours `jail` directly.",
                sandbox_mode
            ));
        }
        // An agent whose profile declares its own mode overrides the session's,
        // `--yolo` included — and says so. The override is right (you spawned
        // `prisoner` precisely to contain something, so a session flag aimed at
        // everything else must not uncontain it) but it reverses "session `none` wins
        // everywhere", and a reversal nobody is told about is the kind of surprise
        // that gets worked around rather than understood.
        if let Some(declared) = config.declared_sandbox
            && declared != config.session_sandbox
        {
            pending_notices.push(format!(
                "sandbox: this agent declares `{declared}` and runs under it, overriding the \
                 session's `{}` — containment is part of what this agent is.",
                config.session_sandbox
            ));
        }

        // The window this agent works against, decided once, here: a configured
        // window wins, otherwise the one its identity derives — network-free, since
        // resolving the identity above already looked it up.
        //
        // Eager on purpose. It is published to this agent's registry entry the
        // moment a frontend attaches (`publish_chrome`), so every agent's context
        // gauge can draw before its first turn. Deriving it lazily meant the
        // delegation path had to compute a window of its own to fill a delegated
        // agent's gauge, while the session's own agent — whose frontend seeded the
        // entry with zeroed counters — showed a bare number until its first reply.
        let context_window = config.context_window.or_else(|| resolved.context_window());
        Ok(Self {
            client,
            prompt_cache_key,
            session_id,
            resolved,
            providers: config.providers,
            pending_notices,
            invalid_guardrails,
            delegation_runtime,
            registry,
            live_home: None,
            delegated: config.delegated,
            read_only: config.read_only,
            subagent_limits,
            gate,
            last_prompt_tokens: None,
            prompt_cache: config.prompt_cache,
            tools,
            ctx,
            messages: Arc::new(vec![ChatMessage::system(system)]),
            max_steps: config.max_steps,
            retry_policy: config.retry,
            auto_compact: config.auto_compact,
            compaction_reserved: config.compaction_reserved,
            context_window,
            // Answered already, unless the identity had nothing to say — in which
            // case a later `ensure_context_window` may still find something.
            context_window_probed: context_window.is_some(),
            self_compact_failed_at: None,
            unsupported_params: Vec::new(),
            tool_syntax_warned: false,
            todo_turn: 0,
            todo_completed_at: HashMap::new(),
            todo_ttl: config.todo_ttl,
            compaction_tail_turns: config.compaction_tail_turns,
            preserve_recent_tokens: config.preserve_recent_tokens,
            project_instructions,
            project_docs,
            project_docs_changed,
            mcp_configs: config.mcp,
            mcp_clients: Vec::new(),
            agent_prompt: config.agent_prompt,
            memory_enabled: config.memory,
            memory_dir: config.memory_dir,
            agent_names,
            commands,
            skills,
            bg_handles,
            cost_total,
            cost_partial,
            cost_rates: None,
            max_cost: config.max_cost,
            allow_unpriced: config.allow_unpriced,
            event_hooks,
        })
    }

    /// Names of the sub-agents this agent can delegate to (for `@name` mention
    /// routing in the frontend). Empty when delegation is disabled.
    pub fn agent_names(&self) -> &[String] {
        &self.agent_names
    }

    /// Record that this agent's context already carries each path's **whole**
    /// current content, satisfying the read-before-edit guard for it.
    ///
    /// The frontend's `@file` expansion inlines referenced files into the
    /// outgoing message, so the model has genuinely seen them — without this the
    /// guard would make it re-read a file whose full text is already sitting in
    /// its context. Full inlines only: a truncated attachment must not license a
    /// blind overwrite, so callers filter those out before calling.
    pub fn mark_files_read(&self, paths: &[std::path::PathBuf]) {
        for path in paths {
            self.ctx.mark_read(path);
        }
    }

    /// Connect to the configured `[[mcp]]` servers, registering each server's
    /// tools (namespaced `<name>_<tool>`) into the tool set and re-rendering the
    /// system prompt so they're listed. Resilient: a server that fails to start
    /// or handshake is skipped. Returns one human-readable status line per
    /// server (for the frontend to surface). Call once, after [`Self::new`],
    /// before the first turn; a second call is a no-op (configs are consumed).
    pub async fn connect_mcp(&mut self) -> Vec<String> {
        let configs = std::mem::take(&mut self.mcp_configs);
        let mut notices = Vec::new();
        for cfg in &configs {
            if cfg.disabled {
                continue;
            }
            let pairs = |m: &HashMap<String, String>| -> Vec<(String, String)> {
                m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            };
            // Transport: `url` → HTTP (Streamable, or legacy SSE when
            // `transport = "sse"`), else `command` → stdio.
            let connected = match (&cfg.url, &cfg.command) {
                (Some(url), _) if cfg.transport.as_deref() == Some("sse") => {
                    hrdr_tools::McpClient::connect_sse(&cfg.name, url, &pairs(&cfg.headers)).await
                }
                (Some(url), _) => {
                    hrdr_tools::McpClient::connect_http(&cfg.name, url, &pairs(&cfg.headers)).await
                }
                (None, Some(cmd)) => {
                    hrdr_tools::McpClient::connect_stdio(
                        &cfg.name,
                        cmd,
                        &cfg.args,
                        &pairs(&cfg.env),
                    )
                    .await
                }
                (None, None) => {
                    notices.push(format!("MCP '{}' skipped: no `command` or `url`", cfg.name));
                    continue;
                }
            };
            match connected {
                Ok((client, tools)) => {
                    let n = tools.len();
                    for t in tools {
                        self.tools.register(t);
                    }
                    self.mcp_clients.push(client);
                    notices.push(format!(
                        "MCP '{}': connected ({n} tool{})",
                        cfg.name,
                        if n == 1 { "" } else { "s" }
                    ));
                }
                Err(e) => notices.push(format!("MCP '{}' failed: {e}", cfg.name)),
            }
        }
        // New tools changed the set the model is told about — rebuild the prompt.
        if !self.mcp_clients.is_empty() {
            self.refresh_system();
        }
        notices
    }

    /// The gathered `AGENTS.md` project instructions for the current cwd, if any.
    /// Whether the project docs re-read by the last [`Self::clear`] / [`Self::set_cwd`]
    /// differ from the ones that were in the prompt.
    ///
    /// A *running* conversation is never re-seeded with a changed `AGENTS.md`: the
    /// agent that edited the file already has the change in its context, and
    /// re-injecting it would say the same thing twice. A new conversation
    /// (`/new`) starts from whatever is on disk now, and this is how a frontend
    /// knows to mention it.
    pub fn project_docs_changed(&self) -> bool {
        self.project_docs_changed
    }

    pub fn project_docs(&self) -> Option<&str> {
        self.project_docs.project.as_deref()
    }

    /// **Whether this session may read project-scoped instruction files at all**
    /// — the single source of truth for that question, for the agent and for
    /// every frontend.
    ///
    /// Derived once in [`Self::new`] from the effective sandbox mode (a jailed
    /// session gets [`ProjectInstructions::Skip`]) and fixed for the agent's
    /// life, so it is safe to read once and keep. A frontend that discovers
    /// commands or skills for its own `:name` completion, picker or send path
    /// **must** pass this value to [`discover_commands`] / [`discover_skills`]
    /// rather than deciding for itself: re-deriving the rule is how the two
    /// answers drift, and the frontend's is the one the user types into.
    pub fn project_instructions(&self) -> prompt::ProjectInstructions {
        self.project_instructions
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Reset the conversation to a fresh state — as if the agent was just
    /// constructed for the current cwd. Drops all history and **re-reads
    /// `AGENTS.md`**, so an updated or removed project-instructions file is
    /// reflected (the old system prompt is not kept around).
    ///
    /// Also aborts any running background sub-agent tasks so stale results from
    /// a previous session don't land in the new conversation, and removes all
    /// background-registry / background live-subagent entries from the previous
    /// session.
    pub fn clear(&mut self) {
        self.abort_background_tasks();
        Arc::make_mut(&mut self.messages).clear();
        self.reset_read_files();
        self.reset_session_cost();
        self.refresh_system();
        // A fresh conversation deserves a fresh chance at proactive compaction —
        // whatever made the summarizer fail belonged to the old history (or was
        // transient), not to this agent for the rest of the session.
        self.self_compact_failed_at = None;
        // A cleared conversation is a NEW conversation: it must not keep the
        // previous one's OpenCode session id, or the gateway would group two
        // unrelated conversations together.
        self.session_id = new_prompt_cache_key();
        self.client.set_session_id(Some(self.session_id.clone()));
    }

    /// Forget which files the model has "seen": once the transcript no longer
    /// contains their content (clear/resume/compaction), edits must re-read
    /// first — the read-before-edit gate tracks the model's context, not disk.
    pub(crate) fn reset_read_files(&mut self) {
        if let Ok(mut set) = self.ctx.read_files.lock() {
            set.clear();
        }
    }

    /// Rebuild `messages[0]` with a freshly-read memory index, leaving project
    /// docs as they are.
    ///
    /// Only compaction calls this. A running conversation is deliberately never
    /// re-seeded from `AGENTS.md` — the agent that edited the file already has
    /// the change in its context — but the *memory index* is different: a note
    /// the agent saves this session exists for it only as a tool exchange in the
    /// history, and compaction summarizes that exchange away. Without this the
    /// note would be on disk, missing from the index, and gone from the
    /// conversation: saved and then invisible.
    ///
    /// Re-reads from the memory roots already resolved for this cwd, so it does
    /// no path resolution and cannot change scope.
    pub(crate) fn refresh_system_prompt_in_place(&mut self) {
        if !self.memory_enabled {
            return;
        }
        let memory = match (&self.ctx.memory_project, &self.ctx.memory_global) {
            (Some(proj), Some(glob)) => gather_memory(proj, glob),
            _ => return,
        };
        let Ok((system, system_cache_split)) = build_system_prompt(
            &self.tools,
            &self.ctx.cwd,
            &self.project_docs,
            &memory,
            &self.commands_snapshot(),
            &self.skills_snapshot(),
            self.agent_prompt.as_deref(),
            self.delegated,
            &self.ctx.sandbox,
            self.subagent_limits,
            &self.gate,
        ) else {
            return;
        };
        // Keep the client's cache boundary in step with the prompt it describes;
        // a stale offset would close the breakpoint in the wrong place.
        self.client.set_system_cache_split(system_cache_split);
        if self.messages.first().map(|m| m.role == Role::System) == Some(true) {
            Arc::make_mut(&mut self.messages)[0] = ChatMessage::system(system);
        } else {
            Arc::make_mut(&mut self.messages).insert(0, ChatMessage::system(system));
        }
    }

    /// A copy of the shared command set, for a prompt rebuild.
    pub(crate) fn commands_snapshot(&self) -> Vec<Command> {
        self.commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// A copy of the shared skill set, for a prompt rebuild.
    fn skills_snapshot(&self) -> Vec<Skill> {
        self.skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Re-gather `AGENTS.md` for the current cwd and rebuild the system prompt
    /// in `messages[0]` (seeding it if the history is empty). Shared by
    /// [`Self::clear`], [`Self::set_cwd`] and [`Self::set_messages`].
    fn refresh_system(&mut self) {
        // Whether the project docs on disk differ from the ones already in the
        // prompt. Content, not just mtime: a `touch` moves the timestamp without
        // changing a word, and re-announcing a reload that changed nothing is a lie.
        let docs = gather_agent_docs(&self.ctx.cwd, self.project_instructions);
        self.project_docs_changed = docs != self.project_docs;
        // A `set_cwd` into a project whose AGENTS.md is over a cap has to say so
        // too — the file is missing from the prompt this call just rebuilt. Deduped,
        // since `/clear` re-runs this against the same tree.
        for notice in docs.skipped.iter().map(|s| s.notice()) {
            if !self.pending_notices.contains(&notice) {
                self.pending_notices.push(notice);
            }
        }
        self.project_docs = docs;
        // Re-resolve memory roots for the (possibly changed) cwd and reload the
        // index, so `/clear` and `set_cwd` reflect saved notes for this project.
        let memory = if self.memory_enabled {
            if let Some((proj, glob)) = memory_dirs(&self.ctx.cwd, self.memory_dir.as_deref()) {
                let mem = gather_memory(&proj, &glob);
                self.ctx.memory_project = Some(proj);
                self.ctx.memory_global = Some(glob);
                mem
            } else {
                MemoryIndex::default()
            }
        } else {
            MemoryIndex::default()
        };
        // Re-discover commands for the (possibly changed) cwd, through the cell the
        // `command` tool holds — so a project switch moves the listing and the tool's
        // answer together.
        let commands = discover_commands(&self.ctx.cwd, self.project_instructions);
        *self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = commands.clone();
        // Skills likewise, through their own cell — a project switch that changes
        // the listing must change what the `skill` tool serves in the same step.
        let skills = discover_skills(&self.ctx.cwd, self.project_instructions).skills;
        *self
            .skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = skills.clone();
        // A different project has a different gate, and the ledger must move with
        // the prompt — measuring a new project against the old project's CI is
        // exactly the kind of confident wrong answer the gate exists to remove.
        self.gate = Arc::new(hrdr_tools::Gate::detect(&self.ctx.cwd));
        self.ctx
            .verification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_gate((*self.gate).clone());
        let Ok((system, system_cache_split)) = build_system_prompt(
            &self.tools,
            &self.ctx.cwd,
            &self.project_docs,
            &memory,
            &commands,
            &skills,
            self.agent_prompt.as_deref(),
            self.delegated,
            &self.ctx.sandbox,
            self.subagent_limits,
            &self.gate,
        ) else {
            return;
        };
        // Keep the client's cache boundary in step with the prompt it describes;
        // a stale offset would close the breakpoint in the wrong place.
        self.client.set_system_cache_split(system_cache_split);
        if self.messages.first().map(|m| m.role == Role::System) == Some(true) {
            Arc::make_mut(&mut self.messages)[0] = ChatMessage::system(system);
        } else {
            Arc::make_mut(&mut self.messages).insert(0, ChatMessage::system(system));
        }
    }

    /// A clone of the full message history (for saving a session).
    pub fn messages_owned(&self) -> Vec<ChatMessage> {
        (*self.messages).clone()
    }

    /// A clone of the current TODO list.
    pub fn todos_owned(&self) -> Vec<TodoItem> {
        self.todos().lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// Replace the message history (for resuming a session). Resets the
    /// read-before-edit gate: this conversation didn't read those files.
    ///
    /// **The saved `messages[0]` is not restored — the system prompt is rebuilt.**
    /// A resume is a new session in a new process, and the whole prompt is
    /// regenerated every session by design (the section ordering is what makes
    /// that cache-safe). The saved copy is stale by construction: yesterday's
    /// date, the memory index and `AGENTS.md` as they were when the session was
    /// written, and — the reason this is a bug and not just staleness — bytes
    /// that no longer match the cache split the client computed for the prompt
    /// built in [`Agent::new`]. Installing saved text under a fresh offset puts
    /// the Anthropic `cache_control` breakpoint at the wrong byte and silently
    /// costs the stable-prefix cache hit.
    ///
    /// Only `messages[0]` is rewritten; the conversation itself — including the
    /// signed Anthropic thinking blocks a pending `tool_use` needs — is installed
    /// verbatim.
    pub fn set_messages(&mut self, messages: Vec<ChatMessage>) {
        self.messages = Arc::new(messages);
        self.reset_read_files();
        // `refresh_system` re-gathers `AGENTS.md` and so recomputes
        // `project_docs_changed`. That flag means "a *new* conversation (`/new`)
        // picked up an edited file, tell the user" — a resume is not that event
        // and must not raise the notice, so restore whatever it was.
        let docs_changed = self.project_docs_changed;
        self.refresh_system();
        self.project_docs_changed = docs_changed;
    }

    /// Adopt this agent's entry in the registry a frontend reads, and publish its
    /// chrome into it.
    ///
    /// From here on, **the agent is the source of what it is running on**. Whatever
    /// the display shows for this agent — model, provider, endpoint — is what the
    /// agent published, so the two cannot disagree. A frontend that kept its own
    /// copy could adopt a session's model and provider label into the status bar
    /// while the agent went on talking to the endpoint it launched with, and the bar
    /// would confidently name a provider the request never went to.
    pub fn attach_live(&mut self, live: AgentRegistry, key: u64) {
        // The agent's own TODO list, so a frontend showing this agent shows *its*
        // list rather than the main agent's.
        let todos = Arc::clone(&self.ctx.todos);
        live.update(key, |e| e.todos = todos);
        self.live_home = Some((live, key));
        self.publish_delegation_runtime();
    }

    fn publish_delegation_runtime(&self) {
        {
            let mut runtime = self
                .delegation_runtime
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The whole resolved identity, in one assignment — a sub-agent spawned
            // after any switch inherits an endpoint that agrees with itself.
            runtime.public.reference = self.resolved.reference().clone();
            runtime.public.effort = self.client.effort().map(str::to_string);
            runtime.endpoint.resolved = self.resolved.clone();
            runtime.endpoint.effort = self.client.effort().map(str::to_string);
        }
        self.publish_chrome();
    }

    /// Push what this agent is running on into its registry entry — the thing a
    /// frontend renders. Called from every path that changes the model, the
    /// provider, or the endpoint, so a display copy can never go stale.
    pub(crate) fn publish_chrome(&self) {
        let Some((live, key)) = &self.live_home else {
            return; // headless / not displayed: nothing to publish to
        };
        let model = self.client.model.clone();
        let provider = Some(self.provider_name().to_string());
        let base_url = self.client.base_url().to_string();
        let effort = self.client.effort().map(str::to_string);
        let window = self.context_window;
        let (auto_compact, reserved) = (self.auto_compact, self.compaction_reserved);
        live.update(*key, |e| {
            e.model = model;
            e.provider = provider;
            e.base_url = base_url;
            e.effort = effort;
            e.auto_compact = auto_compact;
            e.compaction_reserved = reserved;
            // A model switch invalidates the window until it is re-learned; keep
            // showing the last known figure rather than blanking the gauge.
            if window.is_some() {
                e.usage.context_window = window;
            }
        });
    }

    /// **Switch what this agent is running on.** The one mutator.
    ///
    /// A [`ModelRef`] is a complete identity, and everything downstream of it moves
    /// with it, in one step: the endpoint, the API key, the api-version, the
    /// headers, the client's model, the prompt-cache mode (an endpoint fact), the
    /// trust kind (which gates OAuth injection), the price card, the context window
    /// (invalidated — the old figure described a different model), and the runtime
    /// projection sub-agents inherit.
    ///
    /// There is deliberately no way to move one of those without the others. The
    /// five setters this replaces (`set_model`, `set_provider`, `set_endpoint`,
    /// `apply_provider_switch`, `set_provider_identity`) each moved a subset, and
    /// every caller had to remember the rest — which is how a model got to outlive
    /// the provider it belongs to.
    ///
    /// Errors (leaving the agent untouched) when the identity names a provider that
    /// is neither a built-in nor a `[providers.<name>]`.
    ///
    /// The endpoint always comes back from [`resolve_in`] — the provider's own, and
    /// there is no other kind. Nothing carried over from the endpoint in force can
    /// survive a switch, because nothing but a provider definition ever named it.
    pub fn set_model_ref(&mut self, reference: ModelRef) -> Result<()> {
        let resolved = resolve_in(&self.providers, &reference, None)?;
        self.adopt_resolved(resolved);
        Ok(())
    }

    /// Set this agent's OpenCode conversation id — the value its requests send
    /// as `x-opencode-session` to the OpenCode gateway. A frontend calls this
    /// with its durable on-disk session id once a session is reserved (and
    /// again on resume), so the id is stable across restarts; until then the
    /// id minted at construction stands (see the field docs).
    pub fn set_session_id(&mut self, id: String) {
        self.session_id = id;
        self.client.set_session_id(Some(self.session_id.clone()));
    }

    /// The OpenCode conversation id currently in force — the value sent as
    /// `x-opencode-session`. Exposed so a caller that mints or replaces it
    /// (or a test driving [`Self::clear`]) can assert what the agent and its
    /// client actually carry.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Would `reference` be a real identity on this agent's providers? — the
    /// network-free pass that runs BEFORE [`set_model_ref`](Self::set_model_ref)
    /// moves anything.
    ///
    /// `Err` only when the provider itself does not resolve. The *model* is never
    /// refused here: an unproven absence comes back as
    /// [`Identity::Unconfirmed`](crate::Identity::Unconfirmed), which only
    /// [`confirm_identity`](crate::confirm_identity) — and its fresh fetch — may turn
    /// into a refusal.
    ///
    /// Resolves the candidate the same way `set_model_ref` will — same providers,
    /// same endpoint — so what is validated is what would be adopted, not an
    /// approximation of it.
    pub fn validate_ref(&self, reference: &ModelRef) -> Result<validate::Identity> {
        let resolved = resolve_in(&self.providers, reference, None)?;
        Ok(validate::validate_identity_in(&self.providers, &resolved))
    }

    /// Apply a resolved identity to the client and the runtime, atomically. The
    /// single writer of `self.resolved`.
    ///
    /// The auth-derived endpoint switch is applied here — the single writer — so a
    /// `/model` switch to a keyless built-in `openai` with a stored OpenAI OAuth
    /// credential lands on the ChatGPT/Codex endpoint, exactly as construction
    /// does. [`resolve_in`] stays pure; this is where the OAuth store is read.
    fn adopt_resolved(&mut self, resolved: ResolvedModel) {
        let resolved = oauth_derived(resolved);
        // Pre-flight the identity actually being adopted (post auth-switch), here in
        // the single writer — so every path that changes what this agent runs on gets
        // the check, not just the ones that remembered to ask for it. Deduped: bouncing
        // between two models must not stack the same notice twice.
        for notice in preflight_notices(&self.providers, &resolved) {
            if !self.pending_notices.contains(&notice) {
                self.pending_notices.push(notice);
            }
        }
        let cache = resolve_cache_mode(self.prompt_cache.as_deref(), resolved.base_url());
        self.client.set_base_url(resolved.base_url().to_string());
        self.client
            .set_api_key(resolved.api_key().map(str::to_string));
        self.client.set_cache(cache);
        self.client.set_headers(resolved.headers().to_vec());
        // Re-assert the prompt-cache key here, in the single writer, with the
        // SAME value: the conversation did not change, only what it runs on, and
        // the requests after the switch still share their prefix with the ones
        // before it. Re-set rather than left alone so no future rework of this
        // function (which already rebuilds endpoint, key, headers and api-version
        // from scratch) can silently leave the client without one — on GPT-5.6 an
        // absent key means the cache stops matching, and nothing errors.
        self.client
            .set_prompt_cache_key(Some(self.prompt_cache_key.clone()));
        // Same for the OpenCode session id, for the same reason: the
        // conversation did not change, only what it runs on, and the requests
        // after the switch must still identify it to the gateway.
        self.client.set_session_id(Some(self.session_id.clone()));
        self.client
            .set_api_version(resolved.api_version().map(str::to_string));
        self.client.model = resolved.reference().model().to_string();
        self.resolved = resolved;
        self.cost_rates = None;
        // A different model has a different window; the old figure is not ours.
        self.invalidate_context_window();
        self.publish_delegation_runtime();
    }

    /// Take the notices queued outside a turn (see the `pending_notices` field) —
    /// currently the model pre-flight's. [`Agent::run`] drains this at the top of a
    /// turn into [`AgentEvent::Notice`]; a frontend that just applied a `/model`
    /// switch drains it there instead, so the notice lands with the switch rather
    /// than one turn later.
    pub fn take_pending_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_notices)
    }

    /// What this agent is running on: provider AND model, as one value.
    pub fn model_ref(&self) -> &ModelRef {
        self.resolved.reference()
    }

    /// The identity resolved against the config — endpoint, key, headers, trust
    /// kind, window. Derived state; the [`ModelRef`] is what is authoritative.
    pub fn resolved(&self) -> &ResolvedModel {
        &self.resolved
    }

    /// The current provider's trust identity — lets callers (health probe,
    /// `/doctor`) special-case trusted ChatGPT OAuth without re-resolving.
    pub fn provider_kind(&self) -> ResolvedProviderKind {
        self.resolved.kind()
    }

    /// The provider this agent is on. Always a name: an agent without a provider
    /// is not a thing that can exist any more.
    pub fn provider_name(&self) -> &str {
        self.resolved.reference().provider().as_str()
    }

    /// The model this agent will actually send to.
    pub fn model_name(&self) -> String {
        self.client.model.clone()
    }

    /// The endpoint this agent will actually talk to.
    pub fn endpoint_base_url(&self) -> String {
        self.client.base_url().to_string()
    }

    /// Whether this agent can authenticate to its endpoint at all: it holds a
    /// resolved API key, or it is on trusted ChatGPT OAuth (whose bearer is
    /// injected into the client at request time rather than stored here).
    ///
    /// Callers use this to avoid *making a call they know will fail* — an
    /// unauthenticated request to a provider that requires auth returns 401,
    /// which says nothing about the endpoint and everything about the missing
    /// credential.
    pub fn has_credential(&self) -> bool {
        if self.resolved.kind() == ResolvedProviderKind::ChatGptOAuth {
            return true;
        }
        self.resolved.api_key().is_some()
    }

    /// A clone of the model client (for out-of-band calls like the startup
    /// endpoint health check's `list_models`).
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Append a user-role note to the history without running a turn. The
    /// TUI's `!command` shell escape records the command + its output this
    /// way, so the next model call sees what the user ran.
    pub fn push_user_note(&mut self, text: impl Into<String>) {
        Arc::make_mut(&mut self.messages).push(ChatMessage::user(text));
    }

    /// Status of the post-edit LSP layer for `/doctor`:
    /// `(wait_secs, one row per configured server)`, or `None` when disabled.
    pub async fn lsp_statuses(&self) -> Option<(u64, Vec<hrdr_tools::LspServerReport>)> {
        let reg = self.ctx.lsp.as_ref()?;
        Some((reg.wait_secs(), reg.statuses().await))
    }

    pub async fn probe_context_window(&self) -> Option<u32> {
        if let Some(n) = self.client.context_window().await {
            return Some(n);
        }
        // ChatGPT's `/v1/models` 401s (the client returned `None` above), so resolve
        // per-model from the account catalog cache — NOT models.dev, whose
        // cross-provider fallback would return the same-id API model's (different)
        // window. Mirrors `context_window_for`; keeps every probe path consistent.
        if self.client.base_url() == CHATGPT_CODEX_BASE_URL {
            return self.resolved.context_window();
        }
        hrdr_llm::catalog::context_window(
            catalog_provider_key(Some(self.provider_name())).as_deref(),
            &self.client.model,
        )
        .await
    }

    /// Tell the agent its context window — e.g. a frontend that probed the
    /// endpoint for its status bar can hand the figure over instead of making the
    /// agent probe again. The agent discovers it on its own if nobody does.
    pub fn set_context_window(&mut self, window: Option<u32>) {
        self.context_window = window;
        self.context_window_probed = window.is_some();
        self.publish_chrome();
    }

    /// The context window in force, if known.
    pub fn context_window(&self) -> Option<u32> {
        self.context_window
    }

    /// Working directory the tools operate in.
    pub fn cwd(&self) -> std::path::PathBuf {
        self.ctx.cwd.clone()
    }

    /// The resolved filesystem confinement this agent's tools are held to — the
    /// same `Arc` every tool call reads, so a frontend cannot be looking at a
    /// policy the tools do not have.
    pub fn sandbox_policy(&self) -> Arc<hrdr_tools::SandboxPolicy> {
        Arc::clone(&self.ctx.sandbox)
    }

    /// Whether this agent is read-only scoped — its registry was pruned to the
    /// read-only tool set, so it holds no writers.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Change the tools' working directory. Reloads `AGENTS.md` for the new
    /// directory and refreshes the system prompt in place.
    pub fn set_cwd(&mut self, cwd: std::path::PathBuf) {
        self.ctx.cwd = cwd;
        self.refresh_system();
    }

    /// The rendered system prompt currently in effect (message 0).
    pub fn system_prompt(&self) -> Option<String> {
        self.messages
            .first()
            .filter(|m| m.role == Role::System)
            .and_then(|m| m.content.clone())
    }

    /// Active shell guardrails as `(pattern, message)` pairs — built-ins plus
    /// any `[[guardrails]]` config extras (for `/guardrails`) — followed by any
    /// config entry whose regex did not compile, marked as not active. A rule
    /// that silently failed to load looks exactly like one that was never
    /// written; listing it is what tells the two apart.
    pub fn guardrail_specs(&self) -> Vec<(String, String)> {
        self.ctx
            .guardrails
            .iter()
            .map(|g| (g.pattern.as_str().to_string(), g.message.clone()))
            .chain(
                self.invalid_guardrails
                    .iter()
                    .map(|(pattern, err)| (pattern.clone(), format!("NOT ACTIVE — {err}"))),
            )
            .collect()
    }

    /// Registered tools as `(name, description)` pairs.
    pub fn tools(&self) -> Vec<(String, String)> {
        self.tools
            .defs()
            .into_iter()
            .map(|d| (d.function.name, d.function.description))
            .collect()
    }

    /// Sampling temperature, if set.
    pub fn temperature(&self) -> Option<f32> {
        self.client.temperature
    }

    /// Whether prompt caching is active for the current endpoint (see
    /// [`resolve_cache_mode`]).
    pub fn prompt_cache_active(&self) -> bool {
        resolve_cache_mode(self.prompt_cache.as_deref(), self.client.base_url())
            == hrdr_llm::CacheMode::Ephemeral
    }

    /// Set (or clear) the sampling temperature.
    pub fn set_temperature(&mut self, t: Option<f32>) {
        self.client.temperature = t;
    }

    /// Set (or clear) the reasoning-effort label. Sent as `reasoning_effort` on
    /// each request when it names a known level; other labels are display-only.
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.client.set_effort(effort);
        self.publish_delegation_runtime();
    }

    /// Shared TODO list, mutated by the `todo` tool.
    pub fn todos(&self) -> Arc<Mutex<Vec<TodoItem>>> {
        self.ctx.todos.clone()
    }

    /// Shared goal list, mutated by the `goal` tool, read by the turn-end
    /// nudge.
    pub fn goals(&self) -> Arc<Mutex<Vec<GoalItem>>> {
        self.ctx.goals.clone()
    }

    /// Shared recurring-reminder list, mutated by the `cron` tool.
    pub fn crons(&self) -> Arc<Mutex<Vec<hrdr_tools::CronItem>>> {
        self.ctx.crons.clone()
    }

    /// The ids of crons whose scheduler tasks are armed — test-only observable
    /// for the resume/re-arm path (the field is `pub(crate)` in hrdr-tools).
    #[cfg(test)]
    pub fn cron_armed_for_test(&self) -> std::collections::HashSet<u64> {
        self.ctx
            .cron_armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Spawn a scheduler task for every cron in the shared list (idempotent: a
    /// cron whose scheduler already runs is skipped). Called on session resume
    /// so restored crons keep firing, and after a `/clear` that wiped the list
    /// there is nothing to arm.
    pub fn arm_crons(&self) {
        hrdr_tools::arm_crons(&self.ctx);
    }

    /// Shared registry of detached background sub-agents (for the frontend's
    /// live panel). Mutated by the `task` tool's `background` mode.
    pub fn background_tasks(&self) -> Arc<Mutex<Vec<hrdr_tools::BackgroundTask>>> {
        self.ctx.background_tasks.clone()
    }

    /// The sub-agents this agent is holding — the frontend steers, displays, and
    /// drives further turns on them through this handle. See [`AgentRegistry`].
    pub fn registry(&self) -> AgentRegistry {
        self.registry.clone()
    }

    /// Number of messages currently in history (including the system prompt).
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Set how long a completed TODO lingers before ageing out (a frontend may
    /// carry the user's preference for this).
    pub fn set_todo_ttl(&mut self, ttl: u64) {
        self.todo_ttl = ttl;
    }

    /// Learn this agent's context window if the config did not supply one, using
    /// the **local model catalog only**.
    ///
    /// The agent has always been *able* to ask the endpoint
    /// ([`Agent::probe_context_window`]) but never did so for itself — only
    /// frontends probed, and they kept the answer in frontend state. So a headless
    /// run, and every delegated sub-agent, had `context_window: None` and could
    /// never work out that it was full.
    ///
    /// Deliberately no HTTP here: this runs inside a turn, and firing an
    /// out-of-band request at the endpoint mid-turn is a surprise nobody asked for
    /// (it also interleaves with the very stream we are about to open). Endpoint
    /// probing stays where it belongs — at the edges, in `Agent::new`'s caller and
    /// on a provider switch — and whoever does it hands the figure over with
    /// [`Agent::set_context_window`]. Consulted once per model.
    pub(crate) fn ensure_context_window(&mut self) {
        if self.context_window_probed {
            return;
        }
        self.context_window_probed = true;
        // The window the identity resolved to — `(endpoint, model)`, network-free.
        self.context_window = self.resolved.context_window();
    }

    /// Forget what we knew about the window — the model or endpoint changed, so
    /// the old figure describes a different model. It is re-learned on demand.
    fn invalidate_context_window(&mut self) {
        self.context_window = None;
        self.context_window_probed = false;
        self.self_compact_failed_at = None;
    }
}

//! Local model-backend bootstrap.
//!
//! hrdr can bring up a local OpenAI-compatible server so the harness works with
//! zero setup. It is **presence-aware and infr-first**: if [`infr`] is on
//! `PATH` it's spawned as the backend (native `tools`/`tool_calls`, SSE, and a
//! GGUF Jinja chat template); otherwise it falls back to `llama-server`
//! (llama.cpp, started with `--jinja`). If neither is installed, hrdr errors and
//! points at `--no-backend` + `--base-url` for a self-managed endpoint.
//!
//! A backend already answering at `--base-url` is reused as-is (nothing is
//! spawned or owned). Remote providers never reach this module.
//!
//! [`infr`]: https://github.com/kryptic-sh/infr

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hrdr_llm::Client;
use tokio::process::{Child, Command};

/// How a managed backend was provisioned.
pub enum Backend {
    /// We launched a local server; it is killed when this value drops (the held
    /// `Child` is a kill-on-drop RAII guard, never read directly). Boxed so the
    /// enum stays small (the `Child` is larger on Windows).
    Spawned(#[allow(dead_code)] Box<Child>),
    /// A backend was already reachable; we reuse it and own nothing.
    External,
}

/// Which local server to spawn, chosen by what's installed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    /// `infr serve <model> --addr <ip:port>` — preferred (native tool calls).
    Infr,
    /// `llama-server -hf <model> --jinja …` — llama.cpp fallback.
    Llama,
}

/// Settings for the spawned local backend.
#[derive(Clone)]
pub struct BackendConfig {
    /// Model ref (HF `org/repo[:quant]`) or path to a local `.gguf`. Both infr
    /// and llama.cpp accept this shape.
    pub model: String,
    /// `llama-server` binary name or path (used only for the llama fallback).
    pub bin: String,
    /// Context window size — passed to llama.cpp (`-c`) and used for the status
    /// bar's "X of Y" display. infr sizes context per request, so it's display
    /// only there.
    pub ctx: u32,
    /// Extra args passed verbatim to the **llama.cpp** fallback (e.g. `-ngl 99`
    /// for GPU offload). Not forwarded to infr, which is tuned via `INFR_*` env
    /// vars rather than CLI flags.
    pub extra_args: Vec<String>,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            model: "unsloth/Qwen3-30B-A3B-GGUF:Q4_K_M".to_string(),
            bin: "llama-server".to_string(),
            ctx: 16384,
            extra_args: Vec::new(),
        }
    }
}

impl Backend {
    /// Ensure a backend answers at `base_url`. Reuse one if already up; else
    /// spawn `infr` (preferred) or `llama-server` (fallback) and block until it
    /// is ready.
    pub async fn ensure(cfg: &BackendConfig, base_url: &str) -> Result<Self> {
        let probe = Client::new(base_url, None, "default");
        if probe.list_models().await.is_ok() {
            eprintln!("hrdr: reusing existing backend at {base_url}");
            return Ok(Backend::External);
        }

        let (host, port) = parse_host_port(base_url)?;

        // Presence-aware: prefer infr for its native tool support, fall back to
        // llama.cpp, error if neither is installed.
        let kind = if which::which("infr").is_ok() {
            BackendKind::Infr
        } else if which::which(&cfg.bin).is_ok() {
            BackendKind::Llama
        } else {
            bail!(
                "no local backend found on PATH — install `infr` (preferred, native tool \
                 calling) or `llama-server` (llama.cpp), or run your own OpenAI-compatible \
                 server and start hrdr with `--no-backend --base-url <url>`"
            );
        };

        let log_path = log_file(kind);
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("creating {}", log_path.display()))?;
        let log_err = log.try_clone()?;

        let (label, mut command) = match kind {
            BackendKind::Infr => {
                // `infr serve <model> --addr <ip:port>`. The model is a required
                // positional (HF ref or .gguf); tuning is via `INFR_*` env vars.
                let mut c = Command::new("infr");
                c.arg("serve")
                    .arg(&cfg.model)
                    .arg("--addr")
                    .arg(format!("{host}:{port}"));
                ("infr serve", c)
            }
            BackendKind::Llama => {
                // `--jinja` is REQUIRED: it enables the chat template that injects
                // the tool definitions and parses the model's tool calls back
                // into the OpenAI shape. Without it the model never sees tools.
                let mut c = Command::new(&cfg.bin);
                c.arg("-hf")
                    .arg(&cfg.model)
                    .arg("--jinja")
                    .arg("-c")
                    .arg(cfg.ctx.to_string())
                    .arg("--host")
                    .arg(&host)
                    .arg("--port")
                    .arg(port.to_string())
                    .args(&cfg.extra_args);
                ("llama-server", c)
            }
        };

        eprintln!(
            "hrdr: starting {label} ({}) on {host}:{port} — loading model, this can take a \
             minute…\n      logs: {}",
            cfg.model,
            log_path.display(),
        );

        let child = command
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning `{label}` — see {}", log_path.display()))?;

        if !wait_ready(&probe, Duration::from_secs(300)).await {
            bail!(
                "{label} did not become ready within 5 min — see {}",
                log_path.display()
            );
        }
        eprintln!("hrdr: backend ready.");
        Ok(Backend::Spawned(Box::new(child)))
    }
}

async fn wait_ready(client: &Client, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if client.list_models().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// Extract `(host, port)` from a base URL like `http://localhost:8080/v1`.
/// `localhost` is normalised to `127.0.0.1` (infr's `--addr` needs a literal IP,
/// and it's a valid bind address for llama.cpp too).
fn parse_host_port(base_url: &str) -> Result<(String, u16)> {
    let after = base_url.split("://").nth(1).unwrap_or(base_url);
    let authority = after.split('/').next().unwrap_or(after);
    let (host, port) = authority
        .split_once(':')
        .context("base_url must include host:port to spawn a backend")?;
    let host = if host == "localhost" {
        "127.0.0.1"
    } else {
        host
    };
    let port: u16 = port.parse().context("invalid port in base_url")?;
    Ok((host.to_string(), port))
}

fn log_file(kind: BackendKind) -> std::path::PathBuf {
    let dir = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".cache")
        })
        .join("hrdr");
    let _ = std::fs::create_dir_all(&dir);
    let name = match kind {
        BackendKind::Infr => "infr-serve.log",
        BackendKind::Llama => "llama-server.log",
    };
    dir.join(name)
}

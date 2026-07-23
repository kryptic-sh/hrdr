# Web UI for hrdr sessions

Status: **planning (living doc — updated as we discuss).** Date: 2026-07-23.

Serve an hrdr session over a web endpoint so it can be driven from a browser
(desktop or phone) with full feature parity to the TUI, behind authentication.

## Decisions so far

- **Deployment:** both, **headless first**. Ship `hrdr serve` (the web is the
  frontend, no TUI) first; design the internals so _attaching to a live TUI
  session_ (drive one session from the TUI **and** browser at once) is a later,
  additive capability.
- **Network exposure:** **config-gated**. Bind `127.0.0.1` by default; exposing
  on a non-loopback address requires an explicit flag **and** auth **and** TLS.
- **Scope target:** **full parity** — every command, picker, pane, todo, and
  status section the TUI has, adapted to web/mobile idioms (parity of
  _capability_, not pixel-identity). Built in an internal order (below), but the
  first release targets parity.
- **Reuse as a native GUI:** the web frontend + its HTTP/WS protocol is the
  **single** UI implementation. A future desktop/mobile GUI app is a **thin
  native shell** (a system webview) that embeds the server and loads the _same_
  SPA over the _same_ protocol — not a second UI codebase. This is why
  `hrdr-web` is an **embeddable library** (see below), the transport is plain
  localhost WS (identical in a browser and a webview), and the client avoids
  browser-only APIs.

## Why this is feasible (the leverage)

hrdr's core is already UI-agnostic; the TUI is one frontend. A web UI is a
**peer** frontend over the same core, not a rewrite:

- **Serializable transcript.** `Entry`/`EntryKind` + `apply_event` +
  `tool_display` live in `hrdr-agent`, TUI-free. The server folds the event
  stream server-side and pushes `Entry` (or a derived render model) as JSON; the
  browser only renders.
- **Event log with per-reader cursors.**
  `LiveSubagents::events_since(key, cursor)` already supports many readers
  replaying from their own cursor — a browser is just another reader. This is
  also **free reconnect/replay** for flaky mobile networks (resume from the last
  cursor).
- **Unified input path.** Every user message is a queued `Steer`; the browser
  injects input exactly as the TUI does — no special path.
- **Shared command layer.** `CommandHost`/dispatch in `hrdr-app` runs every
  slash command. The web client calls the same dispatch, so pickers/commands are
  render + input over shared logic, not reimplemented behavior.

## Architecture

```
browser (SPA, embedded assets)
   │  WebSocket (events↓ / input↑) + HTTP (auth, assets)
   ▼
hrdr-web  (new crate: axum HTTP+WS server + auth)
   │  hosts a "Session" = Agent + PaneSet + LiveSubagents + steering + CommandHost
   ▼
hrdr-app / hrdr-agent  (the same core the TUI drives)
```

- **New crate `hrdr-web`** — an **embeddable library** exposing
  `serve(session, config) -> RunningServer` (axum HTTP + WebSocket), plus a thin
  `hrdr serve` binary wrapper. Depends on `hrdr-app` (the core) + web/auth deps
  (isolated here; does not bloat the core or TUI). The library shape is what
  lets a native GUI shell embed the server in-process (see _Reuse as a native
  GUI_).
- **Client SPA** — responsive HTML/CSS/JS, **embedded in the binary** (e.g.
  `rust-embed`/`include_dir`) so `hrdr serve` is self-contained. Keep it
  buildless or with a compile-time bundle step so a release ships one binary.
- **The `Session` abstraction is the seam.** Model the server around a shareable
  session (event stream + steering queue + command dispatch), decoupled from the
  TUI's `App` view-state. Headless mode owns the session directly; attach-mode
  (v2) lets a running TUI expose its session to an embedded server, making the
  browser a second reader/input-source — additive, no App rewrite for MVP.

### WebSocket protocol (sketch — to refine)

- **server → client:** `snapshot` (on connect: full transcript, status, panes,
  todos, model/provider, turn state, cursor); `entries` (Entry deltas as the
  turn streams); `status` + `turn` (status bar, tok/s, ctx, cost); `panes`
  (sub-agent list + active); `todos`; `notice`.
- **client → server:** `submit` (input → steering queue); `command` (slash
  command → `CommandHost`); `steer` (mid-turn); `cancel`; `switch_pane`. Pickers
  (model/theme/session) resolve to `command` (`/model …`, `/theme …`,
  `/resume …`), so no bespoke protocol per picker.
- **Rendering single-source-of-truth.** To avoid drift, the server computes the
  display model (reusing `tool_display`/`ToolBody`, diff classification, etc.)
  and the client renders it dumbly — don't re-implement fold/classify logic in
  JS.

## Reuse as a native GUI app

The web UI is the one UI. A native desktop/mobile app is a **shell** around it:

```
native shell (window, tray, notifications, OS integration)
   ├─ embeds hrdr-web (in-process; owns/attaches the Session)
   └─ system webview → loads the same embedded SPA over localhost WS
```

- **One transport for both worlds.** The client always talks localhost WS +
  HTTP. In a browser that's a real socket; in a webview it's the same socket to
  the embedded server. No separate "desktop" client, no divergent code path — a
  bug fixed in the web UI is fixed in the GUI app.
- **`hrdr-web` as a library is the enabler.** The shell calls
  `hrdr_web::serve(session, config)` on `127.0.0.1:<ephemeral>` and points its
  webview at it. The `hrdr serve` binary is the same call with a CLI wrapper.
- **Client portability rules.** No browser-only APIs a webview may lack;
  feature- detect and degrade (clipboard, notifications, file pickers). Native
  niceties (real notifications, file dialogs, deep links) are added by the shell
  and exposed to the client through a small, optional capability bridge — never
  required for the web-only case.
- **Auth is simpler in the shell case.** The shell controls both ends, so it can
  bind loopback-only and inject a per-launch bearer token, skipping a login
  screen while still authenticating the socket. The same auth backends still
  apply when the shell chooses to expose on the network.
- **Candidate shells:** a Rust-native webview (e.g. Tauri/`wry`) keeps
  everything in one toolchain and can embed `hrdr-web` directly; a mobile app
  wraps a `WebView`/`WKWebView` over the embedded (or remote) server. Either way
  the SPA and protocol are unchanged.

## Authentication & security (primary constraint)

A web endpoint exposes a **coding agent with full filesystem + shell access as
the user** — anyone who reaches it can run arbitrary commands. Security is the
design's spine, not an add-on.

- **Two credential backends (config-selected):**
  - **Basic HTTP auth** — `Authorization: Basic …`, 401 challenge. Simple,
    browser-native. Credentials from config (store a **hash**, not plaintext).
    The WS authenticates via the upgrade request's `Authorization` header.
  - **SQLite user table** — `users(username, password_hash, …)` with
    **argon2/bcrypt** hashes; a login endpoint mints a **signed session
    cookie**; the WS authenticates via the cookie. Supports multiple users and
    rotation.
- **Config-gated exposure:** default bind `127.0.0.1`. Binding a non-loopback
  address requires an explicit flag **+** a configured credential backend **+**
  TLS (own cert or a reverse proxy). Refuse to expose on `0.0.0.0` without all
  three. Note: **basic auth over plain HTTP is cleartext** — only allowed on
  loopback/tunnel; network exposure forces TLS.
- **Hardening:** origin/CSRF checks on the WS upgrade; auth-failure
  rate-limiting + lockout; constant-time credential comparison; session-cookie
  `HttpOnly`/`Secure`/`SameSite`; optional per-session bearer token in the URL
  for quick loopback access. Consider a read-only/observer mode (view, no input)
  as a lower-risk sharing option.
- The SQLite DB path, TLS cert/key paths, bind address, and backend choice all
  live under a `[web]` config section (+ `HRDR_WEB_*` env + CLI flags), matching
  hrdr's config/env/flag convention.

## Feature-parity map (TUI → web)

| Capability         | Drives via                                    | Web rendering                              |
| ------------------ | --------------------------------------------- | ------------------------------------------ |
| Transcript         | `Entry` stream (server fold)                  | markdown, code highlight, tool/diff blocks |
| Send / steer input | steering queue (`Steer`)                      | input box; send / cancel                   |
| Slash commands     | `CommandHost`/dispatch                        | command palette + typed `/…`               |
| Sub-agent panes    | `LiveSubagents` + `PaneSet`                   | tabs / drawer, live per-pane transcript    |
| Todos              | the todo list                                 | collapsible panel                          |
| Pickers (model/…)  | `/model`, `/theme`, `/effort`, `/resume` cmds | bottom sheets / menus                      |
| Status bar         | status model + turn stats                     | header/footer chips                        |
| Sessions           | `/new`, `/resume`, session list               | session switcher                           |
| Reasoning, notices | `Entry` kinds                                 | collapsible thinking, toasts               |

Terminal-only bits (vim editing, keybinds, the loader animation) become their
web/mobile equivalents; the underlying capability is preserved because the logic
is in the shared core.

## Mobile

Chat maps naturally to mobile: a scrollable transcript + a sticky input.
Responsive layout, touch targets, virtual-keyboard-aware input; pane switcher →
tabs/drawer; pickers → bottom sheets; status → compact chips.
Reconnect-on-resume via the event cursor so backgrounding the browser doesn't
lose the stream.

## Build order (internal; ship target = full parity)

1. **`hrdr-web` skeleton** — axum server, embedded static client, one WS, the
   `Session` seam; headless `hrdr serve` on a saved/new session.
2. **Auth + exposure gating** — basic + SQLite backends, config-gated bind, TLS
   path, rate-limit. (Land before any non-loopback exposure is possible.)
3. **Core chat loop** — snapshot + streaming `Entry` deltas + input + steer +
   cancel; server-side display model; mobile-responsive shell.
4. **Panes + todos + status** — sub-agent tabs, live per-pane views, todo panel,
   status/turn chips.
5. **Commands + pickers + sessions** — command palette over `CommandHost`;
   model/ theme/effort/session pickers as sheets; session switcher.
6. **v2 — attach to a live TUI session** — a running TUI exposes its session to
   the embedded server; browser as second reader/input-source. Concurrency
   handled at the `Session` seam.
7. **Native GUI shell** — a thin webview shell (e.g. Tauri/`wry`) that embeds
   `hrdr-web` and loads the same SPA, plus optional native capabilities
   (notifications, file dialogs, tray). No new UI code — reuse the web frontend.
   Keep `hrdr-web` a clean library from step 1 so this is purely additive.

## Open questions

- **Client stack:** buildless vanilla JS vs a light framework with a
  compile-time bundle — trade simplicity of embedding against parity/mobile
  ergonomics.
- **Rendering model shape:** exact JSON the server sends per `EntryKind` (how
  much the server pre-renders vs the client formats).
- **Multi-session:** does one `hrdr serve` host a single session or a session
  browser (list + open)? Full parity implies session management; scope the first
  cut.
- **Concurrency for v2 attach:** conflict rules when TUI and browser both send
  input mid-turn (the steering queue already serializes; confirm the UX).
- **TLS story:** built-in rustls vs "bring a reverse proxy" as the documented
  path for network exposure.
- **GUI shell toolchain:** Tauri/`wry` (Rust-native, embeds `hrdr-web` directly)
  vs a lighter custom webview vs per-platform mobile wrappers — and whether the
  shell ever needs a native IPC bridge or the localhost WS is always sufficient
  (leaning: WS is enough; a bridge is optional sugar for native capabilities).

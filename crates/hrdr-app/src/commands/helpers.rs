use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::SessionState;
use hrdr_agent::Agent;
use tokio::sync::Mutex;

pub fn busy_guard(action: &str) -> String {
    format!("can't {action} while a turn is running")
}

pub fn busy_generic() -> String {
    "busy — try again after the current turn".to_string()
}

/// `/verbose` status lines.
pub mod expand_msg {
    pub const ALL: &str = "verbose mode on";
    pub const OFF: &str = "verbose mode off";
}

/// `/reload` + hot-reload status lines.
pub const RELOAD_MANUAL_MSG: &str = "reloaded config (theme, effort, toggles)";

/// Hot-reload notice, naming the config file that changed (home collapsed to
/// `~`). Falls back to the bare notice when there's no resolvable config path
/// (no `HOME` / `XDG_CONFIG_HOME`).
pub fn reload_hot_message() -> String {
    match hrdr_agent::config_file_path() {
        Some(p) => format!("config reloaded ({})", crate::display_dir(&p)),
        None => "config reloaded".to_string(),
    }
}
/// Invalid config file on reload: keep the current settings and warn.
pub fn reload_invalid_message(e: &dyn std::fmt::Display) -> String {
    format!("config invalid — keeping current settings: {e}")
}

/// Startup notice when `AGENTS.md` was gathered into the system prompt.
pub const PROJECT_DOCS_LOADED_MSG: &str = "loaded project instructions from AGENTS.md";

/// Shown by `/new` when the `AGENTS.md` it just re-read differs from the one that
/// was in the prompt — the only point at which project docs are re-seeded.
///
/// A running conversation is never re-seeded: the agent that edited the file has
/// the change in its context already, and another session that wants it starts a
/// new conversation.
pub const PROJECT_DOCS_RELOADED_MSG: &str =
    "AGENTS.md changed on disk — reloaded into the system prompt";

/// Startup notice for non-fatal config problems the TUI should surface: the
/// agent-side env-override warnings and the UI-side enum warnings, combined into
/// one block (each dropped-and-defaulted value on its own line). `None` when the
/// config is clean.
///
/// Hard config errors do NOT come through here — `main` prints and exits on
/// those before any frontend starts (see `hrdr_agent::ConfigDiagnostics`), so by
/// the time the TUI is drawing, only warnings remain.
pub fn startup_config_warning() -> Option<String> {
    let (_, agent) = hrdr_agent::AgentConfig::load_diagnosed();
    let (_, ui_warnings) = crate::UiConfig::load_diagnosed();
    let mut lines: Vec<String> = agent.warnings;
    lines.extend(ui_warnings);
    if lines.is_empty() {
        return None;
    }
    Some(format!("configuration warnings:\n  {}", lines.join("\n  ")))
}

/// Guard shown when `/resume` is attempted mid-turn (the running turn holds
/// the agent mutex: the message swap would silently no-op while the transcript
/// and session id switched, and the turn's autosave would then overwrite the
/// resumed session's file with the old conversation).
pub const RESUME_BUSY_MSG: &str = "a turn is running — interrupt it before /resume";

/// What restoring a session changes beyond the host's own state swap: the
/// working directory to adopt (if any) and the system lines to show, in order.
pub struct ResumePlan {
    /// The session's cwd when it exists and differs from the current one.
    pub new_cwd: Option<PathBuf>,
    /// Notices: the "resumed …" line, then cwd / missing-cwd / endpoint notes.
    pub lines: Vec<String>,
}

/// The `/resume` semantics: follow the session's working directory (in-process
/// only) and surface the notices that go with it.
pub fn resume_plan(session: &SessionState, prev_cwd: &Path, current_base_url: &str) -> ResumePlan {
    let mut lines = vec![format!(
        "resumed '{}' ({} messages)",
        session.name,
        session.messages.len()
    )];
    let mut new_cwd = None;
    if !session.cwd.is_empty() && Path::new(&session.cwd) != prev_cwd {
        let target = PathBuf::from(&session.cwd);
        if target.is_dir() {
            lines.push(format!("cwd → {}", target.display()));
            new_cwd = Some(target);
        } else {
            lines.push(format!(
                "note: session cwd {} no longer exists; staying in {}",
                session.cwd,
                prev_cwd.display()
            ));
        }
    }
    if session.base_url != current_base_url {
        lines.push(format!(
            "note: session endpoint was {} (current: {current_base_url})",
            session.base_url
        ));
    }
    ResumePlan { new_cwd, lines }
}

/// Minimum turn duration before the finish nudge fires (the TUI's terminal
/// bell) — quick replies stay silent.
pub const BELL_MIN_SECS: f64 = 5.0;

/// Whether a finished turn warrants the nudge: the knob is on and the turn ran
/// at least [`BELL_MIN_SECS`].
pub fn should_bell(enabled: bool, elapsed_secs: Option<f64>) -> bool {
    enabled && elapsed_secs.is_some_and(|e| e >= BELL_MIN_SECS)
}

/// The cancel notice (with the discarded-queue count).
pub fn cancel_message(dropped: usize) -> String {
    if dropped > 0 {
        format!("[cancelled · {dropped} queued message(s) discarded]")
    } else {
        "[cancelled]".to_string()
    }
}

/// The cancel notice when messages the user typed mid-turn were put back into
/// the composer rather than sent or dropped.
///
/// `lines` counts what came *off the queue*, not what the composer now holds —
/// anything half-typed at the moment of the cancel is still theirs and is not
/// news.
pub fn cancel_message_restored(lines: usize) -> String {
    let plural = if lines == 1 { "line" } else { "lines" };
    format!("[cancelled · {lines} queued {plural} put back in the input]")
}

/// The one-time notice when a session file is first created.
pub fn session_saved_notice(id: &str) -> String {
    format!("session saved as '{id}' — /resume {id}")
}

/// How a clipboard write went. Callers that only want the status line take
/// [`clipboard_copy_status`]; callers that also grade the outcome (a toast's
/// severity, say) match on this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardWrite {
    Copied,
    Failed,
    /// The platform has no clipboard at all.
    Unavailable,
}

/// Copy `text` to the OS clipboard. `cb` is the frontend's long-lived clipboard
/// handle (`None` when the platform has none).
pub fn clipboard_copy(cb: &mut Option<hjkl_clipboard::Clipboard>, text: &str) -> ClipboardWrite {
    use hjkl_clipboard::{MimeType, Selection};
    match cb
        .as_mut()
        .map(|cb| cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()))
    {
        Some(Ok(())) => ClipboardWrite::Copied,
        Some(Err(_)) => ClipboardWrite::Failed,
        None => ClipboardWrite::Unavailable,
    }
}

/// Copy `text` to the OS clipboard, returning the status line to show.
pub fn clipboard_copy_status(
    cb: &mut Option<hjkl_clipboard::Clipboard>,
    text: &str,
    label: &str,
) -> String {
    match clipboard_copy(cb, text) {
        ClipboardWrite::Copied => format!("copied {label} to clipboard"),
        ClipboardWrite::Failed => "clipboard write failed".to_string(),
        ClipboardWrite::Unavailable => "clipboard unavailable".to_string(),
    }
}

/// Read the OS clipboard as text (`/paste`).
pub fn clipboard_read_text(cb: &Option<hjkl_clipboard::Clipboard>) -> Option<String> {
    use hjkl_clipboard::{MimeType, Selection};
    let bytes = cb
        .as_ref()
        .and_then(|cb| cb.get(Selection::Clipboard, MimeType::Text).ok())?;
    Some(String::from_utf8_lossy(&bytes).to_string())
}

/// What a paste found on the clipboard — the whole answer, so the frontend
/// branches once and every outcome says something.
///
/// The variants are ordered the way [`clipboard_paste`] prefers them: image or
/// PDF bytes first, then a copied *file* (a `text/uri-list`, which is how a file
/// manager spells "I copied this file"), then text. A clipboard that holds an
/// image and its own text description is being pasted for the image.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardPaste {
    /// An image or PDF, already validated against its own bytes.
    Media(hrdr_tools::Attachment),
    /// Text, as `Ctrl+]` has always pasted it.
    Text(String),
    /// The clipboard is readable and holds nothing usable.
    Empty,
    /// There is no clipboard on this platform, or it cannot be read at all.
    Unavailable,
    /// Something was there and could not be taken: a type hrdr cannot attach, a
    /// file URI that would not read, or a backend that cannot do images. Carries
    /// the message to show — the one outcome that must never look like an empty
    /// clipboard.
    Refused(String),
}

/// The file extension for a media type, for naming bytes that arrived without a
/// file name of their own.
fn media_extension(mt: hrdr_tools::MediaType) -> &'static str {
    use hrdr_tools::MediaType;
    match mt {
        MediaType::Jpeg => "jpg",
        MediaType::Png => "png",
        MediaType::Gif => "gif",
        MediaType::Webp => "webp",
        MediaType::Pdf => "pdf",
    }
}

/// One attachment, as a person reads it: `shot.png (image/png, 12.4 KB)`.
///
/// The size is derived from the attachment's `encoded_len` — the base64 length, which is the number the request's own ceiling is checked
/// against — scaled back to raw bytes. That scaling is exact to within base64's
/// padding (at most two bytes), which no rendered size can show; the attachment
/// does not expose its raw length.
pub fn attachment_summary(a: &hrdr_tools::Attachment) -> String {
    let bytes = a.encoded_len() / 4 * 3;
    let size = if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    };
    format!("{} ({}, {size})", a.filename(), a.media_type().mime())
}

/// Take whatever is on the system clipboard, preferring an attachment over text.
///
/// Three shapes reach hrdr as "an image on the clipboard", and this handles all
/// three because a user cannot tell them apart: raw image bytes (a screenshot
/// tool, a browser's "copy image"), a `text/uri-list` naming a file (what a file
/// manager's copy puts there — the common case on Linux), and plain text, which
/// stays plain text.
///
/// `paste_stem` names the bytes that arrive without a file behind them; the
/// extension comes from the type the bytes actually sniff as, never from what
/// the clipboard called them. `cwd` resolves a relative URI, though a uri-list
/// entry is required by the format to be absolute.
pub fn clipboard_paste(
    cb: &Option<hjkl_clipboard::Clipboard>,
    cwd: &Path,
    paste_stem: &str,
) -> ClipboardPaste {
    use hjkl_clipboard::{Capabilities, MimeType, Selection};

    let Some(cb) = cb.as_ref() else {
        return ClipboardPaste::Unavailable;
    };
    let caps = cb.capabilities();
    // A backend without AVAILABLE cannot enumerate, so an empty list is "don't
    // know", not "nothing there" — the probes below fall back to asking directly.
    let offered = cb.available(Selection::Clipboard).unwrap_or_default();

    if caps.contains(Capabilities::IMAGE)
        && let Some(found) = clipboard_media(cb, &offered, paste_stem)
    {
        return found;
    }
    if caps.contains(Capabilities::URI_LIST)
        && let Some(found) = clipboard_file_uri(cb, &offered, cwd)
    {
        return found;
    }
    match cb.get(Selection::Clipboard, MimeType::Text) {
        Ok(bytes) if !bytes.is_empty() => {
            ClipboardPaste::Text(String::from_utf8_lossy(&bytes).into_owned())
        }
        // Readable, and holding nothing — unless the backend cannot do images at
        // all, in which case "empty" would be hiding the reason an image the user
        // can see on their clipboard did not arrive.
        Ok(_) if caps.contains(Capabilities::IMAGE) => ClipboardPaste::Empty,
        Ok(_) => ClipboardPaste::Refused(format!(
            "nothing to paste — the {} clipboard backend can't read images",
            cb.kind()
        )),
        Err(_) => ClipboardPaste::Unavailable,
    }
}

/// Image/PDF bytes off the clipboard proper, or `None` when it is offering none.
fn clipboard_media(
    cb: &hjkl_clipboard::Clipboard,
    offered: &[hjkl_clipboard::MimeType],
    paste_stem: &str,
) -> Option<ClipboardPaste> {
    use hjkl_clipboard::{MimeType, Selection};

    // PNG first — the type every backend translates natively — then whatever
    // else the clipboard says it has. A backend that cannot enumerate still gets
    // the PNG probe, which is what a screenshot lands as on all three platforms.
    let mut candidates = vec![MimeType::Png];
    for m in offered {
        if let MimeType::Custom(name) = m
            && (name.starts_with("image/") || name == "application/pdf")
            && !candidates.contains(m)
        {
            candidates.push(m.clone());
        }
    }
    let mut refusal = None;
    for mime in candidates {
        let Ok(bytes) = cb.get(Selection::Clipboard, mime) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        // The bytes decide, exactly as they do for a file: a clipboard flavour is
        // a claim, and a backend that hands back something else is why this is
        // sniffed rather than trusted.
        let Some(mt) = hrdr_tools::MediaType::sniff(&bytes) else {
            refusal = Some(ClipboardPaste::Refused(
                "the clipboard image isn't a type hrdr can attach \
                 (PNG, JPEG, GIF, WebP or PDF)"
                    .to_string(),
            ));
            continue;
        };
        let name = format!("{paste_stem}.{}", media_extension(mt));
        return match hrdr_tools::Attachment::new(bytes, mt, name) {
            Ok(a) => Some(ClipboardPaste::Media(a)),
            Err(e) => Some(ClipboardPaste::Refused(format!("can't attach: {e}"))),
        };
    }
    refusal
}

/// A file copied in a file manager: `text/uri-list` → the path → the same
/// byte-sniffing read an `@mention` gets. `None` when there is no file URI, or
/// when the file it names is not something hrdr attaches — a copied `.txt` falls
/// through to the text path rather than being refused.
fn clipboard_file_uri(
    cb: &hjkl_clipboard::Clipboard,
    offered: &[hjkl_clipboard::MimeType],
    cwd: &Path,
) -> Option<ClipboardPaste> {
    use hjkl_clipboard::{MimeType, Selection, Uri};

    if !offered.is_empty() && !offered.contains(&MimeType::UriList) {
        return None;
    }
    let uris = cb.get_uri_list(Selection::Clipboard).ok()?;
    let mut refusal = None;
    for uri in uris {
        let Uri::File(path) = uri else {
            continue; // an https:// URI is a link, and pastes as its text
        };
        match hrdr_tools::read_attach_media(&path.to_string_lossy(), cwd) {
            Ok(Some(a)) => return Some(ClipboardPaste::Media(a)),
            // Not an image or PDF: the text path has the path itself to offer.
            Ok(None) => {}
            Err(e) => refusal = Some(ClipboardPaste::Refused(e.to_string())),
        }
    }
    refusal
}

/// The tools' working directory: the agent's cwd when the lock is free
/// (a turn may hold it), else the process cwd.
pub fn agent_cwd(agent: &Arc<Mutex<Agent>>) -> PathBuf {
    agent
        .try_lock()
        .map(|a| a.cwd())
        .ok()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// The names of sub-agents the live agent can delegate to (for `@name` mention
/// routing). Empty when the lock is held (a turn is running) or delegation is off.
pub fn agent_names(agent: &Arc<Mutex<Agent>>) -> Vec<String> {
    agent
        .try_lock()
        .map(|a| a.agent_names().to_vec())
        .unwrap_or_default()
}

/// The live agent's TODO list, for `todo#N` / `task#N` expansion in the send
/// path. Empty when the lock is held (a turn is running) or the list's own lock
/// is poisoned.
pub fn agent_todos(agent: &Arc<Mutex<Agent>>) -> Vec<hrdr_tools::TodoItem> {
    agent
        .try_lock()
        .map(|a| a.todos_owned())
        .unwrap_or_default()
}

/// [`crate::prepare_outgoing`] for frontends holding the shared agent handle:
/// fetches the sub-agent names ([`agent_names`]) and cwd ([`agent_cwd`]) itself.
///
/// This is also where `@file` expansion meets the read-before-edit guard: every
/// file whose *whole* content the expansion inlined is marked read on `agent`
/// (via [`Agent::mark_files_read`]), so the model isn't sent back to re-read a
/// file already sitting verbatim in its context. Use it for messages delivered
/// **to** `agent`; for one merely prepared with its cwd/names and delivered
/// elsewhere, use [`prepare_outgoing_relayed`] so a file the agent never sees
/// doesn't disarm its guard.
///
/// `project` is [`Agent::project_instructions`], which the caller has to supply
/// rather than have this read: the path that needs it most is the *steer*, and
/// there a turn is running and holding the very lock every `try_lock` in this
/// module goes through. The value is fixed for the agent's life, so a frontend
/// reads it once at construction and keeps it.
pub fn prepare_outgoing_via(
    agent: &Arc<Mutex<Agent>>,
    input: &str,
    project: hrdr_agent::ProjectInstructions,
) -> crate::Outgoing {
    let out = crate::prepare_outgoing_tracked(
        input,
        &agent_names(agent),
        &agent_cwd(agent),
        project,
        &agent_todos(agent),
    );
    // Best-effort, and deliberately not blocking: a turn in flight holds the
    // lock, and the same `try_lock` gate already decides whether `@agent`
    // routing resolves at all (see `agent_names`). Missing the mark costs one
    // redundant read; waiting here would stall the frontend.
    if !out.inlined().is_empty()
        && let Ok(a) = agent.try_lock()
    {
        a.mark_files_read(out.inlined());
    }
    // Whole, attachments included: the caller turns it into one message with
    // `Outgoing::into_steer`, which carries the images *and* the label lines
    // naming them. A caller that genuinely cannot carry them takes
    // `Outgoing::into_text`, which drops both together — so an image mention
    // reads to the model as nothing at all rather than as a label for a picture
    // it never got.
    out
}

/// [`prepare_outgoing_via`] for a message expanded with `agent`'s cwd and
/// sub-agent names but delivered to a *different* agent (a sub-agent pane).
/// Identical expansion, no read-state change: the inlined content lands in the
/// recipient's context, not this one's, and marking it here would tell `agent` it
/// had seen a file it never received.
pub fn prepare_outgoing_relayed(
    agent: &Arc<Mutex<Agent>>,
    input: &str,
    project: hrdr_agent::ProjectInstructions,
) -> crate::Outgoing {
    // `&[]`: `todo#N` expansion resolves against the *receiving* agent's list, and
    // this message is not going to the one whose list is in reach here — the same
    // reason the read-state marking above is skipped.
    crate::prepare_outgoing_tracked(input, &agent_names(agent), &agent_cwd(agent), project, &[])
}

/// The working-tree `git diff` for `cwd` (stdout on success, stderr message on
/// failure). Shared by `/diff`.
pub async fn git_working_diff(cwd: &Path) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .arg("diff")
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod reload_message_tests {
    /// The hot-reload notice names the config file that changed. The path is
    /// whatever `config_file_path()` resolves to (home collapsed to `~`), so
    /// assert on its shape rather than an absolute path.
    #[test]
    fn hot_reload_notice_names_the_config_file() {
        let msg = super::reload_hot_message();
        assert!(msg.starts_with("config reloaded"), "{msg}");
        // With no HOME/XDG the path is unresolvable and the bare notice is used.
        if hrdr_agent::config_file_path().is_some() {
            assert!(msg.contains("config.toml"), "{msg}");
            assert!(msg.ends_with(')'), "{msg}");
        }
    }
}

#[cfg(test)]
mod clipboard_paste_tests {
    use super::{ClipboardPaste, attachment_summary, clipboard_paste};
    use hjkl_clipboard::backend::mock::MockBackend;
    use hjkl_clipboard::{BackendKind, Capabilities, Clipboard, MimeType, Selection, Uri};

    /// A PNG header plus filler — the smallest byte string that sniffs as one.
    fn png(pad: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.resize(v.len() + pad, 0);
        v
    }

    /// A clipboard backend advertising `caps` and answering nothing until the
    /// test programs it.
    fn mock(caps: Capabilities) -> MockBackend {
        MockBackend::new(BackendKind::Mock, caps)
    }

    /// Image bytes and a text description are both on the clipboard — the paste
    /// is for the image. (A browser's "copy image" leaves exactly this.)
    #[test]
    fn prefers_image_bytes_over_the_text_beside_them() {
        let dir = tempfile::tempdir().unwrap();
        let m = mock(Capabilities::all());
        m.preset_available(
            Selection::Clipboard,
            Ok(vec![MimeType::Png, MimeType::Text]),
        );
        m.preset_get(Selection::Clipboard, MimeType::Png, Ok(png(64)));
        m.preset_get(
            Selection::Clipboard,
            MimeType::Text,
            Ok(b"a screenshot".to_vec()),
        );
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        match clipboard_paste(&cb, dir.path(), "pasted-1") {
            ClipboardPaste::Media(a) => {
                assert_eq!(a.filename(), "pasted-1.png");
                assert_eq!(a.media_type(), hrdr_tools::MediaType::Png);
            }
            other => panic!("expected the image, got {other:?}"),
        }
    }

    /// A JPEG offered under a `Custom` mime is still named for what its bytes
    /// are, not for what the clipboard called it.
    #[test]
    fn names_a_pasted_image_after_the_type_its_bytes_sniff_as() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = MimeType::Custom("image/jpeg".to_string());
        let m = mock(Capabilities::all());
        m.preset_available(Selection::Clipboard, Ok(vec![jpeg.clone()]));
        m.preset_get(
            Selection::Clipboard,
            jpeg,
            Ok(b"\xFF\xD8\xFF\x00\x00\x00".to_vec()),
        );
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        match clipboard_paste(&cb, dir.path(), "pasted-3") {
            ClipboardPaste::Media(a) => assert_eq!(a.filename(), "pasted-3.jpg"),
            other => panic!("expected a jpeg, got {other:?}"),
        }
    }

    /// Text on its own pastes as text — the behaviour `Ctrl+]` has always had,
    /// and the one this must not disturb.
    #[test]
    fn text_alone_still_pastes_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let m = mock(Capabilities::all());
        m.preset_available(Selection::Clipboard, Ok(vec![MimeType::Text]));
        m.preset_get(
            Selection::Clipboard,
            MimeType::Text,
            Ok(b"just some words".to_vec()),
        );
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        assert_eq!(
            clipboard_paste(&cb, dir.path(), "pasted-1"),
            ClipboardPaste::Text("just some words".to_string())
        );
    }

    /// A file copied in a file manager arrives as a `text/uri-list`, and is read
    /// off disk by the same byte-sniffing path an `@mention` takes.
    #[test]
    fn resolves_a_copied_file_uri_to_the_image_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, png(32)).unwrap();

        // Encode the uri-list through the library's own writer, so the bytes are
        // spelled the way this platform spells them.
        let writer = mock(Capabilities::all());
        let handle = writer.handle();
        let scribe = Clipboard::with_backend(Box::new(writer));
        scribe
            .set_uri_list(Selection::Clipboard, &[Uri::File(shot.clone())])
            .unwrap();
        let uri_bytes = handle.set_calls()[0].bytes.clone();

        let m = mock(Capabilities::all());
        m.preset_available(Selection::Clipboard, Ok(vec![MimeType::UriList]));
        m.preset_get(Selection::Clipboard, MimeType::UriList, Ok(uri_bytes));
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        match clipboard_paste(&cb, dir.path(), "pasted-1") {
            // Named for the file it came from, not for the paste serial.
            ClipboardPaste::Media(a) => assert_eq!(a.filename(), "shot.png"),
            other => panic!("expected the copied file, got {other:?}"),
        }
    }

    /// A copied *text* file is not an attachment: it falls through to the text
    /// path rather than being refused, so the paste still does something.
    #[test]
    fn a_copied_non_image_file_falls_through_to_text() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("note.txt");
        std::fs::write(&note, "not an image").unwrap();

        let writer = mock(Capabilities::all());
        let handle = writer.handle();
        let scribe = Clipboard::with_backend(Box::new(writer));
        scribe
            .set_uri_list(Selection::Clipboard, &[Uri::File(note)])
            .unwrap();
        let uri_bytes = handle.set_calls()[0].bytes.clone();

        let m = mock(Capabilities::all());
        m.preset_available(
            Selection::Clipboard,
            Ok(vec![MimeType::UriList, MimeType::Text]),
        );
        m.preset_get(Selection::Clipboard, MimeType::UriList, Ok(uri_bytes));
        m.preset_get(
            Selection::Clipboard,
            MimeType::Text,
            Ok(b"/tmp/note.txt".to_vec()),
        );
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        assert_eq!(
            clipboard_paste(&cb, dir.path(), "pasted-1"),
            ClipboardPaste::Text("/tmp/note.txt".to_string())
        );
    }

    /// An image type hrdr cannot attach (a BMP, say) is refused by name rather
    /// than falling through as if the clipboard had been empty.
    #[test]
    fn refuses_an_image_type_it_cannot_attach() {
        let dir = tempfile::tempdir().unwrap();
        let bmp = MimeType::Custom("image/bmp".to_string());
        let m = mock(Capabilities::all());
        m.preset_available(Selection::Clipboard, Ok(vec![bmp.clone()]));
        m.preset_get(
            Selection::Clipboard,
            bmp,
            Ok(b"BM\x00\x00\x00\x00".to_vec()),
        );
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        match clipboard_paste(&cb, dir.path(), "pasted-1") {
            ClipboardPaste::Refused(why) => assert!(why.contains("PNG, JPEG"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A backend that cannot do images (OSC 52) says so instead of reporting an
    /// empty clipboard, so a user looking at an image they just copied is told
    /// why it did not arrive.
    #[test]
    fn says_so_when_the_backend_cannot_read_images() {
        let dir = tempfile::tempdir().unwrap();
        let m = mock(Capabilities::READ | Capabilities::WRITE);
        m.preset_get(Selection::Clipboard, MimeType::Text, Ok(Vec::new()));
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        match clipboard_paste(&cb, dir.path(), "pasted-1") {
            ClipboardPaste::Refused(why) => assert!(why.contains("can't read images"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// An image-capable clipboard holding nothing is empty, and says only that.
    #[test]
    fn reports_an_empty_clipboard_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let m = mock(Capabilities::all());
        m.preset_get(Selection::Clipboard, MimeType::Text, Ok(Vec::new()));
        let cb = Some(Clipboard::with_backend(Box::new(m)));

        assert_eq!(
            clipboard_paste(&cb, dir.path(), "pasted-1"),
            ClipboardPaste::Empty
        );
    }

    /// No clipboard at all — the platform has none, or it failed to open.
    #[test]
    fn no_backend_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            clipboard_paste(&None, dir.path(), "pasted-1"),
            ClipboardPaste::Unavailable
        );
    }

    /// The summary names the file, its type and its size — what both the
    /// composer's status line and the transcript row are built from.
    #[test]
    fn summary_names_the_file_its_type_and_its_size() {
        let a =
            hrdr_tools::Attachment::new(png(2048), hrdr_tools::MediaType::Png, "shot.png").unwrap();
        assert_eq!(attachment_summary(&a), "shot.png (image/png, 2.0 KB)");

        // 24 raw bytes — a multiple of three, so the size back out of the base64
        // length is exact.
        let exact =
            hrdr_tools::Attachment::new(png(16), hrdr_tools::MediaType::Png, "tiny.png").unwrap();
        assert_eq!(attachment_summary(&exact), "tiny.png (image/png, 24 B)");

        // 16 raw bytes, which base64 pads out to 24 characters: reading the size
        // back off the encoded length rounds up by the padding, and never by more
        // than the two bytes pinned here. Invisible at any real attachment's size,
        // and the attachment does not carry its raw length to do better with.
        let padded =
            hrdr_tools::Attachment::new(png(8), hrdr_tools::MediaType::Png, "odd.png").unwrap();
        assert_eq!(attachment_summary(&padded), "odd.png (image/png, 18 B)");
    }
}

//! Windows input read as VT sequences, so a bracketed paste arrives as one
//! [`Event::Paste`] instead of a run of key presses whose newlines submit.
//!
//! crossterm's Windows backend reads console input records: it never enables
//! `ENABLE_VIRTUAL_TERMINAL_INPUT`, rejects `EnableBracketedPaste`, and never
//! yields `Event::Paste`. With VT input on, the console host forwards what the
//! terminal sent — keys, mouse reports, paste brackets — as characters, and
//! crossterm's own unix parser (vendored in [`parse`]) turns them into the same
//! events it produces elsewhere.
//!
//! Only a modern host delivers those sequences whole: the inbox `conhost.exe`
//! loses the prefixes of fragmented ones even with VT input on. So
//! [`modern_console_host`] gates this reader, and every other host keeps
//! crossterm's.

mod parse;

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use crossterm::event::{Event, KeyboardEnhancementFlags};
use windows_sys::Wdk::System::Threading::NtQueryInformationProcess;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle,
    INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE, SetConsoleMode,
    WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};

/// What the vendored parser reports. hrdr asks the terminal for none of the
/// replies (cursor position, keyboard flags, device attributes), so only
/// [`InternalEvent::Event`] is ever acted on; the rest exist because the
/// parser recognises them.
#[derive(Debug, PartialEq, Eq)]
enum InternalEvent {
    Event(Event),
    CursorPosition(u16, u16),
    KeyboardEnhancementFlags(KeyboardEnhancementFlags),
    PrimaryDeviceAttributes,
}

/// Feeds bytes to [`parse::parse_event`] one at a time, keeping a partial
/// sequence until it completes — crossterm 0.29.0's unix `Parser`
/// (`src/event/source/unix/mio.rs`), under the MIT notice in [`parse`].
#[derive(Default)]
struct Parser {
    buffer: Vec<u8>,
    events: VecDeque<InternalEvent>,
}

impl Parser {
    /// `more`: whether further input is already waiting, which decides
    /// whether a lone `ESC` is the Escape key or the start of a sequence.
    fn advance(&mut self, bytes: &[u8], more: bool) {
        for (idx, byte) in bytes.iter().enumerate() {
            let more = idx + 1 < bytes.len() || more;
            self.buffer.push(*byte);
            match parse::parse_event(&self.buffer, more) {
                Ok(Some(event)) => {
                    self.events.push_back(event);
                    self.buffer.clear();
                }
                // An incomplete sequence: keep it for the next bytes.
                Ok(None) => {}
                // Unparseable: drop it and start over at the next byte.
                Err(_) => self.buffer.clear(),
            }
        }
    }

    fn next_event(&mut self) -> Option<Event> {
        while let Some(event) = self.events.pop_front() {
            if let InternalEvent::Event(event) = event {
                return Some(event);
            }
        }
        None
    }
}

/// Whether this process's console is hosted by `OpenConsole.exe` — the
/// ConPTY host Windows Terminal ships, and the one a side-loaded `conpty.dll`
/// starts — rather than the inbox `conhost.exe`.
pub(crate) fn modern_console_host() -> bool {
    host_image_name().is_some_and(|name| name.eq_ignore_ascii_case("OpenConsole.exe"))
}

/// `ProcessConsoleHostProcess`: the `NtQueryInformationProcess` class that
/// reports the process hosting this console. Absent from the SDK headers but
/// stable since Windows 8 and what Process Explorer reads; the value's two low
/// bits are flags, not part of the pid.
const PROCESS_CONSOLE_HOST_PROCESS: i32 = 49;

/// The file name of the process hosting this console, if it can be found.
/// Under ConPTY the console window belongs to the client, not the host, so
/// the window's owner cannot answer this.
fn host_image_name() -> Option<String> {
    let mut host: usize = 0;
    // SAFETY: the pseudo handle for this process and a `usize` buffer of the
    // size passed, which is what this information class writes.
    let status = unsafe {
        NtQueryInformationProcess(
            GetCurrentProcess(),
            PROCESS_CONSOLE_HOST_PROCESS,
            (&raw mut host).cast(),
            std::mem::size_of::<usize>() as u32,
            std::ptr::null_mut(),
        )
    };
    let pid = u32::try_from(host & !0b11).ok()?;
    if status < 0 || pid == 0 {
        return None;
    }
    image_name(pid)
}

/// The file name of process `pid`'s executable.
fn image_name(pid: u32) -> Option<String> {
    // SAFETY: a query-only handle to `pid`; closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: `path` holds `len` u16s; `len` is updated to the written length.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len)
    };
    // SAFETY: `process` came from `OpenProcess` above and is closed once.
    unsafe { CloseHandle(process) };
    if ok == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&path[..len as usize]);
    std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Console input as crossterm [`Event`]s parsed from VT sequences, read on a
/// thread of its own. Dropping it restores the console's input mode.
pub(crate) struct VtInput {
    events: tokio::sync::mpsc::UnboundedReceiver<io::Result<Event>>,
    _mode: InputMode,
}

impl VtInput {
    /// Switch the console to VT input and start reading it.
    pub(crate) fn start() -> io::Result<Self> {
        let mode = InputMode::enable_vt()?;
        let handle = mode.handle;
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("hrdr-vt-input".into())
            .spawn(move || read_console(handle as HANDLE, &tx))?;
        Ok(Self {
            events,
            _mode: mode,
        })
    }
}

impl futures_util::Stream for VtInput {
    type Item = io::Result<Event>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events.poll_recv(cx)
    }
}

/// The input mode set for [`VtInput`], put back as it was on drop. The handle
/// is kept as its address: a raw `HANDLE` is a pointer, which would keep the
/// stream from moving between threads, and the process-wide standard input
/// handle is valid on any of them.
struct InputMode {
    handle: isize,
    original: u32,
}

impl InputMode {
    fn enable_vt() -> io::Result<Self> {
        // SAFETY: no arguments; the standard input handle is not owned here.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut original = 0u32;
        // SAFETY: `handle` is the console input handle; `original` is writable.
        if unsafe { GetConsoleMode(handle, &mut original) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above; only the VT-input bit is added.
        if unsafe { SetConsoleMode(handle, original | ENABLE_VIRTUAL_TERMINAL_INPUT) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: handle as isize,
            original,
        })
    }
}

impl Drop for InputMode {
    fn drop(&mut self) {
        // SAFETY: restores the mode read from this same handle.
        unsafe { SetConsoleMode(self.handle as HANDLE, self.original) };
    }
}

/// Key characters arrive as UTF-16 units, and a read can end between the two
/// halves of a surrogate pair; the high half waits for the next read.
#[derive(Default)]
struct Utf16Decoder {
    pending_high: Option<u16>,
}

impl Utf16Decoder {
    fn decode(&mut self, units: Vec<u16>) -> String {
        let mut units: Vec<u16> = self.pending_high.take().into_iter().chain(units).collect();
        if units
            .last()
            .is_some_and(|&unit| (0xD800..0xDC00).contains(&unit))
        {
            self.pending_high = units.pop();
        }
        char::decode_utf16(units)
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    }
}

/// How many input records one read takes.
const RECORDS_PER_READ: usize = 128;

/// Read console input records until the receiver is gone, forwarding the
/// characters of key presses through the parser and window resizes as
/// [`Event::Resize`]. Other records (mouse, focus, menu) are not read here:
/// with VT input on, the terminal reports mouse and focus as sequences.
fn read_console(handle: HANDLE, tx: &tokio::sync::mpsc::UnboundedSender<io::Result<Event>>) {
    let mut parser = Parser::default();
    let mut decoder = Utf16Decoder::default();
    // SAFETY: `INPUT_RECORD` is plain data; zeroed is a valid empty record.
    let mut records: [INPUT_RECORD; RECORDS_PER_READ] = unsafe { std::mem::zeroed() };
    loop {
        let mut read = 0u32;
        // SAFETY: `records` holds `RECORDS_PER_READ` records; `read` is writable.
        let ok = unsafe {
            ReadConsoleInputW(
                handle,
                records.as_mut_ptr(),
                RECORDS_PER_READ as u32,
                &mut read,
            )
        };
        if ok == 0 {
            let _ = tx.send(Err(io::Error::last_os_error()));
            return;
        }
        let mut units: Vec<u16> = Vec::new();
        let mut resized = false;
        for record in &records[..read as usize] {
            match u32::from(record.EventType) {
                KEY_EVENT => {
                    // SAFETY: `EventType` says this union holds a key event.
                    let key = unsafe { record.Event.KeyEvent };
                    // SAFETY: the character field of a key event.
                    let unit = unsafe { key.uChar.UnicodeChar };
                    if key.bKeyDown != 0 && unit != 0 {
                        units.push(unit);
                    }
                }
                WINDOW_BUFFER_SIZE_EVENT => resized = true,
                _ => {}
            }
        }
        let text = decoder.decode(units);
        let mut waiting = 0u32;
        // SAFETY: `waiting` is writable; on failure it stays 0, read as "none".
        unsafe { GetNumberOfConsoleInputEvents(handle, &mut waiting) };
        parser.advance(text.as_bytes(), waiting > 0);
        while let Some(event) = parser.next_event() {
            if tx.send(Ok(event)).is_err() {
                return;
            }
        }
        if resized
            && let Ok((cols, rows)) = crossterm::terminal::size()
            && tx.send(Ok(Event::Resize(cols, rows))).is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Parser, Utf16Decoder};
    use crossterm::event::{Event, KeyCode};

    fn events(parser: &mut Parser) -> Vec<Event> {
        std::iter::from_fn(|| parser.next_event()).collect()
    }

    /// A paste whose frame arrives over several reads still comes out as one
    /// paste, newlines and all, and no key event leaks from inside it.
    #[test]
    fn a_paste_split_across_reads_is_one_paste() {
        let mut parser = Parser::default();
        for chunk in ["\x1b[20", "0~line one\r\nline", " two\x1b[2", "01~"] {
            parser.advance(chunk.as_bytes(), false);
        }
        assert_eq!(
            events(&mut parser),
            vec![Event::Paste("line one\r\nline two".into())]
        );
    }

    /// A lone ESC is the Escape key unless more input is already waiting, in
    /// which case it may open a sequence.
    #[test]
    fn a_lone_escape_waits_only_while_more_input_is_pending() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b", true);
        assert!(events(&mut parser).is_empty());
        parser.advance(b"[A", false);
        assert_eq!(events(&mut parser), vec![Event::Key(KeyCode::Up.into())]);

        parser.advance(b"\x1b", false);
        assert_eq!(events(&mut parser), vec![Event::Key(KeyCode::Esc.into())]);
    }

    /// A surrogate pair split between two reads decodes to its character.
    #[test]
    fn a_surrogate_pair_split_across_reads_decodes_whole() {
        let crab: Vec<u16> = "🦀".encode_utf16().collect();
        let mut decoder = Utf16Decoder::default();
        assert_eq!(decoder.decode(vec![u16::from(b'a'), crab[0]]), "a");
        assert_eq!(decoder.decode(vec![crab[1], u16::from(b'b')]), "🦀b");
    }
}

use std::cell::RefCell;
use std::rc::Rc;

use ratatui::backend::TestBackend;

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Hide,
    Flush,
    Cells(Vec<(u16, u16, String)>),
    Position(Position),
    Show,
    Clear,
}

struct RecordingBackend {
    screen: TestBackend,
    events: Rc<RefCell<Vec<Event>>>,
    fail_at: Option<usize>,
}

impl RecordingBackend {
    fn record(&mut self, event: Event) -> io::Result<()> {
        let mut events = self.events.borrow_mut();
        events.push(event);
        if self.fail_at == Some(events.len()) {
            self.fail_at = None;
            return Err(io::Error::other(format!("failure at {}", events.len())));
        }
        Ok(())
    }
}

impl Backend for RecordingBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<_> = content.collect();
        self.record(Event::Cells(
            cells
                .iter()
                .map(|(x, y, c)| (*x, *y, c.symbol().to_owned()))
                .collect(),
        ))?;
        self.screen
            .draw(cells.into_iter())
            .map_err(|never| match never {})
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.record(Event::Hide)?;
        self.screen.hide_cursor().map_err(|never| match never {})
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.record(Event::Show)?;
        self.screen.show_cursor().map_err(|never| match never {})
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.screen
            .get_cursor_position()
            .map_err(|never| match never {})
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.record(Event::Position(position))?;
        self.screen
            .set_cursor_position(position)
            .map_err(|never| match never {})
    }

    fn clear(&mut self) -> io::Result<()> {
        self.record(Event::Clear)?;
        self.screen.clear().map_err(|never| match never {})
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.record(Event::Clear)?;
        self.screen
            .clear_region(clear_type)
            .map_err(|never| match never {})
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.screen.append_lines(n).map_err(|never| match never {})
    }

    fn size(&self) -> io::Result<Size> {
        self.screen.size().map_err(|never| match never {})
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.screen.window_size().map_err(|never| match never {})
    }

    fn flush(&mut self) -> io::Result<()> {
        self.record(Event::Flush)?;
        self.screen.flush().map_err(|never| match never {})
    }
}

fn terminal() -> Terminal<CursorBackend<RecordingBackend>> {
    Terminal::new(CursorBackend::new(RecordingBackend {
        screen: TestBackend::new(4, 2),
        events: Rc::default(),
        fail_at: None,
    }))
    .unwrap()
}

fn paint(frame: &mut Frame, text: &str, cursor: Option<(u16, u16)>) {
    frame.render_widget(text, frame.area());
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn expected_frame(text: &str, cursor: (u16, u16)) -> Vec<Event> {
    vec![
        Event::Hide,
        Event::Flush,
        Event::Cells(
            text.chars()
                .enumerate()
                .map(|(x, c)| (x as u16, 0, c.to_string()))
                .collect(),
        ),
        Event::Position(cursor.into()),
        Event::Flush,
        Event::Show,
        Event::Flush,
    ]
}

#[test]
fn changed_frames_hide_before_cells_and_reveal_after_position_flush() {
    let mut terminal = terminal();
    for text in ["ab", "cd"] {
        terminal.backend().inner.events.borrow_mut().clear();
        draw_frame(&mut terminal, |f| paint(f, text, Some((2, 1)))).unwrap();
        let backend = terminal.backend();
        assert_eq!(*backend.inner.events.borrow(), expected_frame(text, (2, 1)));
        assert_eq!(backend.inner.screen.buffer()[(0, 0)].symbol(), &text[..1]);
        assert_eq!(backend.inner.screen.buffer()[(1, 0)].symbol(), &text[1..]);
        assert!(backend.inner.screen.cursor_visible());
        assert!(!backend.restoration_owed);
    }
}

#[test]
fn unchanged_and_cursor_only_frames_keep_ordering() {
    let mut terminal = terminal();
    draw_frame(&mut terminal, |f| paint(f, "ab", Some((2, 1)))).unwrap();
    for cursor in [(2, 1), (3, 0)] {
        terminal.backend().inner.events.borrow_mut().clear();
        draw_frame(&mut terminal, |f| paint(f, "ab", Some(cursor))).unwrap();
        assert_eq!(
            *terminal.backend().inner.events.borrow(),
            expected_frame("", cursor)
        );
        assert_eq!(terminal.get_cursor_position().unwrap(), cursor.into());
    }
}

#[test]
fn no_cursor_stays_hidden_and_drop_restores_it() {
    let mut terminal = terminal();
    let events = terminal.backend().inner.events.clone();
    draw_frame(&mut terminal, |f| paint(f, "a", None)).unwrap();
    assert_eq!(
        *events.borrow(),
        vec![
            Event::Hide,
            Event::Flush,
            Event::Cells(vec![(0, 0, "a".into())]),
            Event::Hide,
            Event::Flush
        ]
    );
    assert!(!terminal.backend().inner.screen.cursor_visible());
    assert!(terminal.backend().restoration_owed);
    drop(terminal);
    assert!(events.borrow().ends_with(&[Event::Show, Event::Flush]));
}

#[test]
fn resize_hides_before_autoresize_clear() {
    let mut terminal = terminal();
    draw_frame(&mut terminal, |f| paint(f, "a", Some((1, 0)))).unwrap();
    terminal.backend_mut().inner.screen.resize(5, 3);
    terminal.backend().inner.events.borrow_mut().clear();
    draw_frame(&mut terminal, |f| paint(f, "b", Some((2, 0)))).unwrap();
    let mut expected = expected_frame("b", (2, 0));
    expected.insert(2, Event::Clear);
    assert_eq!(*terminal.backend().inner.events.borrow(), expected);
}

#[test]
fn errors_preserve_original_stop_reveal_and_reset_deferral() {
    let expected = expected_frame("a", (1, 0));
    for fail_at in 1..=expected.len() {
        let mut terminal = terminal();
        let events = terminal.backend().inner.events.clone();
        terminal.backend_mut().inner.fail_at = Some(fail_at);
        let error = draw_frame(&mut terminal, |f| paint(f, "a", Some((1, 0)))).unwrap_err();
        assert_eq!(error.to_string(), format!("failure at {fail_at}"));
        assert_eq!(*events.borrow(), expected[..fail_at]);
        let backend = terminal.backend();
        assert!(!backend.deferring);
        assert!(!backend.pending_show);
        assert!(backend.restoration_owed);
        drop(terminal);
        assert_eq!(&events.borrow()[fail_at..], &[Event::Show, Event::Flush]);
    }
}

#[test]
fn callback_unwind_resets_scope_without_io_and_drop_restores() {
    let mut terminal = terminal();
    let events = terminal.backend().inner.events.clone();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = draw_frame(&mut terminal, |_| panic!("render panic"));
    }));
    assert!(panic.is_err());
    assert_eq!(*events.borrow(), vec![Event::Hide, Event::Flush]);
    assert!(!terminal.backend().deferring);
    assert!(!terminal.backend().pending_show);
    assert!(terminal.backend().restoration_owed);
    drop(terminal);
    assert_eq!(
        *events.borrow(),
        vec![Event::Hide, Event::Flush, Event::Show, Event::Flush]
    );
}

#[test]
fn hide_cancels_deferred_show_and_outside_frame_show_is_immediate() {
    let mut terminal = terminal();
    let backend = terminal.backend_mut();
    backend.deferring = true;
    backend.show_cursor().unwrap();
    assert!(backend.pending_show);
    backend.hide_cursor().unwrap();
    assert!(!backend.pending_show);
    assert_eq!(*backend.inner.events.borrow(), vec![Event::Hide]);
    backend.deferring = false;
    terminal.show_cursor().unwrap();
    assert_eq!(
        *terminal.backend().inner.events.borrow(),
        vec![Event::Hide, Event::Show, Event::Flush]
    );
    assert!(!terminal.backend().restoration_owed);
}

#[test]
fn adapter_drop_bypasses_deferral_and_tolerates_restoration_error() {
    let terminal = terminal();
    let mut backend = CursorBackend::new(RecordingBackend {
        screen: TestBackend::new(4, 2),
        events: terminal.backend().inner.events.clone(),
        fail_at: Some(2),
    });
    let events = backend.inner.events.clone();
    backend.hide_cursor().unwrap();
    backend.deferring = true;
    backend.show_cursor().unwrap();
    drop(backend);
    assert_eq!(
        *events.borrow(),
        vec![Event::Hide, Event::Show, Event::Flush]
    );
}

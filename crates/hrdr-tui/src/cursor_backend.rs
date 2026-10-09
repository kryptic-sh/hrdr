use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::{Frame, Terminal};

pub(crate) struct CursorBackend<B: Backend> {
    inner: B,
    deferring: bool,
    pending_show: bool,
    restoration_owed: bool,
}

impl<B: Backend> CursorBackend<B> {
    pub(crate) fn new(inner: B) -> Self {
        Self {
            inner,
            deferring: false,
            pending_show: false,
            restoration_owed: false,
        }
    }
}

// Own the terminal borrow so cleanup runs on both errors and callback unwinds,
// without issuing terminal I/O while unwinding the frame.
struct FrameScope<'a, B: Backend>(&'a mut Terminal<CursorBackend<B>>);

impl<B: Backend> Drop for FrameScope<'_, B> {
    fn drop(&mut self) {
        let backend = self.0.backend_mut();
        backend.deferring = false;
        backend.pending_show = false;
    }
}

pub(crate) fn draw_frame<B: Backend>(
    terminal: &mut Terminal<CursorBackend<B>>,
    render: impl FnOnce(&mut Frame),
) -> Result<(), B::Error> {
    let scope = FrameScope(terminal);
    scope.0.backend_mut().deferring = true;
    // Hide before draw's autoresize, which can clear the screen.
    scope.0.hide_cursor()?;
    scope.0.backend_mut().flush()?;
    scope.0.draw(render)?;
    let backend = scope.0.backend_mut();
    backend.deferring = false;
    if backend.pending_show {
        backend.show_cursor()?;
    }
    Ok(())
}

impl<B: Backend> Backend for CursorBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.pending_show = false;
        // Even a failed hide may have partially reached the terminal.
        self.restoration_owed = true;
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        if self.deferring {
            self.pending_show = true;
            return Ok(());
        }
        self.inner.show_cursor()?;
        self.inner.flush()?;
        self.restoration_owed = false;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

impl<B: Backend + Write> Write for CursorBackend<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<B: Backend> Drop for CursorBackend<B> {
    fn drop(&mut self) {
        if self.restoration_owed {
            // Ratatui may already consider a deferred show successful. Restore
            // independently, without masking the original error or panic.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = self.inner.show_cursor();
                let _ = self.inner.flush();
            }));
        }
    }
}

#[cfg(test)]
mod tests;

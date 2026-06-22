//! [`alacritty_terminal::Term`] wrapper + event proxy + reader thread.
//!
//! `Term<T>` is parameterised on a `T: EventListener` that it calls
//! into when it wants to ask the host for something (PTY write, title
//! change, exit, bell, …). The listener has to be `Send + Sync + 'static`
//! and is cloned into every place Term might use it from — most
//! notably the reader thread's parser. We give it the PTY (for
//! `Event::PtyWrite`) and the winit [`EventLoopProxy`] (for redraw +
//! exit signalling on the main thread).

use std::io::{ErrorKind, Read};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;
use winit::event_loop::EventLoopProxy;

use crate::pty::{Pty, PtyWriter};

// `FairMutex` is alacritty's re-export of parking_lot's fair-locking
// mutex. We use it (vs std::sync::Mutex) so the reader thread can't
// monopolise the lock when render is trying to grab it.

/// Custom event delivered to the winit main thread.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// The Term has changed and the window should be redrawn.
    Redraw,
    /// The child shell exited (PTY EOF on the reader). The main thread
    /// should close the window on receipt.
    ChildExited,
    /// New window title (OSC 0/2 from the running program).
    Title(String),
}

/// Bridge between Term's `EventListener` callbacks and the rest of the
/// process. Cheaply cloneable — every callback site holds an `Arc`.
#[derive(Clone)]
pub struct EventProxy {
    inner: Arc<EventProxyInner>,
}

struct EventProxyInner {
    proxy: EventLoopProxy<UserEvent>,
    pty_writer: PtyWriter,
}

impl EventProxy {
    pub fn new(proxy: EventLoopProxy<UserEvent>, pty_writer: PtyWriter) -> Self {
        Self {
            inner: Arc::new(EventProxyInner { proxy, pty_writer }),
        }
    }

    fn redraw(&self) {
        let _ = self.inner.proxy.send_event(UserEvent::Redraw);
    }

    fn title(&self, title: String) {
        let _ = self.inner.proxy.send_event(UserEvent::Title(title));
    }

    fn child_exited(&self) {
        let _ = self.inner.proxy.send_event(UserEvent::ChildExited);
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        match event {
            AlacEvent::PtyWrite(text) => {
                // The shell is responding to a sequence (e.g. cursor
                // position report). Forward to PTY; ignore errors —
                // the reader thread will notice the EOF if the shell
                // is gone.
                let _ = self.inner.pty_writer.write(text.as_bytes());
            }
            AlacEvent::Title(t) => self.title(t),
            AlacEvent::ResetTitle => self.title(crate::WINDOW_TITLE.into()),
            AlacEvent::Wakeup | AlacEvent::MouseCursorDirty | AlacEvent::CursorBlinkingChange => {
                self.redraw();
            }
            AlacEvent::Bell => {
                // Out of scope for MVP — visual bell would be a flash
                // in the renderer.
            }
            AlacEvent::Exit | AlacEvent::ChildExit(_) => self.child_exited(),
            // Clipboard / color requests / text-area size: ignored in
            // MVP. The default `String` reply slots are filled with
            // the result of the callback, which we just don't invoke.
            _ => {}
        }
    }
}

/// Dimensions implementing alacritty_terminal's `Dimensions` trait so
/// we can pass cols/rows into `Term::new` and `Term::resize` without
/// also touching scrollback history.
#[derive(Copy, Clone, Debug)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl alacritty_terminal::grid::Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Construct a fresh Term wrapped in a fair mutex so the reader thread
/// (writer) and the main thread (reader) don't starve each other.
pub fn new_term(size: GridSize, event_proxy: EventProxy) -> Arc<FairMutex<Term<EventProxy>>> {
    let config = TermConfig::default();
    let term = Term::new(config, &size, event_proxy);
    Arc::new(FairMutex::new(term))
}

/// Spawn the PTY-reader thread. It blocks on `pty.reader()`, feeds
/// bytes into an `ansi::Processor` (which mutates Term through its
/// `Handler` impl), and signals the main thread to redraw. On PTY
/// EOF — which is how shell exit propagates upward — it sends
/// `UserEvent::ChildExited`.
pub fn start_pty_reader(
    pty: &Pty,
    term: Arc<FairMutex<Term<EventProxy>>>,
    event_proxy: EventProxy,
) -> std::io::Result<JoinHandle<()>> {
    let mut reader = pty
        .reader()
        .map_err(|e| std::io::Error::other(format!("clone pty reader: {e}")))?;

    let handle = thread::Builder::new()
        .name("tacet-terminal-pty-reader".into())
        .spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF — shell exited. Signal main thread.
                        event_proxy.child_exited();
                        return;
                    }
                    Ok(n) => {
                        let mut t = term.lock();
                        parser.advance(&mut *t, &buf[..n]);
                        drop(t);
                        event_proxy.redraw();
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        // Read error (PTY torn down). Treat as exit.
                        event_proxy.child_exited();
                        return;
                    }
                }
            }
        })?;

    Ok(handle)
}

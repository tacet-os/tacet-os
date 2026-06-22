//! PTY + shell process. Wraps portable-pty's master/slave pair: the
//! slave is handed to a $SHELL child; the master is what we read/write.
//!
//! alacritty_terminal ships its own `tty` module (also Unix openpty),
//! but portable-pty was already the codebase's PTY choice, has a
//! cleaner separation of reader/writer/resize, and lets us keep this
//! file shell-agnostic. The cost is one extra crate; the benefit is
//! a familiar API.
//!
//! [`MasterPty`] is `Send` but not `Sync`, which means `Pty` itself
//! can't go behind an `Arc` shared across threads. We split out
//! [`PtyWriter`] (a `Send + Sync` clonable handle wrapping the writer
//! Mutex) so the EventProxy — which is shared into the reader thread
//! — can still call `pty_write.write(...)` without the master.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

/// Master-thread handle for the PTY: holds the master fd, the child
/// reference, and exposes resize / take-reader / take-writer-handle.
/// Lives on the main thread only.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: PtyWriter,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// Send + Sync writer handle. Cloneable cheaply (Arc) so we can hand
/// one to every thread that needs to push bytes back to the shell.
#[derive(Clone)]
pub struct PtyWriter {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Pty {
    pub fn spawn_shell(cols: u16, rows: u16) -> Result<Self> {
        let system = NativePtySystem::default();
        let pair = system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(&shell);
        // xterm-256color is the closest stable advertisement for what
        // alacritty_terminal actually supports (256-color + truecolor
        // via COLORTERM). Most apps key off TERM for color decisions.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair.slave.spawn_command(cmd).context("spawn child")?;
        // Drop the slave handle on our side — the child holds it. Without
        // this, EOF won't propagate to our reader when the shell exits.
        drop(pair.slave);

        let writer_box = pair.master.take_writer().context("pty writer")?;
        let writer = PtyWriter {
            inner: Arc::new(Mutex::new(writer_box)),
        };

        Ok(Self {
            master: pair.master,
            writer,
            _child: child,
        })
    }

    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().context("clone pty reader")
    }

    pub fn writer(&self) -> PtyWriter {
        self.writer.clone()
    }

    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write(bytes)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")
    }
}

impl PtyWriter {
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("pty writer mutex poisoned"))?;
        w.write_all(bytes)?;
        w.flush()
    }
}

//! tacet-terminal — native Wayland terminal for tacet-os.
//!
//! Architecture:
//!
//!   shell ($SHELL under a PTY)
//!        ↑           ↓
//!     write        read
//!        │           │
//!      ┌─┴───────────┴─┐         ┌────────────────────────┐
//!      │ rust process  │         │ winit Wayland window   │
//!      │  - portable-  │ Mutex   │  - softbuffer surface  │
//!      │    pty master │ <─────> │  - cosmic-text glyphs  │
//!      │  - alacritty- │ Term    │  - keyboard → PTY      │
//!      │    Processor  │         └────────────────────────┘
//!      │    (VT parse) │
//!      └───────────────┘
//!
//! The `Term` struct lives in process memory. Any in-process agent
//! (compositor read-only mirror, chat surface, etc.) can read
//! `term.grid()` directly — this is the structural reason for the
//! native approach over a Chromium/xterm.js bridge.

mod keys;
mod pty;
mod render;
mod term;

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::pty::Pty;
use crate::render::Renderer;
use crate::term::{EventProxy, GridSize, UserEvent};

/// Default window title — also the value Term will OSC-reset to.
pub const WINDOW_TITLE: &str = "tacet-terminal";

fn main() -> Result<()> {
    init_tracing();

    // EventLoop must be created on the main thread; UserEvent is the
    // custom event delivered via EventLoopProxy from worker threads.
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("build winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::Uninit {
        proxy: event_loop.create_proxy(),
    };
    event_loop
        .run_app(&mut app)
        .context("winit event loop")?;
    Ok(())
}

fn init_tracing() {
    let env = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(env)
        .init();
}

/// Winit's `ApplicationHandler` requires a `&mut self` callback model.
/// We store everything once the window is realised in `resumed`. The
/// uninit variant keeps the proxy so worker threads can be wired into
/// the loop before they're spawned.
enum App {
    Uninit {
        proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    },
    Ready(AppState),
}

struct AppState {
    renderer: Renderer,
    pty: Pty,
    term: Arc<alacritty_terminal::sync::FairMutex<alacritty_terminal::Term<EventProxy>>>,
    /// Kept alive so the reader thread's clone stays valid for the
    /// lifetime of the window — dropping the last proxy would orphan
    /// the worker's `EventLoopProxy<UserEvent>` clone.
    #[allow(dead_code)]
    event_proxy: EventProxy,
    modifiers: winit::event::Modifiers,
    cols: u32,
    rows: u32,
    _reader_thread: std::thread::JoinHandle<()>,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // `resumed` can fire multiple times on some platforms; only
        // initialise once.
        if matches!(self, App::Ready(_)) {
            return;
        }

        let App::Uninit { proxy } = self else {
            unreachable!();
        };
        let proxy = proxy.clone();

        match build_state(event_loop, proxy) {
            Ok(state) => *self = App::Ready(state),
            Err(e) => {
                error!(err = %e, "fatal: failed to initialise terminal");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let App::Ready(state) = self else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => {
                info!("close requested; exiting");
                event_loop.exit();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                state.modifiers = modifiers;
            }
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => {
                if let Some(bytes) = keys::encode(&key_event, &state.modifiers) {
                    if let Err(e) = state.pty.write(&bytes) {
                        warn!(err = %e, "pty write failed");
                    }
                }
            }
            WindowEvent::Resized(new_size) => {
                let (cols, rows) = state.renderer.grid_size(new_size.width, new_size.height);
                if cols != state.cols || rows != state.rows {
                    state.cols = cols;
                    state.rows = rows;
                    let grid = GridSize {
                        cols: cols as usize,
                        rows: rows as usize,
                    };
                    state.term.lock().resize(grid);
                    if let Err(e) = state.pty.resize(rows as u16, cols as u16) {
                        warn!(err = %e, "pty resize failed");
                    }
                    debug!(cols, rows, "resized");
                }
                state.renderer.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = state.renderer.paint(&state.term) {
                    warn!(err = %e, "paint error");
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let App::Ready(state) = self else {
            return;
        };
        match event {
            UserEvent::Redraw => state.renderer.window.request_redraw(),
            UserEvent::ChildExited => {
                info!("child shell exited; closing");
                event_loop.exit();
            }
            UserEvent::Title(t) => state.renderer.window.set_title(&t),
        }
    }
}

fn build_state(
    event_loop: &ActiveEventLoop,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
) -> Result<AppState> {
    // 1. Window first — we need its size to compute initial grid dims.
    let attrs = WindowAttributes::default()
        .with_title(WINDOW_TITLE)
        .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
    let window: Arc<Window> = Arc::new(
        event_loop
            .create_window(attrs)
            .context("create window")?,
    );
    info!("window created");

    // 2. Renderer (loads fonts, measures cell).
    let renderer = Renderer::new(window.clone()).context("init renderer")?;
    let inner = window.inner_size();
    let (cols_u32, rows_u32) = renderer.grid_size(inner.width.max(1), inner.height.max(1));
    let cols = cols_u32 as usize;
    let rows = rows_u32 as usize;
    info!(cols, rows, cell_w = renderer.cell_width, cell_h = renderer.cell_height, "grid sized");

    // 3. PTY + shell.
    let pty = Pty::spawn_shell(cols_u32 as u16, rows_u32 as u16).context("spawn pty")?;

    // 4. Event proxy (bridges Term events → winit / PTY).
    let event_proxy = EventProxy::new(proxy, pty.writer());

    // 5. Term, then reader thread.
    let grid_size = GridSize { cols, rows };
    let term = term::new_term(grid_size, event_proxy.clone());
    let reader_thread = term::start_pty_reader(&pty, term.clone(), event_proxy.clone())
        .context("spawn pty reader thread")?;

    Ok(AppState {
        renderer,
        pty,
        term,
        event_proxy,
        modifiers: Default::default(),
        cols: cols_u32,
        rows: rows_u32,
        _reader_thread: reader_thread,
    })
}

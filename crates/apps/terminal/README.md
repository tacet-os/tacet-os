# tacet-terminal

Native Wayland terminal for tacet-os. A single Rust binary that
spawns `$SHELL` under a PTY and renders the grid in a winit window.

## Why native (vs Chromium/xterm.js)

tacet-os is agent-default. Every other component in the stack —
compositor, chat surface, in-process agent — needs to read the
terminal's contents as data, not as pixels or a DOM. Native
`alacritty_terminal::Term` is a Rust struct in process memory:
`term.grid()` is one method call away. A web-bridged terminal would
have forced any consumer to either scrape pixels or open a WebSocket
to talk to xterm.js inside the Chromium process, neither of which is
acceptable for this project's read-as-data goal.

## Stack

- [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal)
  — VT parser + grid + cursor + scrollback.
- [`winit`](https://crates.io/crates/winit) — Wayland window + event
  loop. CSD via `wayland-csd-adwaita`.
- [`softbuffer`](https://crates.io/crates/softbuffer) — CPU pixel
  surface; no GPU/wgpu dependency for the MVP.
- [`cosmic-text`](https://crates.io/crates/cosmic-text) — font
  discovery, shaping, and (via `swash`) glyph rasterization, cached
  per `CacheKey`.
- [`portable-pty`](https://crates.io/crates/portable-pty) — `$SHELL`
  spawn under a PTY (master/slave pair).

## Threads

- **main**: winit event loop. Owns the window, renderer, and Term
  reference. Translates keyboard events to PTY bytes; on
  `RedrawRequested` it locks Term and paints.
- **pty-reader**: blocks on `pty.reader()`, feeds bytes into an
  `ansi::Processor` that mutates Term through its `Handler` impl,
  and sends `UserEvent::Redraw` to wake the main thread. On PTY EOF
  it sends `UserEvent::ChildExited` so the window closes when the
  shell exits.

The Term itself sits behind `alacritty_terminal::sync::FairMutex` so
the reader and renderer can't starve each other.

## MVP scope

In: ASCII + Latin-1 rendering, 16/256/truecolor fg+bg, block cursor,
bold weight, resize → grid + PTY resize, keyboard input incl. ctrl,
alt, arrows, F1–F12.

Out (for now): mouse, selection, scrollback UI, IME, italic,
underline, ligatures, bell, clipboard.

## Wired into the session

The compositor maps Super+Enter to `$TACET_TERMINAL`, which the
session module sets to `tacet-terminal`. The Nix package wraps the
binary with a default monospace font on `XDG_DATA_DIRS` so font
discovery works even on minimal systems.

## Verify

```
nix develop --command cargo check -p tacet-terminal
nix develop --command cargo test  -p tacet-terminal
nix develop --command cargo build -p tacet-terminal
```

Runs only inside a Wayland session (no nested-X fallback in MVP).

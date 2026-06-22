# tacet-compositor

Wayland compositor for [tacet-os](../../). Smithay-based; derived from the
[anvil](https://github.com/Smithay/smithay/tree/master/anvil) reference compositor.

## Backends

| Feature | When | Use |
| --- | --- | --- |
| `winit` | Nested dev — runs as a window inside an existing Wayland/X11 session | `cargo run -p tacet-compositor --no-default-features --features egl,winit -- --winit` |
| `udev` | Real session — tty + DRM + libinput | Installed via `nix build .#tacet-compositor`, launched by greeter |
| `x11` | Nested dev on X11 hosts | `cargo run -p tacet-compositor --no-default-features --features egl,x11 -- --x11` |
| `xwayland` | X11 app support on top of any backend | Default-on |

Default features: `egl + winit + udev + xwayland`. The nix package builds with
`egl + udev + xwayland` only (no winit) since the installed binary always
targets a real session.

## Runtime env vars

- `TACET_AUTOSTART=cmd1:cmd2` — colon-separated commands to spawn after the
  Wayland socket is up. Empty by default.
- `TACET_TERMINAL` — explicit terminal binary for the Super+T shortcut.
- `WAYLAND_DEBUG=1` — Smithay-side protocol tracing.
- `RUST_LOG=debug` — tracing-subscriber filter.

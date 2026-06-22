# tacet-terminal

Terminal for tacet-os. Alpha.1 ships as a placeholder built on
[`tacet-view`](../../libs/view) — it opens a contextless Chromium
window via `--app=` mode pointed at an inline placeholder page.

## What this is today

A few-hundred-LOC `tacet-view` consumer. Confirms the rest of the stack
(Wayland surface, Chromium spawn, contextless profile, CDP) is wired
correctly when you press Super+Enter or run `tacet-terminal`.

## What this becomes

The plan is for `packages/apps/terminal/` to ship an HTML+JS bundle
hosting [libghostty-wasm](https://ghostty.org) for the VT state
machine and an xterm-style cell renderer. The rust binary spawns
a PTY, opens a local WebSocket, and points `tacet-view` at the
bundled `index.html?ws=…`. PTY bytes flow PTY → rust → WS →
libghostty-wasm → DOM canvas.

The interface for the rust binary stays the same — `tacet-terminal`
on `$PATH` — so the compositor's Super+Enter binding and the XDG
default already in `nix/modules/tacet-session.nix` keep working
without changes when the real UI lands.

## Why Chromium-via-`tacet-view` rather than embedded webview

See [`crates/libs/view/src/lib.rs`](../../libs/view/src/lib.rs) for
the long-form rationale. Short version: WebKitGTK can't speak CDP,
CEF/Servo would dwarf the rest of tacet-os, and bundling Chromium
duplicates ~200 MB per app. Wrapping the distro's `chromium` binary
keeps each tacet app to a few hundred LOC and inherits security
patches automatically.

## Override the UI bundle path

The binary checks `$TACET_TERMINAL_UI` first; if set, it loads
`file://$TACET_TERMINAL_UI` instead of the inline placeholder.
The Nix wrapper uses this to point at the bundled HTML once it
exists.

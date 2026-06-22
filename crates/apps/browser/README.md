# tacet-browser

A minimal contextless Chromium wrapper with CDP exposed. Two intended
use-cases, one launch path:

- **Render mode** — point it at a local HTML file (your generative UI,
  an app's bundled view, a docs page) and get a frameless window:

  ```sh
  tacet-browser ./ui/index.html
  ```

- **Browse mode** — point it at an `https://` URL for ad-hoc browsing
  (or for an agent to drive over CDP):

  ```sh
  tacet-browser https://example.com
  ```

Every launch is **contextless**: a fresh ephemeral `--user-data-dir` is
created and deleted on exit, so no cookies, cache, localStorage, or
service workers persist across launches.

## CDP

The resolved CDP WebSocket URL is printed to stdout once Chromium
publishes its DevTools port:

```sh
$ tacet-browser https://example.com
ws://127.0.0.1:43219/devtools/browser/abc123...
```

Pin a known port with `--cdp-port N`. Pass extra Chromium flags after
`--`.

## Architecture

This binary is a thin CLI shell over [`tacet-view`](../../libs/view),
the workspace's primitive for "one ephemeral Chromium-backed surface
with CDP." Other tacet-os apps (a future terminal, agent canvas, etc.)
compose on the same `View::spawn` API rather than each embedding their
own webview.

The web engine is intentionally **not embedded**: WebKitGTK can't speak
CDP, and bundling CEF or building Servo would dwarf the rest of
tacet-os. Wrapping the system `chromium` binary keeps this whole stack
to a few hundred lines and inherits the distro's security patches.

Set `TACET_BROWSER_BIN` to override the Chromium binary (defaults to
`chromium`). The Nix package wraps the binary with `pkgs.chromium` on
`PATH`.

//! tacet-terminal — alpha.1 placeholder.
//!
//! Opens a contextless Chromium window via `tacet_view::View::spawn`
//! pointed at an inline placeholder page. The real terminal will land
//! in `packages/apps/terminal` as an HTML+JS bundle hosting
//! libghostty-wasm and an xterm-style cell renderer. The Nix wrapper
//! will then point `$TACET_TERMINAL_UI` at the bundled `index.html`
//! and we drop the `placeholder_url()` fallback below.
//!
//! Why an inline `data:` URL rather than a packaged file: avoids
//! shipping any JS for alpha.1 (the placeholder is a few hundred
//! bytes of HTML), so the crate has zero non-cargo build inputs.
//! Once the real UI bundle exists, the env-var override path takes
//! priority and the `data:` URL becomes a dev-time fallback only.

use anyhow::Result;
use tacet_view::{View, ViewOptions};

/// Env var the Nix wrapper sets to point at the bundled UI's
/// `index.html`. When unset (e.g. `cargo run` during dev), we fall
/// back to the inline placeholder.
const ENV_UI: &str = "TACET_TERMINAL_UI";

fn main() -> Result<()> {
    let target = std::env::var(ENV_UI)
        .map(|p| format!("file://{p}"))
        .unwrap_or_else(|_| placeholder_url());

    let view = View::spawn(ViewOptions::new(target))?;
    let status = view.wait()?;
    std::process::exit(status.code().unwrap_or(0));
}

fn placeholder_url() -> String {
    // Base64-encoded so we sidestep URL-percent-encoding for `<` etc.
    // Small enough that we don't worry about Chromium's data: size cap
    // (~2 MB on most builds).
    let html = include_str!("placeholder.html");
    format!("data:text/html;base64,{}", b64(html.as_bytes()))
}

/// Minimal base64 encoder — pulling the `base64` crate just for one
/// fixed string at startup would be ceremony.
fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

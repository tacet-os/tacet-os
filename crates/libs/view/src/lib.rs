//! Contextless Chromium-backed web view with CDP exposed.
//!
//! `View::spawn` launches a system Chromium binary in `--app=` mode
//! (no browser chrome) pointed at a URL, with an ephemeral
//! `--user-data-dir` so no cookies/cache/localStorage persist across
//! launches, and `--remote-debugging-port` enabled so an agent (or
//! anything speaking CDP) can attach.
//!
//! The web engine is intentionally NOT embedded. WebKitGTK can't speak
//! CDP, and bundling CEF or building Servo would dwarf the rest of
//! tacet-os. Wrapping system Chromium keeps this crate to a few
//! hundred lines and inherits the distro's security patches.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use tempfile::TempDir;

/// Env var that overrides the Chromium binary name. Set by the Nix
/// wrapper to pin a specific `chromium` build.
pub const ENV_CHROMIUM_BIN: &str = "TACET_BROWSER_BIN";

const DEFAULT_CHROMIUM_BIN: &str = "chromium";

/// Whether Chromium should paint normal browser chrome (omnibox, tabs,
/// bookmarks bar) or be a frameless single-page surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeMode {
    /// `--app=URL` — frameless, no chrome. For embedded UIs like
    /// `tacet-terminal` or future agent canvases where the page IS the
    /// app surface.
    App,
    /// Positional URL — full Chromium chrome including the address bar.
    /// For `tacet-browser`, where the user expects to navigate.
    Browser,
}

/// Options for launching a [`View`].
pub struct ViewOptions {
    /// Already-resolved URL (use [`resolve_target`] for path-or-URL input).
    pub url: String,
    /// CDP port. `0` lets Chromium pick a free one; read back via
    /// [`View::cdp_endpoint`] once Chromium writes `DevToolsActivePort`.
    pub cdp_port: u16,
    /// Override the Chromium binary. Defaults to `$TACET_BROWSER_BIN`
    /// then `chromium` on PATH.
    pub chromium_bin: Option<OsString>,
    /// Additional Chromium flags appended after the built-in set.
    pub extra_args: Vec<OsString>,
    /// Chrome-paint behavior. Default is [`ChromeMode::App`] for
    /// back-compat with the original embedded-UI callsites.
    pub chrome: ChromeMode,
}

impl ViewOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            cdp_port: 0,
            chromium_bin: None,
            extra_args: Vec::new(),
            chrome: ChromeMode::App,
        }
    }

    pub fn with_chrome(mut self, chrome: ChromeMode) -> Self {
        self.chrome = chrome;
        self
    }
}

/// A running Chromium process plus its ephemeral profile directory.
///
/// Dropping `View` (or calling [`View::wait`]) cleans the profile dir.
/// `View` does not kill the child on drop — the typical caller wants
/// to wait for the user closing the window.
pub struct View {
    child: Child,
    profile: TempDir,
}

impl View {
    /// Spawn Chromium with the given options.
    pub fn spawn(opts: ViewOptions) -> Result<Self> {
        let profile = TempDir::new().context("creating ephemeral user-data-dir")?;
        let bin = opts
            .chromium_bin
            .or_else(|| env::var_os(ENV_CHROMIUM_BIN))
            .unwrap_or_else(|| OsString::from(DEFAULT_CHROMIUM_BIN));

        let mut cmd = Command::new(&bin);
        match opts.chrome {
            ChromeMode::App => {
                cmd.arg(format!("--app={}", opts.url));
            }
            ChromeMode::Browser => {
                // Positional URL → normal Chromium window with omnibox.
                // Must come AFTER all `--` flags or Chromium misparses it
                // as a flag value; we add it last via `.arg(url)` below.
            }
        }
        cmd.arg(format!(
            "--user-data-dir={}",
            profile.path().display()
        ));
        cmd.arg(format!("--remote-debugging-port={}", opts.cdp_port));
        cmd.args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-features=TranslateUI",
            // Belt-and-suspenders: NIXOS_OZONE_WL=1 covers most cases,
            // but the explicit flag avoids surprises if a consumer
            // launches us from a non-tacet base.
            "--ozone-platform=wayland",
        ]);
        cmd.args(opts.extra_args);
        if opts.chrome == ChromeMode::Browser {
            cmd.arg(&opts.url);
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", bin.to_string_lossy()))?;
        Ok(Self { child, profile })
    }

    /// Block until Chromium writes `DevToolsActivePort`, then return
    /// the CDP WebSocket URL (`ws://127.0.0.1:<port><path>`). Errors
    /// if the file does not appear within `timeout`.
    pub fn cdp_endpoint(&self, timeout: Duration) -> Result<String> {
        let path = self.profile.path().join("DevToolsActivePort");
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(s) = fs::read_to_string(&path) {
                let mut lines = s.lines();
                let port = lines.next().unwrap_or("").trim();
                let ws_path = lines.next().unwrap_or("/").trim();
                if !port.is_empty() {
                    return Ok(format!("ws://127.0.0.1:{port}{ws_path}"));
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("timed out waiting for {}", path.display())
    }

    /// Wait for Chromium to exit. Returns its exit status.
    pub fn wait(mut self) -> Result<ExitStatus> {
        Ok(self.child.wait()?)
    }

    /// Borrow the ephemeral profile path. Useful for tests that need
    /// to inspect Chromium's runtime files.
    pub fn profile_path(&self) -> &Path {
        self.profile.path()
    }
}

/// Convert a path-or-URL string into a URL Chromium can load.
///
/// - `http://`, `https://`, `file://`, `about:`, `data:` → returned as-is
/// - anything else is treated as a filesystem path and canonicalized
///   into a `file://` URL
pub fn resolve_target(target: &str) -> Result<String> {
    if has_url_scheme(target) {
        return Ok(target.to_owned());
    }
    let p = Path::new(target);
    let abs: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        env::current_dir()?.join(p)
    };
    let canonical = fs::canonicalize(&abs)
        .with_context(|| format!("resolving local path: {}", abs.display()))?;
    Ok(format!("file://{}", canonical.display()))
}

fn has_url_scheme(s: &str) -> bool {
    const SCHEMES: &[&str] = &[
        "http://", "https://", "file://", "about:", "data:", "chrome://",
    ];
    SCHEMES.iter().any(|p| s.starts_with(p))
}

/// Pass-through helper for callers that already have a binary name in
/// hand and want to honor [`ENV_CHROMIUM_BIN`].
pub fn resolve_chromium_bin(explicit: Option<&OsStr>) -> OsString {
    explicit
        .map(OsString::from)
        .or_else(|| env::var_os(ENV_CHROMIUM_BIN))
        .unwrap_or_else(|| OsString::from(DEFAULT_CHROMIUM_BIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_passes_urls_through() {
        for url in [
            "https://example.com",
            "http://localhost:1234/x",
            "file:///tmp/a.html",
            "about:blank",
            "data:text/html,<h1>hi</h1>",
        ] {
            assert_eq!(resolve_target(url).unwrap(), url);
        }
    }

    #[test]
    fn resolve_target_canonicalizes_paths() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ui.html");
        fs::write(&f, "<!doctype html>").unwrap();
        let resolved = resolve_target(f.to_str().unwrap()).unwrap();
        assert!(resolved.starts_with("file://"));
        assert!(resolved.ends_with("ui.html"));
    }
}

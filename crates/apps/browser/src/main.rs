//! tacet-browser — CLI shell around `tacet_view::View`.
//!
//! Usage:
//!   tacet-browser <url-or-path> [--cdp-port N] [-- <extra chromium args>]
//!
//! Always contextless. The resolved CDP WebSocket URL is printed to
//! stdout once Chromium publishes its DevTools port, so agents can
//! attach without parsing logs:
//!
//!   $ tacet-browser https://example.com
//!   ws://127.0.0.1:43219/devtools/browser/abc123...

use std::{env, ffi::OsString, process, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use tacet_view::{resolve_target, View, ViewOptions};

const CDP_READY_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> Result<()> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let parsed = match parse_args(&args)? {
        ParseResult::Run(p) => p,
        ParseResult::Help => {
            print_help();
            return Ok(());
        }
    };

    let url = resolve_target(&parsed.target)
        .with_context(|| format!("resolving target: {}", parsed.target))?;

    let view = View::spawn(ViewOptions {
        url,
        cdp_port: parsed.cdp_port,
        chromium_bin: None,
        extra_args: parsed.passthrough,
    })?;

    // stdout is reserved for the CDP endpoint so consumers can
    // capture it cleanly (`endpoint=$(tacet-browser ...)`). Any
    // diagnostic goes to stderr.
    match view.cdp_endpoint(CDP_READY_TIMEOUT) {
        Ok(ep) => println!("{ep}"),
        Err(e) => eprintln!("tacet-browser: CDP endpoint not resolved: {e}"),
    }

    let status = view.wait()?;
    process::exit(status.code().unwrap_or(0));
}

struct Parsed {
    target: String,
    cdp_port: u16,
    passthrough: Vec<OsString>,
}

enum ParseResult {
    Run(Parsed),
    Help,
}

fn parse_args(args: &[OsString]) -> Result<ParseResult> {
    let mut target: Option<String> = None;
    let mut cdp_port: u16 = 0;
    let mut passthrough: Vec<OsString> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].to_string_lossy();
        match a.as_ref() {
            "-h" | "--help" => return Ok(ParseResult::Help),
            "--cdp-port" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| anyhow!("--cdp-port expects a number"))?;
                cdp_port = v
                    .to_string_lossy()
                    .parse()
                    .context("--cdp-port must be a u16")?;
            }
            "--" => {
                passthrough.extend_from_slice(&args[i + 1..]);
                break;
            }
            _ if target.is_none() => target = Some(a.into_owned()),
            other => bail!("unexpected argument: {other}"),
        }
        i += 1;
    }
    let target = target.ok_or_else(|| anyhow!("missing <url-or-path>; see --help"))?;
    Ok(ParseResult::Run(Parsed {
        target,
        cdp_port,
        passthrough,
    }))
}

fn print_help() {
    println!(
        "tacet-browser <url-or-path> [--cdp-port N] [-- <chromium args...>]

Ephemeral Chromium window with CDP. The CDP WebSocket URL is printed
to stdout once Chromium publishes its DevTools port.

  <url-or-path>   http(s)://, file://, about:, data:, chrome://, or a
                  local filesystem path (canonicalized to file://).
  --cdp-port N    Pin the CDP port. Default 0 = let Chromium pick.
  --              Pass remaining args straight to Chromium.

Set TACET_BROWSER_BIN to override the Chromium binary (default: chromium)."
    );
}

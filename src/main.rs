use crate::app::{App, StartMode};
use crate::color::query_host_bg_color;
use crate::config::Config;
use crate::log_buffer::LogBuffer;

pub mod app;
pub mod claude_hooks;
pub mod clipboard;
pub mod cmd_hook;
pub mod cmd_parse;
pub mod color;
pub mod config;
pub mod cursor_hooks;
pub mod data_view;
pub mod event;
pub mod index;
pub mod log_buffer;
pub mod logging;
pub mod picker;
pub mod plugin_install;
pub mod providers;
pub mod reader_op;
pub mod session_socket;
pub mod status_bar;
pub mod terminal;
pub mod theme;
pub mod transforms;
pub mod tree_operation;
pub mod tree_scroll_view;

const USAGE: &str = "\
Usage: agt [--resume] [--debug] [<provider>[:<session-id>]]
       agt parse [--waterfall] [--debug] <provider>:<session-id>
       agt install-hooks <provider>
       agt -h | --help

Open a TUI wrapper around an agent CLI session (Claude Code, Cursor Agent).

Arguments:
  <provider>                   Start a new session  (claude, cursor)
  <provider>:<session-id>      Open an existing session transcript

Options:
      --resume                 Re-launch the agent CLI into the given session
      --debug                  Enable debug logging to /tmp/agent-transcript.log
  -h, --help                   Show this message

Subcommands:
  parse [--waterfall] [--debug] <provider>:<session-id>
                               Parse transcript, apply transforms, pretty-print tree and exit.
                               --waterfall simulates live streaming one entry at a time,
                               printing RESET ids when the reader rewinds.
  install-hooks [--force] <provider>
                               Install session hooks for the given provider.

Examples:
  agt                                  Open the session picker
  agt claude                           Start a new Claude Code session
  agt claude:abc123                    View session abc123
  agt --resume claude:abc123           Resume Claude Code in session abc123
  agt parse cursor:abc123              Dump parsed+transformed tree for a Cursor session
  agt parse claude:abc123              Dump parsed+transformed tree for a Claude session
  agt parse --waterfall claude:abc123  Simulate live streaming and show rewind events
";

/// Parse a pre-extracted (`resume` flag, optional free argument) into a `StartMode`.
///
/// Separated from `parse_args` so it can be unit-tested without touching process args.
fn parse_mode_args(resume: bool, free_arg: Option<&str>) -> color_eyre::Result<StartMode> {
    match free_arg {
        None => Ok(StartMode::Picker),
        Some(arg) => {
            if let Some(colon_pos) = arg.find(':') {
                let provider_str = &arg[..colon_pos];
                let id = arg[colon_pos + 1..].to_string();
                let provider = provider_str
                    .parse()
                    .map_err(|()| color_eyre::eyre::eyre!("unknown provider: {}", provider_str))?;
                Ok(StartMode::Session {
                    provider,
                    id,
                    resume,
                })
            } else {
                let provider = arg
                    .parse()
                    .map_err(|()| color_eyre::eyre::eyre!("unknown provider: {}", arg))?;
                Ok(StartMode::NewSession { provider })
            }
        }
    }
}

fn parse_args() -> color_eyre::Result<StartMode> {
    // Scan raw args first: pico_args would silently consume unknown flags as
    // positional args, so we must check before handing off.
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--resume" | "--debug" => {}
            s if s.starts_with('-') => {
                eprintln!("error: unknown argument: {s}\n");
                eprint!("{USAGE}");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    let mut args = pico_args::Arguments::from_env();
    let resume = args.contains("--resume");
    let _debug = args.contains("--debug"); // consumed here; init happens before parse_args

    let free = args
        .opt_free_from_str::<String>()
        .map_err(|e| color_eyre::eyre::eyre!("argument error: {e}"))?;

    // Catch extra positional args (flags are already ruled out above).
    let remaining = args.finish();
    if !remaining.is_empty() {
        let joined = remaining
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("error: unknown argument(s): {joined}\n");
        eprint!("{USAGE}");
        std::process::exit(1);
    }

    parse_mode_args(resume, free.as_deref())
}

fn main() {
    // Dispatch synchronous subcommands before starting the Tokio runtime.
    match std::env::args().nth(1).as_deref() {
        Some("hook") => {
            if let Err(e) = cmd_hook::run() {
                eprintln!("{e}");
                std::process::exit(1);
            }
            return;
        }
        Some("install-hooks") => {
            let mut force = false;
            let mut provider: Option<String> = None;
            for arg in std::env::args().skip(2) {
                if arg == "--force" {
                    force = true;
                } else {
                    provider = Some(arg);
                }
            }
            match provider.as_deref() {
                Some("claude") => {
                    if force {
                        if let Err(e) = claude_hooks::install() {
                            eprintln!("{e}");
                            std::process::exit(1);
                        }
                    } else {
                        println!(
                            "Claude hooks are configured automatically via --plugin-dir on each launch.\nTo install to ~/.claude/settings.json instead, run: agt install-hooks --force claude"
                        );
                    }
                }
                Some("cursor") => {
                    if let Err(e) = cursor_hooks::install() {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
                other => {
                    eprintln!(
                        "unknown provider {:?}\nusage: agt install-hooks <claude|cursor>",
                        other.unwrap_or("")
                    );
                    std::process::exit(1);
                }
            }
            return;
        }
        _ => {}
    }

    if let Err(e) = run_app() {
        eprintln!("{e:?}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run_app() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let debug = std::env::args().any(|a| a == "--debug");
    let is_parse = std::env::args().nth(1).as_deref() == Some("parse");
    let log_buffer = LogBuffer::new(2000);
    let debug_handle = logging::init_tracing(debug, log_buffer.clone(), is_parse)?;
    if debug {
        tracing::info!("debug logging enabled");
    }

    // Dispatch 'parse' before start_mode parsing (which would reject it as an unknown provider).
    if is_parse {
        let parse_args: Vec<String> = std::env::args().skip(2).collect();
        let waterfall = parse_args.iter().any(|a| a == "--waterfall");
        let session_arg = parse_args
            .iter()
            .find(|a| !a.starts_with('-'))
            .cloned()
            .unwrap_or_default();
        let config = Config::load();
        return cmd_parse::run(&session_arg, &config, waterfall).await;
    }

    let start_mode = parse_args().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    let host_bg = query_host_bg_color();
    let config = Config::load();
    let terminal = ratatui::init();
    let result = App::new(start_mode, host_bg, config, log_buffer, debug_handle)
        .await?
        .run(terminal)
        .await;
    ratatui::restore();
    if let Some((provider, session_id)) = result? {
        eprintln!("\nTo resume this session, run:");
        eprintln!("  agt --resume {}:{}", provider, session_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderKind;

    #[test]
    fn no_args_is_picker() {
        assert!(matches!(
            parse_mode_args(false, None).unwrap(),
            StartMode::Picker
        ));
    }

    #[test]
    fn claude_is_new_session() {
        let mode = parse_mode_args(false, Some("claude")).unwrap();
        assert!(matches!(
            mode,
            StartMode::NewSession {
                provider: ProviderKind::Claude
            }
        ));
    }

    #[test]
    fn cursor_is_new_session() {
        let mode = parse_mode_args(false, Some("cursor")).unwrap();
        assert!(matches!(
            mode,
            StartMode::NewSession {
                provider: ProviderKind::Cursor
            }
        ));
    }

    #[test]
    fn session_no_resume() {
        let mode = parse_mode_args(false, Some("claude:abc123")).unwrap();
        match mode {
            StartMode::Session {
                provider,
                id,
                resume,
            } => {
                assert_eq!(provider, ProviderKind::Claude);
                assert_eq!(id, "abc123");
                assert!(!resume);
            }
            _ => panic!("expected Session"),
        }
    }

    #[test]
    fn session_with_resume() {
        let mode = parse_mode_args(true, Some("claude:abc123")).unwrap();
        match mode {
            StartMode::Session {
                provider,
                id,
                resume,
            } => {
                assert_eq!(provider, ProviderKind::Claude);
                assert_eq!(id, "abc123");
                assert!(resume);
            }
            _ => panic!("expected Session"),
        }
    }

    #[test]
    fn unknown_provider_errors() {
        assert!(parse_mode_args(false, Some("bad")).is_err());
        assert!(parse_mode_args(false, Some("bad:id")).is_err());
    }
}

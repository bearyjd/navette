//! Navette CLI. Scaffolding only — every subcommand below prints a stub
//! message and exits non-zero. Multi-host targeting (PRP §4.2,
//! `navette <host> run <app>`) is not wired yet; that's part of the
//! Navette API design at milestone M1 (see `docs/prp/startup.md`).

use clap::{Parser, Subcommand};

/// Navette CLI — start, list, attach to, detach from, and kill
/// per-app GUI sessions running under `navetted`.
#[derive(Parser, Debug)]
#[command(name = "navette", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List all sessions.
    Ls,
    /// Start (or re-launch) a named app as a new session.
    Run {
        /// Application to launch (an XDG desktop entry id for now).
        app: String,
    },
    /// Attach to a running session.
    Attach {
        /// Session name to attach to.
        session: String,
    },
    /// Detach from a session without killing it.
    Detach {
        /// Session to detach from. Detaches the current session if omitted.
        session: Option<String>,
    },
    /// Kill a session.
    Kill {
        /// Session name to kill.
        session: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Ls => eprintln!("navette: not yet implemented"),
        Command::Run { app: _ } => eprintln!("navette: not yet implemented"),
        Command::Attach { session: _ } => eprintln!("navette: not yet implemented"),
        Command::Detach { session: _ } => eprintln!("navette: not yet implemented"),
        Command::Kill { session: _ } => eprintln!("navette: not yet implemented"),
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ls() {
        let cli = Cli::try_parse_from(["navette", "ls"]).unwrap();
        assert!(matches!(cli.command, Command::Ls));
    }

    #[test]
    fn parses_run_with_app() {
        let cli = Cli::try_parse_from(["navette", "run", "firefox"]).unwrap();
        match cli.command {
            Command::Run { app } => assert_eq!(app, "firefox"),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_attach_with_session() {
        let cli = Cli::try_parse_from(["navette", "attach", "work-browser"]).unwrap();
        match cli.command {
            Command::Attach { session } => assert_eq!(session, "work-browser"),
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn parses_detach_without_session() {
        let cli = Cli::try_parse_from(["navette", "detach"]).unwrap();
        match cli.command {
            Command::Detach { session } => assert_eq!(session, None),
            other => panic!("expected Detach, got {other:?}"),
        }
    }

    #[test]
    fn parses_detach_with_session() {
        let cli = Cli::try_parse_from(["navette", "detach", "work-browser"]).unwrap();
        match cli.command {
            Command::Detach { session } => assert_eq!(session, Some("work-browser".to_string())),
            other => panic!("expected Detach, got {other:?}"),
        }
    }

    #[test]
    fn parses_kill_with_session() {
        let cli = Cli::try_parse_from(["navette", "kill", "work-browser"]).unwrap();
        match cli.command {
            Command::Kill { session } => assert_eq!(session, "work-browser"),
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    #[test]
    fn rejects_run_without_app() {
        let result = Cli::try_parse_from(["navette", "run"]);
        assert!(result.is_err());
    }
}

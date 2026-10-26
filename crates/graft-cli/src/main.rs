//! `graft`: the command-line client (`docs/design.md` §3 verbs, §9).
//!
//! Every command takes `--json`, whose shape is the JSON Schema saved
//! under `crates/graft-cli/schemas/`. Exit codes follow design §8.2 ([`Exit`]).

#[macro_use]
mod out;

mod api;
#[cfg(test)]
mod build_rev;
mod cli;
mod cmd;
mod config;
mod conn;
mod error;
mod identity;
mod render;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

use cli::{Cli, Command};
use error::Failure;

/// Exit codes (`docs/design.md` §8.2). `Failure` is for everything §8.2 does
/// not name (not found, a damaged working copy, an I/O error); it is 1, the
/// conventional general failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Exit {
    /// Success.
    Ok = 0,
    /// Any other failure.
    Failure = 1,
    /// Usage error.
    Usage = 2,
    /// Land or sync conflict, or local changes that would be overwritten.
    Conflict = 3,
    /// Contended; retryable.
    Contended = 4,
    /// A land policy refused.
    Rejected = 5,
    /// Not authorized.
    Denied = 6,
    /// Unavailable or transport error; retryable.
    Unavailable = 7,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = legacy_role_entry(&args) {
        return code;
    }
    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        Err(e) => return parse_failure(e),
    };
    let mut cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => return parse_failure(e),
    };
    let json = cli.global.json;
    if let Err(failure) = enter_directory(&cli.global) {
        return ExitCode::from(failure.report(json) as u8);
    }
    if let Some(code) = role_entry(&cli.command) {
        return code;
    }
    mark_explicit_line(&mut cli, &matches);
    let result = dispatch(&cli);
    let code = match result {
        Ok(()) => Exit::Ok,
        Err(failure) => failure.report(json),
    };
    ExitCode::from(code as u8)
}

fn legacy_role_entry(args: &[String]) -> Option<ExitCode> {
    let executable = std::path::Path::new(args.first()?)
        .file_name()
        .and_then(|name| name.to_str())?;
    let rest = || args.iter().skip(1).cloned().collect();
    match executable {
        "graft-server" => Some(graft_server::run(rest())),
        "graft-node" => Some(graft_node::run(rest())),
        "graft-mirror" => Some(graft_mirror::run(rest())),
        _ => None,
    }
}

fn role_entry(command: &Command) -> Option<ExitCode> {
    match command {
        Command::Serve(args) => Some(serve_entry(args)),
        Command::Node(args) => Some(graft_node::run(args.args.clone())),
        Command::Mirror(args) => Some(graft_mirror::run(args.args.clone())),
        _ => None,
    }
}

/// A command line clap refused. Help and version print as usual. A usage
/// error prints as the JSON error object when `--json` is on the line.
fn parse_failure(e: clap::Error) -> ExitCode {
    if e.exit_code() == 0 {
        let _ = e.print();
        return ExitCode::SUCCESS;
    }
    if std::env::args_os().any(|a| a == "--json") {
        let first = e.to_string();
        let message = first.lines().next().unwrap_or("usage error");
        let message = message.strip_prefix("error: ").unwrap_or(message);
        let code = Failure::usage(message.to_owned()).report(true);
        return ExitCode::from(code as u8);
    }
    e.exit()
}

/// `graft -C <dir>` runs as if it had been started in `<dir>`.
fn enter_directory(g: &cli::Global) -> Result<(), Failure> {
    if let Some(dir) = &g.directory {
        std::env::set_current_dir(dir).map_err(|e| {
            Failure::from(std::io::Error::other(format!(
                "cannot enter {}: {e}",
                dir.display()
            )))
        })?;
    }
    Ok(())
}

/// Records that `--line` was written on the command line. Clap's default
/// is the same string, so the clone conflict check cannot see the difference
/// from the parsed value alone. `ValueSource` sees `--line b` and `--line=b`.
fn mark_explicit_line(cli: &mut Cli, matches: &clap::ArgMatches) {
    if let Command::Clone(args) = &mut cli.command
        && cli::clone_line_from_command_line(matches)
    {
        args.line_explicit = true;
    }
}

fn serve_entry(args: &cli::ServeArgs) -> ExitCode {
    if !args.dev {
        return graft_server::run(args.args.clone());
    }
    if !args.args.is_empty() {
        let failure =
            Failure::usage("serve --dev does not take production server arguments".to_owned());
        return ExitCode::from(failure.report(false) as u8);
    }
    match run_dev_server(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => ExitCode::from(failure.report(false) as u8),
    }
}

fn run_dev_server(args: &cli::ServeArgs) -> Result<(), Failure> {
    use std::net::SocketAddr;
    use std::sync::mpsc;

    let repo = args.repo.clone().unwrap_or_else(|| "dev".to_owned());
    let data = args
        .data
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from(".graft-serve"));
    let state = args
        .state
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from(".graft-serve-state"));
    let listen: SocketAddr = args
        .listen
        .as_deref()
        .unwrap_or("127.0.0.1:0")
        .parse()
        .map_err(|error| Failure::usage(format!("listen address: {error}")))?;
    let server_name = args.server.clone().unwrap_or_else(|| repo.clone());
    config::validate_name(&server_name)?;

    let user = config::load()?;
    let stored = match user.servers.get(&server_name) {
        Some(saved) => match std::fs::read_to_string(&saved.token_file) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(Failure::new(
                    Exit::Failure,
                    "config",
                    format!("read {}: {error}", saved.token_file.display()),
                ));
            }
        },
        None => None,
    };

    let mut options = graft_server::DevOptions::new(data, state);
    options.listen = listen;
    options.repo = Some(repo.clone());
    options.insecure_bind = args.insecure_bind;
    options.token = stored;
    let prepared = graft_server::DevServer::prepare(options)
        .map_err(|error| Failure::new(Exit::Failure, "serve", error))?;
    let minted = prepared.token().map(str::to_owned);
    let token_path = config::token_path(&server_name)?;
    if let Some(token) = &minted {
        config::store_token(&token_path, token)?;
    }

    let (tx, rx) = mpsc::channel();
    let repo_for_ready = repo;
    let ready = move |addr: SocketAddr| {
        tx.send(addr)
            .map_err(|_| "development server stopped before it was ready".to_owned())?;
        println!(
            "graft clone graft://{}:{}/{repo_for_ready}",
            addr.ip(),
            addr.port()
        );
        Ok(())
    };

    let handle = std::thread::spawn(move || prepared.serve(ready));
    let addr = match rx.recv() {
        Ok(addr) => addr,
        Err(_) => {
            let message = match handle.join() {
                Ok(Err(error)) => error,
                Ok(Ok(())) => "development server exited before it was ready".to_owned(),
                Err(_) => "development server panicked before it was ready".to_owned(),
            };
            return Err(Failure::new(Exit::Failure, "serve", message));
        }
    };
    let endpoint = format!("http://{}:{}", addr.ip(), addr.port());
    let mut loaded = config::load()?;
    loaded.servers.insert(
        server_name.clone(),
        config::ServerConfig {
            endpoint,
            token_file: config::token_path(&server_name)?,
        },
    );
    if loaded.default_server.is_none() {
        loaded.default_server = Some(server_name);
    }
    config::save(&loaded)?;
    handle
        .join()
        .map_err(|_| {
            Failure::new(
                Exit::Failure,
                "serve",
                "development server panicked".to_owned(),
            )
        })?
        .map_err(|error| Failure::new(Exit::Failure, "serve", error))
}

fn dispatch(cli: &Cli) -> Result<(), Failure> {
    let g = &cli.global;
    if matches!(cli.command, Command::Attest(_) | Command::Ci(_)) && !experimental_enabled(g) {
        return Err(Failure::usage(
            "experimental; enable with GRAFT_EXPERIMENTAL=1".to_owned(),
        ));
    }
    match &cli.command {
        Command::Login(args) => cmd::login::run(g, args),
        Command::Logout(args) => cmd::login::logout(g, args),
        Command::Config(args) => cmd::config::run(g, args),
        Command::Init(args) => cmd::init::run(g, args),
        Command::Clone(args) => cmd::clone::run(g, args),
        Command::Status(args) => cmd::status::run(g, args),
        Command::Diff(args) => cmd::diff::run(g, args),
        Command::Log(args) => cmd::log::run(g, args),
        Command::Show(args) => cmd::show::run(g, args),
        Command::Blame(args) => cmd::blame::run(g, args),
        Command::Cat(args) => cmd::cat::run(g, args),
        Command::Ls(args) => cmd::ls::run(g, args),
        Command::Sparse(args) => cmd::sparse::run(g, args),
        Command::Commit(args) => cmd::commit::run(g, args),
        Command::Workspace(args) => cmd::workspace::run(g, args),
        Command::Line(args) => cmd::line::run(g, args),
        Command::Tag(args) => cmd::tag::run(g, args),
        Command::Stack(args) => cmd::stack::run(g, args),
        Command::Sync => cmd::sync::run(g),
        Command::Land(args) => cmd::land::run(g, args),
        Command::Revert(args) => cmd::revert::run(g, args),
        Command::Pick(args) => cmd::pick::run(g, args),
        Command::Attest(args) => cmd::attest::run(g, args),
        Command::Ci(args) => cmd::ci::run(g, args),
        Command::Api(args) => api::run(g, args),
        Command::Version => cmd::version::run(g),
        Command::Mcp(args) => cmd::mcp::run(g, args),
        Command::Lsp { args } => match cmd::lsp::run(args) {
            Exit::Ok => Ok(()),
            code => Err(Failure::silent(code)),
        },
        Command::Serve(_) | Command::Node(_) | Command::Mirror(_) => {
            unreachable!("role command")
        }
        Command::Planned(args) => {
            let name = args.first().map_or("", String::as_str);
            Err(Failure::usage(format!("unknown command: {name}")))
        }
    }
}

fn experimental_enabled(global: &cli::Global) -> bool {
    global.experimental || std::env::var_os("GRAFT_EXPERIMENTAL").is_some_and(|value| value == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_design() {
        assert_eq!(Exit::Ok as u8, 0);
        assert_eq!(Exit::Usage as u8, 2);
        assert_eq!(Exit::Conflict as u8, 3);
        assert_eq!(Exit::Contended as u8, 4);
        assert_eq!(Exit::Rejected as u8, 5);
        assert_eq!(Exit::Denied as u8, 6);
        assert_eq!(Exit::Unavailable as u8, 7);
    }
}

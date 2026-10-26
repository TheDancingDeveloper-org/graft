//! Shell completion and manual-page generation from the clap command tree.

use std::fs::{self, File};
use std::io;
use std::path::Path;

use clap_complete::Shell;
use clap_mangen::Man;

use crate::Exit;
use crate::cli;
use crate::error::Failure;

/// Prints a completion script for `shell`.
pub(crate) fn run(shell: Shell) -> Result<(), Failure> {
    let mut command = cli::public_command();
    clap_complete::generate(shell, &mut command, "graft", &mut io::stdout());
    Ok(())
}

/// Writes one manual page for graft and each visible top-level subcommand.
pub(crate) fn man(out: &Path) -> Result<(), Failure> {
    fs::create_dir_all(out).map_err(|error| io_failure(out, error))?;

    let command = cli::public_command();
    render(&command, &out.join("graft.1"))?;
    for subcommand in command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
    {
        let name = subcommand.get_name();
        render(subcommand, &out.join(format!("graft-{name}.1")))?;
    }
    Ok(())
}

fn render(command: &clap::Command, path: &Path) -> Result<(), Failure> {
    let mut file = File::create(path).map_err(|error| io_failure(path, error))?;
    Man::new(command.clone())
        .render(&mut file)
        .map_err(|error| io_failure(path, error))
}

fn io_failure(path: &Path, error: io::Error) -> Failure {
    Failure::new(Exit::Failure, "io", format!("{}: {error}", path.display()))
}

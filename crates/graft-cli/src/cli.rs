//! The command line (`docs/design.md` §3, §9.3): the global options and each
//! command's arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The default line when no working copy or `--at` names a snapshot.
pub(crate) const DEFAULT_LINE: &str = "main";

/// Entries per page for the paged reads, unless `--limit` says otherwise.
pub(crate) const PAGE_DEFAULT: u32 = 100;

#[derive(Debug, Parser)]
#[command(
    name = "graft",
    about = "The graft command-line client.",
    disable_version_flag = true,
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: Global,
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Options every command accepts.
#[derive(Clone, Debug, Args)]
pub(crate) struct Global {
    /// Print the command's JSON, shaped by its schema in crates/graft-cli/schemas/
    #[arg(long, global = true)]
    pub(crate) json: bool,
    /// Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
    #[arg(long, global = true, value_name = "URL")]
    pub(crate) endpoint: Option<String>,
    /// Repository name (default: the working copy's)
    #[arg(long, global = true, value_name = "NAME")]
    pub(crate) repo: Option<String>,
    /// File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) token_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Open a workspace and lay out a sparse working copy
    Clone(CloneArgs),
    /// Working-copy changes against the base (no server needed)
    Status,
    /// Diff the working copy, or two changesets
    Diff(DiffArgs),
    /// First-parent history
    Log(LogArgs),
    /// A changeset with its patch
    Show(ShowArgs),
    /// Line origins for a file
    Blame(BlameArgs),
    /// Print a file at a changeset
    Cat(CatArgs),
    /// List a directory at a changeset
    Ls(LsArgs),
    /// List, add or remove the slice's path prefixes
    Sparse(SparseArgs),
    /// Call a read operation of the API, with JSON in and out
    Api(ApiArgs),
    /// Print versions
    Version,
    /// Run the LSP server over stdio (owned by G3.6)
    Lsp {
        /// Arguments passed to the LSP server
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Commands designed but not built yet
    #[command(external_subcommand)]
    Planned(Vec<String>),
}

#[derive(Debug, Args)]
pub(crate) struct CloneArgs {
    /// Repository to clone
    pub(crate) repo: String,
    /// Directory to create (default: the repository name's last segment)
    pub(crate) dir: Option<PathBuf>,
    /// Line whose tip to clone, unless `--at` is given
    #[arg(long, default_value = DEFAULT_LINE)]
    pub(crate) line: String,
    /// Snapshot to clone: a changeset id or a ref name
    #[arg(long, value_name = "REV")]
    pub(crate) at: Option<String>,
    /// Include only this path prefix (repeatable; default: the whole snapshot)
    #[arg(long = "path", value_name = "PREFIX")]
    pub(crate) paths: Vec<String>,
    /// With `--path`, also include the files at the repository root
    #[arg(long)]
    pub(crate) root_files: bool,
}

#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    /// The earlier changeset or ref
    pub(crate) from: Option<String>,
    /// The later changeset or ref. With neither, diff the working copy.
    pub(crate) to: Option<String>,
    /// Restrict to this path or directory (repeatable)
    #[arg(long = "path", value_name = "PATH")]
    pub(crate) paths: Vec<String>,
    /// Files per page
    #[arg(long, default_value_t = PAGE_DEFAULT)]
    pub(crate) limit: u32,
    /// Resume token from a previous page
    #[arg(long, value_name = "TOKEN")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    /// Start here: a changeset id or ref (default: the working copy's base, else `main`)
    #[arg(long, value_name = "REV")]
    pub(crate) at: Option<String>,
    /// Only changesets that touch this path or directory (repeatable)
    #[arg(long = "path", value_name = "PATH")]
    pub(crate) paths: Vec<String>,
    /// Changesets per page
    #[arg(long, default_value_t = 50)]
    pub(crate) limit: u32,
    /// Resume token from a previous page
    #[arg(long, value_name = "TOKEN")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// The changeset id or ref
    pub(crate) rev: String,
    /// Patch files per page
    #[arg(long, default_value_t = PAGE_DEFAULT)]
    pub(crate) limit: u32,
    /// Resume token from a previous page of files
    #[arg(long, value_name = "TOKEN")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct BlameArgs {
    /// The file
    pub(crate) path: String,
    /// The snapshot (default: the working copy's base, else `main`)
    #[arg(long, value_name = "REV")]
    pub(crate) at: Option<String>,
    /// Only lines `START:END` (0-based, END exclusive)
    #[arg(long, value_name = "START:END")]
    pub(crate) lines: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct CatArgs {
    /// The file
    pub(crate) path: String,
    /// The snapshot (default: the working copy's base, else `main`)
    #[arg(long, value_name = "REV")]
    pub(crate) at: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct LsArgs {
    /// The directory (default: the root)
    pub(crate) path: Option<String>,
    /// The snapshot (default: the working copy's base, else `main`)
    #[arg(long, value_name = "REV")]
    pub(crate) at: Option<String>,
    /// List every descendant, not only the direct children
    #[arg(long, short = 'R')]
    pub(crate) recursive: bool,
    /// Entries per page
    #[arg(long, default_value_t = PAGE_DEFAULT)]
    pub(crate) limit: u32,
    /// Resume token from a previous page
    #[arg(long, value_name = "TOKEN")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct SparseArgs {
    /// What to do (default: list)
    #[command(subcommand)]
    pub(crate) action: Option<SparseAction>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SparseAction {
    /// List the prefixes the working copy holds
    List,
    /// Add prefixes and fetch them
    Add {
        /// Path prefixes to add
        #[arg(required = true)]
        prefixes: Vec<String>,
    },
    /// Remove prefixes; their files stay on disk, untouched
    Remove {
        /// Path prefixes to remove
        #[arg(required = true)]
        prefixes: Vec<String>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct ApiArgs {
    /// The operation, as named by `--list` (for example `list_tree`)
    pub(crate) op: Option<String>,
    /// List every operation and how the CLI surfaces it
    #[arg(long, conflicts_with = "op")]
    pub(crate) list: bool,
    /// JSON request from a file, or `-` for standard input (default: standard input, when it is not a terminal)
    #[arg(long, value_name = "FILE")]
    pub(crate) input: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_tree_is_valid() {
        Cli::command().debug_assert();
    }
}

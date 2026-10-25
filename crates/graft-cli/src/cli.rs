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
    /// Run as if started in this directory, like `git -C`
    #[arg(short = 'C', long = "directory", global = true, value_name = "DIR")]
    pub(crate) directory: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Store credentials for a named server, or list configured servers
    Login(LoginArgs),
    /// Remove a named server and its stored token
    Logout(LogoutArgs),
    /// Inspect user configuration
    Config(ConfigArgs),
    /// Open a workspace and lay out a sparse working copy
    Clone(CloneArgs),
    /// Working-copy changes against the head draft, else the base (no server needed)
    ///
    /// With --upstream it also asks the server, without moving anything,
    /// how far the line tip is ahead of the base, which files changed there,
    /// and whether the files you changed (uncommitted edits and the stack)
    /// would merge onto the tip or conflict, with the conflicting lines.
    ///
    /// With --drafts it asks which other agents' live workspaces have open
    /// drafts on the files you changed (or on PATHS), before anyone lands.
    /// That needs the repo:<r>:drafts scope.
    Status(StatusArgs),
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
    /// Upload the slice's changes and create a draft on the server
    Commit(CommitArgs),
    /// Show, list, open, renew or close workspaces
    Workspace(WorkspaceArgs),
    /// Create or list lines
    Line(LineArgs),
    /// Create or list immutable tags
    Tag(TagArgs),
    /// List, squash, reorder, split or drop the drafts between the base and the head
    Stack(StackArgs),
    /// Move the base to the line tip (the server rebases a stack; conflicts are written into the files)
    Sync,
    /// Land the stack on the line, or hand it to the PR bridge
    Land(LandArgs),
    /// Add a draft that inverts a changeset
    Revert(RevertArgs),
    /// Add a draft that re-applies a landed changeset (a cherry-pick; conflicts are written into the files)
    Pick(PickArgs),
    /// Record a check result for a changeset (for CI)
    Attest(AttestArgs),
    /// CI helpers: exchange the job's identity token for a graft token
    Ci(CiArgs),
    /// Call a read operation of the API, with JSON in and out
    Api(ApiArgs),
    /// Print versions
    Version,
    /// Run the MCP server over stdio, for agents
    ///
    /// Serves graft's MCP tools against the remote API, at the endpoint the
    /// other commands use. Inside a working copy it adds the working-copy
    /// tools: graft_status, graft_commit with from_working_copy, and
    /// graft_land and graft_sync that move the working copy. Standard output
    /// is the MCP channel; messages go to standard error. The token comes
    /// only from --token-file or GRAFT_TOKEN.
    Mcp,
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
pub(crate) struct LoginArgs {
    /// Server name to store
    pub(crate) name: Option<String>,
    /// List configured servers without changing them
    #[arg(long, conflicts_with = "name")]
    pub(crate) list: bool,
}

#[derive(Debug, Args)]
pub(crate) struct LogoutArgs {
    /// Server name to remove
    pub(crate) name: String,
}

#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub(crate) command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Print named servers and the effective selection; tokens are redacted
    Show,
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
pub(crate) struct StatusArgs {
    /// Also compare with the line tip (read-only; contacts the server)
    #[arg(long)]
    pub(crate) upstream: bool,
    /// With --upstream: the most entries in each list
    #[arg(long, default_value_t = PAGE_DEFAULT, requires = "upstream")]
    pub(crate) limit: u32,
    /// With --upstream: resume token for the line's changed files
    #[arg(long, value_name = "TOKEN", requires = "upstream")]
    pub(crate) cursor: Option<String>,
    /// Also list other live workspaces with open drafts on your changed
    /// files (read-only; contacts the server)
    #[arg(long)]
    pub(crate) drafts: bool,
    /// Also list the untracked entries that `.gitignore` and
    /// `.graftignore` ignore (an ignored directory is one entry)
    #[arg(long)]
    pub(crate) ignored: bool,
    /// With --drafts: ask about these files or directories instead
    #[arg(value_name = "PATHS", requires = "drafts")]
    pub(crate) paths: Vec<String>,
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
    /// Per-file added and removed line counts, and a summary
    #[arg(long)]
    pub(crate) stat: bool,
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
pub(crate) struct CommitArgs {
    /// The message (required, unless --amend keeps the old one)
    #[arg(short, long, value_name = "MESSAGE")]
    pub(crate) message: Option<String>,
    /// Replace the head draft instead of adding one; the old head is superseded
    #[arg(long)]
    pub(crate) amend: bool,
    /// The author as `Name <email>` (default: GRAFT_AUTHOR_NAME and GRAFT_AUTHOR_EMAIL, else $USER with no email)
    #[arg(long, value_name = "AUTHOR")]
    pub(crate) author: Option<String>,
    /// What kind of author this is: human, agent, bot or ci (default: GRAFT_AUTHOR_KIND, else agent with VOGT_SESSION_ID set, else human)
    #[arg(long, value_name = "KIND")]
    pub(crate) author_kind: Option<String>,
    /// Metadata `KEY=VALUE` for tools, for example `vogt.work_item=WI-1` (repeatable)
    #[arg(long = "meta", value_name = "KEY=VALUE")]
    pub(crate) meta: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct WorkspaceArgs {
    /// What to do (default: show)
    #[command(subcommand)]
    pub(crate) action: Option<WorkspaceAction>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceAction {
    /// Show a workspace (default: the working copy's)
    Show {
        /// The workspace
        name: Option<String>,
    },
    /// List your workspaces
    List {
        /// Workspaces per page
        #[arg(long, default_value_t = PAGE_DEFAULT)]
        limit: u32,
        /// Resume token from a previous page
        #[arg(long, value_name = "TOKEN")]
        cursor: Option<String>,
    },
    /// Open a workspace: for the working copy at its base, or on a line's tip
    Open {
        /// The name (default: one the server picks)
        name: Option<String>,
        /// Outside a working copy, the line to track
        #[arg(long, default_value = DEFAULT_LINE)]
        line: String,
    },
    /// Renew a workspace's lease
    Renew {
        /// The workspace (default: the working copy's)
        name: Option<String>,
        /// Lease length in seconds (default: 2 hours)
        #[arg(long, value_name = "SECONDS")]
        lease: Option<u64>,
    },
    /// Close a workspace; its drafts become abandoned
    Close {
        /// The workspace (default: the working copy's)
        name: Option<String>,
        /// Ask the server to keep the drafts
        #[arg(long)]
        keep_drafts: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct LineArgs {
    /// What to do
    #[command(subcommand)]
    pub(crate) action: LineAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum LineAction {
    /// Fork a new line at a changeset; an existing line is never overwritten
    Create {
        /// The new line's name
        name: String,
        /// Where it starts: a changeset id or ref
        #[arg(long, value_name = "REV")]
        from: String,
    },
    /// Merge another line into a line (D29); only the target moves
    Merge {
        /// The source line, only read
        src: String,
        /// The target line (default: the working copy's line)
        #[arg(long, value_name = "LINE")]
        into: Option<String>,
        /// The source changeset to merge: an id or ref on the source line (default: its tip)
        #[arg(long, value_name = "REV")]
        at: Option<String>,
        /// Message of the merge landing
        #[arg(short, long, value_name = "MSG")]
        message: Option<String>,
        /// How a file both sides changed is merged: `line` (D16) or `file`
        #[arg(long, value_enum, value_name = "MODE", default_value_t = LandMerge::Line)]
        merge: LandMerge,
        /// On a conflict, sync this working copy (on the target line) to the tip and write the merge into the files
        #[arg(long)]
        sync: bool,
    },
    /// List a line's merge points, newest first
    Merges {
        /// The target line (default: the working copy's line)
        dst: Option<String>,
        /// Only merges from this line
        #[arg(long, value_name = "LINE")]
        from: Option<String>,
        /// Merge points per page
        #[arg(long, default_value_t = PAGE_DEFAULT)]
        limit: u32,
        /// Resume token from a previous page
        #[arg(long, value_name = "TOKEN")]
        cursor: Option<String>,
    },
    /// List the lines, in name order
    List {
        /// Only lines whose name starts with this
        #[arg(long, value_name = "PREFIX")]
        prefix: Option<String>,
        /// Lines per page
        #[arg(long, default_value_t = PAGE_DEFAULT)]
        limit: u32,
        /// Resume token from a previous page
        #[arg(long, value_name = "TOKEN")]
        cursor: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct TagArgs {
    /// What to do
    #[command(subcommand)]
    pub(crate) action: TagAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TagAction {
    /// Tag a changeset as `tags/<name>`; a tag never moves and is never re-created
    Create {
        /// The tag's name (`tags/` is added when missing)
        name: String,
        /// What to tag: a changeset id or ref
        #[arg(long, value_name = "REV", default_value = DEFAULT_LINE)]
        at: String,
    },
    /// List the tags, in name order
    List {
        /// Only tags whose name (after `tags/`) starts with this
        #[arg(long, value_name = "PREFIX")]
        prefix: Option<String>,
        /// Refs per page
        #[arg(long, default_value_t = PAGE_DEFAULT)]
        limit: u32,
        /// Resume token from a previous page
        #[arg(long, value_name = "TOKEN")]
        cursor: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct StackArgs {
    /// What to do (default: list)
    #[command(subcommand)]
    pub(crate) action: Option<StackAction>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum StackAction {
    /// List the drafts, oldest first
    List,
    /// Squash adjacent drafts into one (default: the whole stack)
    Squash {
        /// The drafts, by id or unique id prefix
        drafts: Vec<String>,
    },
    /// Put the drafts in a new order, oldest first
    Reorder {
        /// Every draft, by id or unique id prefix, oldest first
        #[arg(required = true)]
        drafts: Vec<String>,
    },
    /// Split a draft into one draft per part; paths no part names stay in a last draft
    Split {
        /// The draft, by id or unique id prefix
        draft: String,
        /// The paths (or directories) of one part, comma-separated (repeatable)
        #[arg(long = "part", value_name = "PATHS", required = true)]
        parts: Vec<String>,
    },
    /// Drop a draft from the stack
    Drop {
        /// The draft, by id or unique id prefix
        draft: String,
    },
}

#[derive(Debug, Args)]
pub(crate) struct LandArgs {
    /// Wait up to this many seconds: for a contended land (default 60), and for a queued pull request to merge (default: don't wait)
    #[arg(long, value_name = "SECONDS")]
    pub(crate) wait: Option<u64>,
    /// Land the stack as one changeset
    #[arg(long)]
    pub(crate) squash: bool,
    /// On a conflict, sync to the tip and write the conflicts into the files (still exits 3)
    #[arg(long)]
    pub(crate) sync: bool,
    /// How the server's rebase treats a file both sides changed: `line` merges text files line by line (D16), `file` refuses any file changed on both sides
    #[arg(long, value_enum, value_name = "MODE", default_value_t = LandMerge::Line)]
    pub(crate) merge: LandMerge,
    /// Skip the pre-land check (the dry-run rebase that stops a land on a real conflict, and the draft-overlap report)
    #[arg(long)]
    pub(crate) no_check: bool,
    /// Land a merge resolution (D29): record a merge point from `<line>@<rev>`
    #[arg(long, value_name = "LINE@REV")]
    pub(crate) merged_from: Option<String>,
}

/// How `graft land` merges a file both sides changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum LandMerge {
    /// Three-way line merge for text files under 1 MiB (decision D16)
    Line,
    /// Any file changed on both sides conflicts
    File,
}

#[derive(Debug, Args)]
pub(crate) struct RevertArgs {
    /// The changeset to invert: an id or a ref
    pub(crate) rev: String,
}

#[derive(Debug, Args)]
pub(crate) struct PickArgs {
    /// The landed changeset to re-apply: an id or a ref
    pub(crate) rev: String,
    /// How a file both sides changed is merged
    #[arg(long, value_enum, value_name = "MODE", default_value_t = LandMerge::Line)]
    pub(crate) merge: LandMerge,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("result").required(true).args(["pass", "fail"])))]
pub(crate) struct AttestArgs {
    /// The changeset checked: an id or a ref
    pub(crate) rev: String,
    /// The check's name, for example `ci/test`
    #[arg(long, value_name = "NAME")]
    pub(crate) check: String,
    /// The check passed
    #[arg(long)]
    pub(crate) pass: bool,
    /// The check failed
    #[arg(long)]
    pub(crate) fail: bool,
    /// A link to the run
    #[arg(long, value_name = "URL", default_value = "")]
    pub(crate) url: String,
}

#[derive(Debug, Args)]
pub(crate) struct CiArgs {
    #[command(subcommand)]
    pub(crate) action: CiAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CiAction {
    /// Exchange the CI job's identity token (on standard input) for a short graft token, written to --out
    Exchange(CiExchangeArgs),
}

/// The CI system a job runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum CiProvider {
    /// GitHub Actions OIDC
    Github,
    /// A Woodpecker job token
    Woodpecker,
}

impl CiProvider {
    /// The name on the command line.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Woodpecker => "woodpecker",
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct CiExchangeArgs {
    /// The CI system
    #[arg(long, value_enum)]
    pub(crate) provider: CiProvider,
    /// Pin the token to the changeset the job attests (a binding that can attest requires it)
    #[arg(long, value_name = "ID")]
    pub(crate) changeset: Option<String>,
    /// File to write the graft token to (mode 0600; replaced whole). The token is never printed.
    #[arg(long, value_name = "PATH")]
    pub(crate) out: PathBuf,
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

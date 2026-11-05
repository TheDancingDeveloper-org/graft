//! `graft repo`: creates repositories (`CreateRepo`, D41). Creating one
//! needs `repos:admin`, after which the creator administers it, or the
//! repository's own admin. A repository that exists is a successful retry.

use serde_json::json;

use crate::cli::{Global, RepoAction, RepoArgs};
use crate::conn::{call_context, connect, runtime};
use crate::error::Failure;
use crate::render::print_json;
use graft_client::workcopy::WorkingCopy;

/// Runs a repo action.
pub(crate) fn run(g: &Global, args: &RepoArgs) -> Result<(), Failure> {
    match &args.action {
        RepoAction::Create { name } => runtime()?.block_on(async {
            // The name (or a graft URL) is the repository to connect for.
            let mut g = g.clone();
            g.repo = Some(name.clone());
            let conn = connect(&g, None::<&WorkingCopy>).await?;
            let cx = call_context();
            conn.remote.create_repo(&cx, conn.repo.clone()).await?;
            if g.json {
                return print_json(&json!({ "repo": conn.repo }));
            }
            outln!("created repository {}", conn.repo);
            Ok(())
        }),
    }
}

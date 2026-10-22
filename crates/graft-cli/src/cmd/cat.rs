//! `graft cat`: a file's bytes at a changeset. Text goes to standard output
//! as it is; `--json` carries the content as UTF-8, or base64 when it is not.

use std::io::Write;

use graft_proto::api::RepoApi;
use graft_proto::types::PathAt;

use crate::cli::{CatArgs, Global};
use crate::conn::{call_context, connect, default_rev, repo_path, runtime, working_copy};
use crate::error::Failure;
use crate::render::{file_content, print_json};

/// Prints a file at a changeset.
pub(crate) fn run(g: &Global, args: &CatArgs) -> Result<(), Failure> {
    let path = repo_path(&args.path)?;
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let rev = args.at.clone().unwrap_or_else(|| default_rev(wc.as_ref()));
        let at = conn.resolve(&cx, &rev).await?;
        let content = conn
            .remote
            .read_file(
                &cx,
                PathAt {
                    repo: conn.repo.clone(),
                    at,
                    path,
                },
            )
            .await?;
        if g.json {
            return print_json(&file_content(at, &args.path, &content));
        }
        let mut out = std::io::stdout().lock();
        out.write_all(&content.data)?;
        out.flush()?;
        Ok(())
    })
}

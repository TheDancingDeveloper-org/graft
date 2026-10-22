//! `graft ls`: a directory's entries at a changeset, one page at a time.

use graft_proto::api::RepoApi;
use graft_proto::types::{ListTree, Page};

use crate::cli::{Global, LsArgs};
use crate::conn::{call_context, connect, default_rev, repo_path, runtime, working_copy};
use crate::error::Failure;
use crate::render::{entries, kind_name, print_json};

/// Lists a directory at a changeset.
pub(crate) fn run(g: &Global, args: &LsArgs) -> Result<(), Failure> {
    let path = args.path.as_deref().map(repo_path).transpose()?;
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let rev = args.at.clone().unwrap_or_else(|| default_rev(wc.as_ref()));
        let at = conn.resolve(&cx, &rev).await?;
        let page = conn
            .remote
            .list_tree(
                &cx,
                ListTree {
                    repo: conn.repo.clone(),
                    at,
                    path: path.clone(),
                    recursive: args.recursive,
                    page: Page {
                        limit: args.limit,
                        token: args.cursor.clone(),
                    },
                },
            )
            .await?;
        if g.json {
            let mut value = entries(&page);
            value["at"] = serde_json::json!(at.to_string());
            value["path"] = serde_json::json!(args.path);
            return print_json(&value);
        }
        for e in &page.entries {
            outln!("{:<9} {}", kind_name(e.kind), e.path);
        }
        if let Some(next) = &page.next {
            outln!("more: --cursor {next}");
        }
        Ok(())
    })
}

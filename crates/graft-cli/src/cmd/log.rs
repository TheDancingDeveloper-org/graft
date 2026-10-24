//! `graft log`: first-parent history, newest first, from the server.

use graft_proto::api::RepoApi;
use graft_proto::types::{Log, Page};

use crate::cli::{Global, LogArgs};
use crate::conn::{call_context, connect, default_rev, repo_path, runtime, working_copy_info};
use crate::error::Failure;
use crate::render::{history, print_json, short};

/// Prints a page of history.
pub(crate) fn run(g: &Global, args: &LogArgs) -> Result<(), Failure> {
    let paths = args
        .paths
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let rev = args.at.clone().unwrap_or_else(|| default_rev(wc.as_ref()));
        let from = conn.resolve(&cx, &rev).await?;
        let page = conn
            .remote
            .log(
                &cx,
                Log {
                    repo: conn.repo.clone(),
                    from,
                    until: None,
                    paths,
                    page: Page {
                        limit: args.limit,
                        token: args.cursor.clone(),
                    },
                },
            )
            .await?;
        if g.json {
            return print_json(&history(&page));
        }
        for s in &page.changesets {
            outln!("{}  {}  ({})", short(s.id), s.subject, s.author.name);
        }
        if let Some(next) = &page.next {
            outln!("more: --cursor {next}");
        }
        Ok(())
    })
}

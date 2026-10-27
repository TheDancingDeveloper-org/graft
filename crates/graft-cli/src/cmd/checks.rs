//! `graft checks <line>`: check state for landed changesets.

use graft_proto::api::RepoApi;
use graft_proto::types::{GetLandChecks, LandCheckState, Page};
use serde_json::json;

use crate::cli::{ChecksArgs, Global, PAGE_DEFAULT};
use crate::conn::{call_context, connect, runtime, working_copy_info};
use crate::error::Failure;
use crate::render::{print_json, short};

pub(crate) fn run(g: &Global, args: &ChecksArgs) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let page = conn
            .remote
            .get_land_checks(
                &call_context(),
                GetLandChecks {
                    repo: conn.repo.clone(),
                    line: args.line.clone(),
                    from_pos: args.from,
                    page: Page {
                        limit: PAGE_DEFAULT,
                        token: None,
                    },
                },
            )
            .await?;
        if g.json {
            return print_json(&json!({
                "line": args.line,
                "checks": page.rows.iter().map(|row| json!({
                    "pos": row.pos,
                    "changeset": row.changeset.to_string(),
                    "state": state_name(row.state),
                    "revert": row.revert.map(|id| id.to_string()),
                })).collect::<Vec<_>>(),
                "next": page.next,
            }));
        }
        for row in page.rows {
            outln!(
                "{} {} {}",
                row.pos,
                short(row.changeset),
                state_name(row.state)
            );
        }
        if let Some(next) = page.next {
            outln!("next {next}");
        }
        Ok(())
    })
}

fn state_name(state: LandCheckState) -> &'static str {
    match state {
        LandCheckState::Pending => "pending",
        LandCheckState::Good => "good",
        LandCheckState::Bad => "bad",
        LandCheckState::Reverted => "reverted",
        _ => "unknown",
    }
}

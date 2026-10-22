//! `graft blame`: the changeset that introduced each run of lines, from the
//! server's history (never a local walk).

use graft_proto::api::RepoApi;
use graft_proto::types::Blame;

use crate::cli::{BlameArgs, Global};
use crate::conn::{call_context, connect, default_rev, repo_path, runtime, working_copy};
use crate::error::Failure;
use crate::render::{blame, print_json, short};

/// Parses `START:END` (0-based, end exclusive).
fn parse_lines(text: &str) -> Result<(u32, u32), Failure> {
    let bad = || {
        Failure::usage(format!(
            "--lines wants START:END with START < END, got {text:?}"
        ))
    };
    let (start, end) = text.split_once(':').ok_or_else(bad)?;
    let start: u32 = start.trim().parse().map_err(|_| bad())?;
    let end: u32 = end.trim().parse().map_err(|_| bad())?;
    if start >= end {
        return Err(bad());
    }
    Ok((start, end))
}

/// Prints the line origins of a file.
pub(crate) fn run(g: &Global, args: &BlameArgs) -> Result<(), Failure> {
    let path = repo_path(&args.path)?;
    let lines = args.lines.as_deref().map(parse_lines).transpose()?;
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let rev = args.at.clone().unwrap_or_else(|| default_rev(wc.as_ref()));
        let at = conn.resolve(&cx, &rev).await?;
        let result = conn
            .remote
            .blame(
                &cx,
                Blame {
                    repo: conn.repo.clone(),
                    at,
                    path: path.clone(),
                    lines,
                },
            )
            .await?;
        if g.json {
            return print_json(&blame(at, &args.path, &result));
        }
        for r in &result.ranges {
            let by = &r.origin;
            outln!(
                "{:>5}-{:<5} {} {}  {}",
                r.start + 1,
                r.end,
                short(by.id),
                by.author.name,
                by.subject
            );
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_parse_and_reject_empty_ones() {
        assert_eq!(parse_lines("2:5").unwrap(), (2, 5));
        assert!(parse_lines("5:5").is_err());
        assert!(parse_lines("7").is_err());
    }
}

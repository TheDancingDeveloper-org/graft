//! `graft clone`: opens a working copy on a line's tip, or at a snapshot,
//! and fetches only the slice's objects (`docs/design.md` §9).

use std::path::{Path, PathBuf};

use graft_client::Sparse;
use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{self, WcConfig};
use serde_json::json;

use crate::cli::{CloneArgs, Global};
use crate::conn::{call_context, connect, repo_path, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// The directory a clone of `repo` goes into when none is named.
fn default_dir(repo: &str) -> PathBuf {
    Path::new(repo)
        .file_name()
        .map_or_else(|| PathBuf::from("graft-clone"), PathBuf::from)
}

/// Clones the repository into a new working copy.
pub(crate) fn run(g: &Global, args: &CloneArgs) -> Result<(), Failure> {
    if args.root_files && args.paths.is_empty() {
        return Err(Failure::usage("--root-files needs at least one --path"));
    }
    let paths = args
        .paths
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    let sparse = if paths.is_empty() {
        Sparse::full()
    } else {
        Sparse::new(paths, args.root_files)
    };
    let mut g = g.clone();
    g.repo = Some(args.repo.clone());
    let dest = args.dir.clone().unwrap_or_else(|| default_dir(&args.repo));

    runtime()?.block_on(async {
        let cx = call_context();
        let conn = connect(&g, None).await?;
        let at = match &args.at {
            Some(rev) => Some(conn.resolve(&cx, rev).await?),
            None => None,
        };
        let config = WcConfig {
            endpoint: Some(conn.remote.endpoint().to_string()),
            repo: conn.repo.clone(),
            workspace: None,
        };
        let (wc, stats) = workcopy::clone(
            &conn.remote,
            &cx,
            &dest,
            config,
            &args.line,
            at,
            sparse,
            &FetchOptions::default(),
        )
        .await?;
        let base = wc.state().base;
        let files = wc.index().entries.len();
        if g.json {
            return print_json(&json!({
                "repo": conn.repo,
                "line": args.line,
                "base": base.to_string(),
                "dir": dest.display().to_string(),
                "files": files,
                "fetch": {
                    "trees": stats.trees,
                    "blobs": stats.blobs,
                    "bytes": stats.bytes,
                    "requests": stats.requests,
                    "cached": stats.cached,
                },
            }));
        }
        println!(
            "cloned {} {} at {} into {} ({files} files)",
            conn.repo,
            args.line,
            short(base),
            dest.display()
        );
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_directory_is_the_last_segment() {
        assert_eq!(default_dir("team/app"), PathBuf::from("app"));
        assert_eq!(default_dir("app"), PathBuf::from("app"));
    }
}

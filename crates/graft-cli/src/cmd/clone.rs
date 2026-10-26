//! `graft clone`: opens a working copy on a line's tip, or at a snapshot,
//! and fetches only the slice's objects (`docs/design.md` §9).

use std::path::{Path, PathBuf};

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{self, WcConfig, WcInfo};
use graft_client::{RepoUrl, Sparse};
use serde_json::json;

use crate::cli::{CloneArgs, Global};
use crate::conn::{call_context, connect, repo_path, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// A repository argument that may be a `graft://` URL.
struct CloneTarget {
    repo: String,
    endpoint: Option<String>,
    line: Option<String>,
}

fn clone_target(repo: &str) -> Result<CloneTarget, Failure> {
    if repo.starts_with("graft://") || repo.starts_with("grafts://") {
        let url = RepoUrl::parse(repo).map_err(Failure::usage)?;
        Ok(CloneTarget {
            repo: url.repo,
            endpoint: Some(url.endpoint),
            line: url.line,
        })
    } else if repo.contains("://") {
        Err(Failure::usage(
            "expected a repository name, graft://host[:port]/repo, or grafts://host[:port]/repo",
        ))
    } else {
        Ok(CloneTarget {
            repo: repo.to_owned(),
            endpoint: None,
            line: None,
        })
    }
}

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
    let target = clone_target(&args.repo)?;
    if let Some(from_url) = &target.line
        && args.line_explicit
        && args.line != *from_url
    {
        return Err(Failure::usage(
            "the URL already names a line; drop --line or drop ?line=",
        ));
    }
    let line = target.line.clone().unwrap_or_else(|| args.line.clone());
    let mut g = g.clone();
    // Keep the URL on `--repo` so connect matches a named server to it.
    // `?line=` is clone-only and is not part of that argument.
    g.repo = Some(match &target.endpoint {
        Some(endpoint) => {
            let scheme = if endpoint.starts_with("https://") {
                "grafts"
            } else {
                "graft"
            };
            let authority = endpoint
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(endpoint);
            format!("{scheme}://{authority}/{}", target.repo)
        }
        None => target.repo.clone(),
    });
    let dest = args
        .dir
        .clone()
        .unwrap_or_else(|| default_dir(&target.repo));

    runtime()?.block_on(async {
        let cx = call_context();
        let conn = connect(&g, None::<&WcInfo>).await?;
        let at = match &args.at {
            Some(rev) => Some(conn.resolve(&cx, rev).await?),
            None => None,
        };
        let config = WcConfig {
            endpoint: conn
                .server
                .is_none()
                .then(|| conn.remote.endpoint().map(ToString::to_string))
                .flatten(),
            repo: conn.repo.clone(),
            workspace: None,
        };
        let (mut wc, stats) = workcopy::clone(
            &conn.remote,
            &cx,
            &dest,
            config,
            &line,
            at,
            sparse,
            &FetchOptions::default(),
        )
        .await?;
        wc.set_server(conn.server.clone())?;
        let base = wc.state().base;
        let files = wc.index().entries.len();
        if g.json {
            return print_json(&json!({
                "repo": conn.repo,
                "endpoint": conn.remote.endpoint().map(ToString::to_string),
                "line": line,
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
        outln!(
            "cloned {} {} at {} into {} ({files} files)",
            conn.repo,
            line,
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

    #[test]
    fn url_sets_repo_endpoint_and_line() {
        let target = clone_target("graft://graft-dev:17888/rusttorrent?line=trial").unwrap();
        assert_eq!(target.repo, "rusttorrent");
        assert_eq!(target.endpoint.as_deref(), Some("http://graft-dev:17888"));
        assert_eq!(target.line.as_deref(), Some("trial"));
        let bare = clone_target("demo").unwrap();
        assert!(bare.endpoint.is_none());
        assert!(clone_target("graft://user:secret@host/repo").is_err());
        assert!(clone_target("https://host/repo").is_err());
    }
}

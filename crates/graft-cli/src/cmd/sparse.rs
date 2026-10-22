//! `graft sparse`: the slice's path prefixes (`docs/design.md` §9.1).
//!
//! Adding or removing prefixes rewrites `.graft/sparse` and checks the new
//! slice out again. A checkout overwrites the files it writes, so the change
//! is refused while the working copy has local changes, and a failed checkout
//! puts the old prefixes back.

use std::fs;
use std::io;
use std::path::Path;

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{DEFAULT_CACHE_BYTES, GRAFT_DIR, WorkingCopy};
use graft_client::{Error as ClientError, Sparse};
use graft_model::RepoPath;
use serde_json::json;

use crate::cli::{Global, SparseAction, SparseArgs};
use crate::conn::{call_context, connect, repo_path, require_working_copy, runtime};
use crate::error::Failure;
use crate::render::print_json;

/// The sparse file's name in `.graft/` (design §9.1).
const SPARSE_FILE: &str = "sparse";

/// Lists the slice, or adds or removes prefixes.
pub(crate) fn run(g: &Global, args: &SparseArgs) -> Result<(), Failure> {
    match &args.action {
        None | Some(SparseAction::List) => {
            let wc = require_working_copy()?;
            print_sparse(g.json, wc.sparse())
        }
        Some(SparseAction::Add { prefixes }) => change(g, prefixes, Change::Add),
        Some(SparseAction::Remove { prefixes }) => change(g, prefixes, Change::Remove),
    }
}

/// Which way a prefix change goes.
#[derive(Clone, Copy)]
enum Change {
    Add,
    Remove,
}

/// The slice as JSON or text.
fn print_sparse(json: bool, sparse: &Sparse) -> Result<(), Failure> {
    let prefixes: Vec<String> = sparse.prefixes.iter().map(ToString::to_string).collect();
    if json {
        return print_json(&json!({
            "full": sparse.is_full(),
            "prefixes": prefixes,
            "root_files": sparse.root_files,
        }));
    }
    if sparse.is_full() {
        outln!("full snapshot");
    }
    for prefix in &prefixes {
        outln!("{prefix}");
    }
    if sparse.root_files {
        outln!("(root files)");
    }
    Ok(())
}

/// Applies a prefix change, then checks the new slice out.
fn change(g: &Global, prefixes: &[String], how: Change) -> Result<(), Failure> {
    let named = prefixes
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<RepoPath>, _>>()?;
    runtime()?.block_on(async {
        let wc = require_working_copy()?;
        let current = wc.sparse().clone();
        if current.is_full() {
            return Err(Failure::usage(
                "the working copy holds the whole snapshot; it has no prefixes to change",
            ));
        }
        let mut kept = current.prefixes.clone();
        match how {
            Change::Add => kept.extend(named),
            Change::Remove => {
                for path in &named {
                    if !kept.contains(path) {
                        return Err(Failure::usage(format!("{path} is not in the slice")));
                    }
                    kept.retain(|k| k != path);
                }
                if kept.is_empty() {
                    return Err(Failure::usage(
                        "cannot remove every prefix: an empty slice is the whole snapshot",
                    ));
                }
            }
        }
        let next = Sparse::new(kept, current.root_files);
        if next == current {
            return print_sparse(g.json, &current);
        }
        let changes = wc.status()?;
        if !changes.is_empty() {
            let paths = changes.iter().map(|c| c.path().clone()).collect();
            return Err(ClientError::Dirty(paths).into());
        }

        let conn = connect(g, Some(&wc)).await?;
        let cx = call_context();
        write_sparse(wc.root(), &next)?;
        let fetched = fetch_slice(wc.root(), &conn.remote, &cx).await;
        match fetched {
            Ok(fresh) => print_sparse(g.json, fresh.sparse()),
            Err(e) => {
                if let Err(restore) = write_sparse(wc.root(), &current) {
                    eprintln!("graft: could not restore .graft/sparse: {restore}");
                }
                Err(e)
            }
        }
    })
}

/// Reopens the working copy with the new prefixes and checks them out.
async fn fetch_slice(
    root: &Path,
    remote: &graft_client::Remote,
    cx: &graft_proto::CallContext,
) -> Result<WorkingCopy, Failure> {
    let mut fresh = WorkingCopy::open(root)?;
    let cache = fresh.open_cache(DEFAULT_CACHE_BYTES)?;
    fresh
        .checkout(remote, &cache, cx, &FetchOptions::default())
        .await?;
    Ok(fresh)
}

/// Replaces `.graft/sparse` by a rename, so a reader never sees half a file.
fn write_sparse(root: &Path, sparse: &Sparse) -> io::Result<()> {
    let dir = root.join(GRAFT_DIR);
    let tmp = dir.join(format!("{SPARSE_FILE}.graft-tmp"));
    fs::write(&tmp, sparse.to_text())?;
    fs::rename(tmp, dir.join(SPARSE_FILE))
}

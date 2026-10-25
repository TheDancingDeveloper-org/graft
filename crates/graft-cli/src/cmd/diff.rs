//! `graft diff`: two changesets (server-side, with patches), or the working
//! copy against its base (paths and statuses, no patch text).
//!
//! `--stat` prints per-file added and removed line counts, counted from the
//! patch the command already asks for (or, for the working copy, a local
//! patch of the same shape), capped the same way the patch is.

use std::fs;

use graft_client::fetch::load_blob;
use graft_client::workcopy::{FileStatus, WorkingCopy};
use graft_model::{Change, EntryKind};
use graft_proto::api::RepoApi;
use graft_proto::types::{Diff, Page, PatchOptions};
use serde_json::json;

use crate::cli::{DiffArgs, Global};
use crate::cmd::status::{change_json, word};
use crate::conn::{
    call_context, connect, repo_path, require_working_copy, runtime, working_copy_info,
};
use crate::error::Failure;
use crate::render::{diff, kind_name, print_json, short};

/// Context lines and the per-file patch cap, as the MCP `graft_diff` tool uses.
const PATCH_CONTEXT: u32 = 3;
const PATCH_MAX_BYTES: u64 = 64 * 1024;

/// How many files `--stat` counts. Past this the rest are named only.
const STAT_FILES: usize = 100;

/// Diffs two changesets, or the working copy.
pub(crate) fn run(g: &Global, args: &DiffArgs) -> Result<(), Failure> {
    match (&args.from, &args.to) {
        (None, None) => working_copy_diff(g, args.stat),
        (Some(from), Some(to)) => changeset_diff(g, args, from, to),
        _ => Err(Failure::usage(
            "give two changesets (FROM TO), or none to diff the working copy",
        )),
    }
}

/// The working copy against its base: the changed paths and how each changed.
fn working_copy_diff(g: &Global, stat: bool) -> Result<(), Failure> {
    let wc = require_working_copy()?;
    let changes = wc.status()?;
    if g.json {
        let mut value = json!({
            "from": wc.state().base.to_string(),
            "to": null,
            "files": changes.iter().map(change_json).collect::<Vec<_>>(),
        });
        if stat {
            value["stat"] = json!(working_copy_stat(&wc, &changes));
        }
        return print_json(&value);
    }
    if stat {
        let rows = working_copy_stat(&wc, &changes);
        print_stat(&rows)?;
        return Ok(());
    }
    for change in &changes {
        outln!("{:<9} {}", word(change), change.path());
    }
    if changes.is_empty() {
        outln!("no changes against {}", short(wc.state().base));
    } else {
        outln!("(paths only; diff two changesets for patches)");
    }
    Ok(())
}

/// Added and removed lines against the base blob in the local cache.
fn working_copy_stat(wc: &WorkingCopy, changes: &[FileStatus]) -> Vec<StatRow> {
    let cache = wc.open_cache(64 * 1024 * 1024).ok();
    let rt = tokio::runtime::Builder::new_current_thread().build().ok();
    changes
        .iter()
        .take(STAT_FILES)
        .map(|change| {
            let counts = match (&cache, &rt) {
                (Some(cache), Some(rt)) => rt.block_on(one_stat(wc, cache, change)),
                _ => (0, 0, true),
            };
            StatRow {
                path: change.path().to_string(),
                added: counts.0,
                removed: counts.1,
                binary: counts.2,
            }
        })
        .collect()
}

async fn one_stat(
    wc: &WorkingCopy,
    cache: &graft_store_fs::FsObjectStore,
    change: &FileStatus,
) -> (u32, u32, bool) {
    let path = change.path();
    let old = match change {
        FileStatus::Added(_) => None,
        FileStatus::Modified(_) | FileStatus::Deleted(_) => {
            let Some(entry) = wc.index().entries.get(path) else {
                return (0, 0, true);
            };
            if !matches!(entry.kind, EntryKind::File | EntryKind::Exec) {
                return (0, 0, true);
            }
            match load_blob(cache, entry.id.0).await {
                Ok(Some(bytes)) => Some(bytes),
                _ => return (0, 0, true),
            }
        }
    };
    let new = match change {
        FileStatus::Deleted(_) => None,
        FileStatus::Added(_) | FileStatus::Modified(_) => {
            let file = wc.root().join(path.as_str());
            match fs::read(&file) {
                Ok(bytes) => Some(bytes),
                Err(_) => return (0, 0, true),
            }
        }
    };
    lines_of(old.as_deref(), new.as_deref())
}

/// Insertions and deletions by line. A changed line counts as one of each.
/// Non-UTF-8 is binary.
fn lines_of(old: Option<&[u8]>, new: Option<&[u8]>) -> (u32, u32, bool) {
    let old = match as_text(old) {
        Ok(v) => v,
        Err(()) => return (0, 0, true),
    };
    let new = match as_text(new) {
        Ok(v) => v,
        Err(()) => return (0, 0, true),
    };
    let (added, removed) = diff_lines(old.unwrap_or(""), new.unwrap_or(""));
    (added, removed, false)
}

fn as_text(bytes: Option<&[u8]>) -> Result<Option<&str>, ()> {
    match bytes {
        None => Ok(None),
        Some(b) => std::str::from_utf8(b).map(Some).map_err(|_| ()),
    }
}

/// Line counts of a histogram-free diff: equal lines cancel, the rest are
/// insertions or deletions. Order is not aligned, so a moved line counts as
/// unchanged; a changed line counts as one deletion and one insertion.
fn diff_lines(old: &str, new: &str) -> (u32, u32) {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<&str, i32> = BTreeMap::new();
    for line in old.lines() {
        *counts.entry(line).or_default() -= 1;
    }
    for line in new.lines() {
        *counts.entry(line).or_default() += 1;
    }
    let mut added = 0u32;
    let mut removed = 0u32;
    for n in counts.values() {
        if *n > 0 {
            added = added.saturating_add(*n as u32);
        } else {
            removed = removed.saturating_add((-*n) as u32);
        }
    }
    (added, removed)
}

/// Two changesets, server-side, one page of files.
fn changeset_diff(g: &Global, args: &DiffArgs, from: &str, to: &str) -> Result<(), Failure> {
    let paths = args
        .paths
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let from = conn.resolve(&cx, from).await?;
        let to = conn.resolve(&cx, to).await?;
        let result = conn
            .remote
            .diff(
                &cx,
                Diff {
                    repo: conn.repo.clone(),
                    from,
                    to,
                    paths,
                    patch: Some(PatchOptions {
                        context: PATCH_CONTEXT,
                        max_bytes_per_file: PATCH_MAX_BYTES,
                    }),
                    page: Page {
                        limit: args.limit,
                        token: args.cursor.clone(),
                    },
                },
            )
            .await?;
        if g.json {
            let mut value = diff(from, to, &result);
            if args.stat {
                value["stat"] = json!(stat_of(&result.files));
            }
            return print_json(&value);
        }
        if args.stat {
            print_stat(&stat_of(&result.files))?;
            if let Some(next) = &result.next {
                outln!("more: --cursor {next}");
            }
            return Ok(());
        }
        for f in &result.files {
            match &f.change {
                Change::Put { path, kind, .. } => {
                    outln!("put {} {path}", kind_name(*kind));
                }
                Change::Delete { path } => outln!("delete {path}"),
            }
            if let Some(patch) = &f.patch {
                out!("{patch}");
            }
        }
        if let Some(next) = &result.next {
            outln!("more: --cursor {next}");
        }
        Ok(())
    })
}

/// One file's added and removed lines.
struct StatRow {
    path: String,
    added: u32,
    removed: u32,
    binary: bool,
}

/// Counts from the patches the diff already returned. A file with no patch
/// (binary, or truncated to nothing) is reported as binary.
fn stat_of(files: &[graft_proto::types::FileDiff]) -> Vec<StatRow> {
    files
        .iter()
        .take(STAT_FILES)
        .map(|f| {
            let path = match &f.change {
                Change::Put { path, .. } | Change::Delete { path } => path.to_string(),
            };
            let (added, removed) = f.patch.as_deref().map(patch_lines).unwrap_or((0, 0));
            StatRow {
                path,
                added,
                removed,
                binary: f.patch.is_none(),
            }
        })
        .collect()
}

/// `+` and `-` body lines of a unified patch, ignoring headers.
fn patch_lines(patch: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        if line.starts_with('+') {
            added = added.saturating_add(1);
        } else if line.starts_with('-') {
            removed = removed.saturating_add(1);
        }
    }
    (added, removed)
}

/// The human `--stat` table and its summary line.
fn print_stat(rows: &[StatRow]) -> Result<(), crate::error::Failure> {
    let width = rows.iter().map(|r| r.path.len()).max().unwrap_or(0);
    let mut files = 0u32;
    let mut added = 0u32;
    let mut removed = 0u32;
    for row in rows {
        files += 1;
        added = added.saturating_add(row.added);
        removed = removed.saturating_add(row.removed);
        if row.binary {
            outln!("{:<width$} | Bin", row.path, width = width);
        } else {
            outln!(
                "{:<width$} | {:>5} +{:<5} -{}",
                row.path,
                row.added + row.removed,
                row.added,
                row.removed,
                width = width
            );
        }
    }
    outln!(
        "{files} file{}, {added} insertion{}, {removed} deletion{}",
        plural(files),
        plural(added),
        plural(removed)
    );
    Ok(())
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl serde::Serialize for StatRow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("StatRow", 4)?;
        s.serialize_field("path", &self.path)?;
        s.serialize_field("added", &self.added)?;
        s.serialize_field("removed", &self.removed)?;
        s.serialize_field("binary", &self.binary)?;
        s.end()
    }
}

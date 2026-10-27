//! `graft diff`: two changesets (server-side, with patches), or the working
//! copy against its base (unified patches, capped).
//!
//! `--name-only` prints the changed paths without patches. `--stat` prints
//! per-file added and removed line counts, counted from the patch the command
//! already asks for (or, for the working copy, a local patch of the same
//! shape), capped the same way the patch is.

use std::fs::{self, File};
use std::io::Read;

use graft_client::fetch::load_blob;
use graft_client::workcopy::{FileStatus, WorkingCopy};
use graft_model::{Change, EntryKind};
use graft_proto::api::RepoApi;
use graft_proto::types::{Diff, Page, PatchOptions};
use imara_diff::{
    Algorithm, BasicLineDiffPrinter, Diff as LineDiff, InternedInput, UnifiedDiffConfig,
};
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

/// Total unified-patch bytes printed for one working-copy diff.
const WORKING_COPY_PATCH_CAP: usize = 256 * 1024;

fn working_copy_patch_cap() -> usize {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("GRAFT_DIFF_PATCH_CAP")
        && let Ok(n) = value.parse::<usize>()
        && n > 0
    {
        return n;
    }
    WORKING_COPY_PATCH_CAP
}

/// How many files `--stat` counts before reporting that more were omitted.
const STAT_FILES: usize = 100;

/// Diffs two changesets, or the working copy.
pub(crate) fn run(g: &Global, args: &DiffArgs) -> Result<(), Failure> {
    match (&args.from, &args.to) {
        (None, None) => working_copy_diff(g, args),
        (Some(from), Some(to)) => changeset_diff(g, args, from, to),
        _ => Err(Failure::usage(
            "give two changesets (FROM TO), or none to diff the working copy",
        )),
    }
}

/// The working copy against its base: unified patches, or paths only.
fn working_copy_diff(g: &Global, args: &DiffArgs) -> Result<(), Failure> {
    let wc = require_working_copy()?;
    let changes = wc.status()?;
    if g.json {
        let mut value = json!({
            "from": wc.state().base.to_string(),
            "to": null,
            "files": changes.iter().map(change_json).collect::<Vec<_>>(),
        });
        if args.stat {
            let rows = working_copy_stat(&wc, &changes)?;
            value["stat"] = json!(rows);
            value["stat_omitted"] = json!(changes.len().saturating_sub(rows.len()));
        }
        return print_json(&value);
    }
    if args.stat {
        let rows = working_copy_stat(&wc, &changes)?;
        print_stat(&rows)?;
        print_omitted(changes.len(), rows.len())?;
        return Ok(());
    }
    if args.name_only {
        for change in &changes {
            outln!("{}", change.path());
        }
        return Ok(());
    }
    print_working_copy_patches(&wc, &changes)
}

/// Unified patches of the working copy, stopped once the total exceeds the cap.
fn print_working_copy_patches(wc: &WorkingCopy, changes: &[FileStatus]) -> Result<(), Failure> {
    if changes.is_empty() {
        outln!("no changes against {}", short(wc.state().base));
        return Ok(());
    }
    let cache = wc.open_cache(64 * 1024 * 1024)?;
    let rt = tokio::runtime::Builder::new_current_thread().build()?;
    let mut used = 0usize;
    for (index, change) in changes.iter().enumerate() {
        if used >= working_copy_patch_cap() {
            let left = changes.len() - index;
            outln!(
                "(truncated: patch cap reached, {left} file{} omitted)",
                plural(left as u32)
            );
            break;
        }
        let patch = rt.block_on(working_copy_patch(wc, &cache, change))?;
        let room = working_copy_patch_cap() - used;
        let (shown, cut) = take_patch(&patch, room);
        used = used.saturating_add(shown.len());
        out!("{shown}");
        if cut {
            let left = changes.len() - index - 1;
            if left > 0 {
                outln!(
                    "(truncated: patch cap reached, {left} file{} omitted)",
                    plural(left as u32)
                );
            } else {
                outln!("(truncated: patch cap reached)");
            }
            break;
        }
    }
    Ok(())
}

/// One file's unified patch, or a short note when it is binary or too large.
async fn working_copy_patch(
    wc: &WorkingCopy,
    cache: &graft_store_fs::FsObjectStore,
    change: &FileStatus,
) -> Result<String, Failure> {
    let path = change.path();
    let header = format!("{:<9} {path}\n", word(change));
    if matches!(change, FileStatus::Added(_) | FileStatus::Modified(_)) {
        let metadata = fs::metadata(wc.root().join(path.as_str()))?;
        if metadata.len() > PATCH_MAX_BYTES {
            return Ok(format!("{header}(truncated: file exceeds the patch cap)\n"));
        }
    }
    let old = match change {
        FileStatus::Added(_) => None,
        FileStatus::Modified(_) | FileStatus::Deleted(_) => {
            let Some(entry) = wc.index().entries.get(path) else {
                return Err(Failure::new(
                    crate::Exit::Failure,
                    "state",
                    "working-copy index entry is missing",
                ));
            };
            if !matches!(entry.kind, EntryKind::File | EntryKind::Exec) {
                return Ok(format!("{header}Binary file {path}\n"));
            }
            if entry.stat.size > PATCH_MAX_BYTES {
                return Ok(format!("{header}(truncated: file exceeds the patch cap)\n"));
            }
            match load_blob(cache, entry.id.0).await {
                Ok(Some(bytes)) => Some(bytes),
                Ok(None) => {
                    return Err(Failure::new(
                        crate::Exit::Failure,
                        "store",
                        "base blob is missing from cache",
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    let new = match change {
        FileStatus::Deleted(_) => None,
        FileStatus::Added(_) | FileStatus::Modified(_) => {
            Some(read_capped(&wc.root().join(path.as_str()))?)
        }
    };
    if new
        .as_ref()
        .is_some_and(|bytes| bytes.len() > PATCH_MAX_BYTES as usize)
    {
        return Ok(format!("{header}(truncated: file exceeds the patch cap)\n"));
    }
    let old_text = match as_text(old.as_deref()) {
        Ok(v) => v,
        Err(()) => return Ok(format!("{header}Binary file {path}\n")),
    };
    let new_text = match as_text(new.as_deref()) {
        Ok(v) => v,
        Err(()) => return Ok(format!("{header}Binary file {path}\n")),
    };
    Ok(format!(
        "{header}{}",
        unified_patch(path.as_str(), old_text, new_text, PATCH_CONTEXT)
    ))
}

/// A unified diff of one file, with `---`/`+++` headers, matching server patches.
fn unified_patch(path: &str, old: Option<&str>, new: Option<&str>, context: u32) -> String {
    let mut out = String::new();
    out.push_str(&match old {
        Some(_) => format!("--- a/{path}\n"),
        None => "--- /dev/null\n".to_owned(),
    });
    out.push_str(&match new {
        Some(_) => format!("+++ b/{path}\n"),
        None => "+++ /dev/null\n".to_owned(),
    });
    let input = InternedInput::new(old.unwrap_or(""), new.unwrap_or(""));
    let mut diff = LineDiff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let mut config = UnifiedDiffConfig::default();
    config.context_len(context);
    let printer = BasicLineDiffPrinter(&input.interner);
    out.push_str(&diff.unified_diff(&printer, config, &input).to_string());
    out
}

/// The prefix of `patch` that fits in `room` bytes, cut at a line end.
fn take_patch(patch: &str, room: usize) -> (String, bool) {
    if patch.len() <= room {
        return (patch.to_owned(), false);
    }
    let mut end = room;
    while !patch.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = patch[..end].rfind('\n') {
        end = nl + 1;
    }
    (patch[..end].to_owned(), true)
}

/// Added and removed lines against the base blob in the local cache.
fn working_copy_stat(wc: &WorkingCopy, changes: &[FileStatus]) -> Result<Vec<StatRow>, Failure> {
    let cache = wc.open_cache(64 * 1024 * 1024)?;
    let rt = tokio::runtime::Builder::new_current_thread().build()?;
    changes
        .iter()
        .take(STAT_FILES)
        .map(|change| {
            let counts = rt.block_on(one_stat(wc, &cache, change))?;
            Ok(StatRow {
                path: change.path().to_string(),
                added: counts.0,
                removed: counts.1,
                binary: counts.2,
                truncated: counts.3,
            })
        })
        .collect()
}

async fn one_stat(
    wc: &WorkingCopy,
    cache: &graft_store_fs::FsObjectStore,
    change: &FileStatus,
) -> Result<(u32, u32, bool, bool), Failure> {
    let path = change.path();
    if matches!(change, FileStatus::Added(_) | FileStatus::Modified(_)) {
        let metadata = fs::metadata(wc.root().join(path.as_str()))?;
        if metadata.len() > PATCH_MAX_BYTES {
            return Ok((0, 0, false, true));
        }
    }
    let old = match change {
        FileStatus::Added(_) => None,
        FileStatus::Modified(_) | FileStatus::Deleted(_) => {
            let Some(entry) = wc.index().entries.get(path) else {
                return Err(Failure::new(
                    crate::Exit::Failure,
                    "state",
                    "working-copy index entry is missing",
                ));
            };
            if !matches!(entry.kind, EntryKind::File | EntryKind::Exec) {
                return Ok((0, 0, true, false));
            }
            if entry.stat.size > PATCH_MAX_BYTES {
                return Ok((0, 0, false, true));
            }
            match load_blob(cache, entry.id.0).await {
                Ok(Some(bytes)) => Some(bytes),
                Ok(None) => {
                    return Err(Failure::new(
                        crate::Exit::Failure,
                        "store",
                        "base blob is missing from cache",
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    let new = match change {
        FileStatus::Deleted(_) => None,
        FileStatus::Added(_) | FileStatus::Modified(_) => {
            let file = wc.root().join(path.as_str());
            Some(read_capped(&file)?)
        }
    };
    if new
        .as_ref()
        .is_some_and(|bytes| bytes.len() > PATCH_MAX_BYTES as usize)
    {
        return Ok((0, 0, false, true));
    }
    let (added, removed, binary) = lines_of(old.as_deref(), new.as_deref());
    Ok((added, removed, binary, false))
}

fn read_capped(path: &std::path::Path) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(PATCH_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
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

/// Line counts from the same histogram algorithm as server-side patches.
fn diff_lines(old: &str, new: &str) -> (u32, u32) {
    let input = InternedInput::new(old, new);
    let mut diff = LineDiff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let mut added = 0u32;
    let mut removed = 0u32;
    for hunk in diff.hunks() {
        added = added.saturating_add(u32::try_from(hunk.after.len()).unwrap_or(u32::MAX));
        removed = removed.saturating_add(u32::try_from(hunk.before.len()).unwrap_or(u32::MAX));
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
                let rows = stat_of(&result.files);
                value["stat"] = json!(rows);
                value["stat_omitted"] = json!(result.files.len().saturating_sub(rows.len()));
            }
            return print_json(&value);
        }
        if args.stat {
            let rows = stat_of(&result.files);
            print_stat(&rows)?;
            print_omitted(result.files.len(), rows.len())?;
            if let Some(next) = &result.next {
                outln!("more: --cursor {next}");
            }
            return Ok(());
        }
        if args.name_only {
            for f in &result.files {
                outln!("{}", f.change.path());
            }
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
    truncated: bool,
}

/// Counts from the patches the diff already returned. A file with no patch
/// is binary unless the server marked its patch truncated.
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
                binary: f.patch.is_none() && !f.truncated,
                truncated: f.truncated,
            }
        })
        .collect()
}

/// `+` and `-` body lines of a unified patch, ignoring headers.
fn patch_lines(patch: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
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
        if row.truncated {
            outln!("{:<width$} | ? (truncated)", row.path, width = width);
        } else if row.binary {
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
        "{files} file{}, {added} insertion{}, {removed} deletion{}{}",
        plural(files),
        plural(added),
        plural(removed),
        if rows.iter().any(|row| row.truncated) {
            " (partial)"
        } else {
            ""
        }
    );
    Ok(())
}

fn print_omitted(total: usize, shown: usize) -> Result<(), Failure> {
    if total > shown {
        outln!("more: {} files omitted", total - shown);
    }
    Ok(())
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl serde::Serialize for StatRow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("StatRow", 5)?;
        s.serialize_field("path", &self.path)?;
        s.serialize_field("added", &self.added)?;
        s.serialize_field("removed", &self.removed)?;
        s.serialize_field("binary", &self.binary)?;
        s.serialize_field("truncated", &self.truncated)?;
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use graft_model::{Change, EntryKind, ObjectId, RepoPath};
    use graft_proto::types::FileDiff;

    use super::{diff_lines, patch_lines, stat_of, take_patch, unified_patch};

    #[test]
    fn line_diff_counts_a_move() {
        assert_eq!(diff_lines("one\ntwo\nthree\n", "two\nthree\none\n"), (1, 1));
    }

    #[test]
    fn line_diff_aligns_repeated_lines() {
        assert_eq!(diff_lines("same\nold\nsame\n", "same\nnew\nsame\n"), (1, 1));
    }

    #[test]
    fn patch_lines_counts_removed_content_that_starts_with_header_marker() {
        let patch = "--- a/frontmatter.md\n+++ b/frontmatter.md\n@@ -1 +0,0 @@\n----\n";
        assert_eq!(patch_lines(patch), (0, 1));
    }

    #[test]
    fn take_patch_cuts_at_a_line_and_marks_truncation() {
        let patch = "modified  a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let (shown, cut) = take_patch(patch, 24);
        assert!(cut);
        assert!(shown.ends_with('\n'));
        assert!(shown.len() <= 24);
        let (whole, cut) = take_patch(patch, patch.len());
        assert!(!cut);
        assert_eq!(whole, patch);
    }

    #[test]
    fn unified_patch_has_a_hunk_for_an_edit() {
        let patch = unified_patch("README.md", Some("hello\n"), Some("edited\n"), 3);
        assert!(patch.contains("@@"));
        assert!(patch.contains("-hello"));
        assert!(patch.contains("+edited"));
    }

    #[test]
    fn stat_marks_a_cut_server_patch_truncated() {
        let rows = stat_of(&[FileDiff {
            change: Change::Put {
                path: RepoPath::new("large.txt").unwrap(),
                kind: EntryKind::File,
                id: ObjectId([1; 32]),
            },
            patch: Some("--- a/large.txt\n+++ b/large.txt\n@@ -1 +1 @@\n-old\n+new\n".into()),
            truncated: true,
        }]);
        assert!(rows[0].truncated);
        assert!(!rows[0].binary);
    }
}

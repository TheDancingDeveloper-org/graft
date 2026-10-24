//! `graft line`: creates, lists and merges lines (`docs/design.md` §4.6,
//! §8.9). `create` forks a line by compare-and-swap from absent, so it never
//! overwrites an existing line; it needs `repo:<r>:land:<name>` or
//! `repo:<r>:admin`.
//!
//! `merge <src> [--into <dst>]` is one `LineMerge` call (D29): the server
//! finds the merge base (merge points first, then the ancestry walk),
//! merges, and moves only `dst` by compare-and-swap, recording a merge
//! point. It exits as `land`: 0 merged or up to date, 3 conflict (the error
//! carries the tip, the base and the source ids; resolve in a working copy
//! on `dst` and `graft land --merged-from <src>@<id>`), 4 contended. When
//! the bounded merge point search was capped, the base came from the
//! ancestry walk and the output says `via ancestry`. `merges [<dst>]` lists
//! a line's merge points, newest first, paginated.

use graft_proto::api::RepoApi;
use graft_proto::types::{
    CreateLine, LineMerge, LineMergeOutcome, LineMerges, ListRefs, MergeBaseVia, MergeMode, Page,
    RefKind,
};
use serde_json::json;

use crate::cli::{DEFAULT_LINE, Global, LandMerge, LineAction, LineArgs};
use crate::conn::{call_context, connect, runtime, working_copy_info};
use crate::error::Failure;
use crate::render::{print_json, ref_view, short};

/// How a merge base was found, as the JSON and text name it.
pub(crate) fn via_name(via: MergeBaseVia) -> &'static str {
    match via {
        MergeBaseVia::MergePoint => "merge_point",
        MergeBaseVia::Fork => "fork",
        _ => "ancestry",
    }
}

/// Runs a line action.
pub(crate) fn run(g: &Global, args: &LineArgs) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let here = || {
            wc.as_ref()
                .map(|wc| wc.state().line.clone())
                .unwrap_or_else(|| DEFAULT_LINE.to_owned())
        };
        match &args.action {
            LineAction::Merge {
                src,
                into,
                at,
                message,
                merge,
            } => {
                let dst = into.clone().unwrap_or_else(here);
                let src_at = match at {
                    Some(rev) => Some(conn.resolve(&cx, rev).await?),
                    None => None,
                };
                let out = conn
                    .remote
                    .line_merge(
                        &cx,
                        LineMerge {
                            repo: conn.repo.clone(),
                            src: src.clone(),
                            dst: dst.clone(),
                            src_at,
                            merge: match merge {
                                LandMerge::Line => MergeMode::Line,
                                LandMerge::File => MergeMode::File,
                            },
                            deadline_ms: None,
                            message: message.clone(),
                        },
                    )
                    .await
                    .map_err(|e| {
                        let failure = Failure::from(e);
                        let note = match failure.kind() {
                            "conflict" => format!(
                                "nothing moved: resolve in a working copy on {dst}, then `graft land --merged-from {src}@<source>`"
                            ),
                            "contended" => "retryable: run `graft line merge` again".to_owned(),
                            _ => return failure,
                        };
                        failure.with_note(note)
                    })?;
                match out {
                    LineMergeOutcome::Merged {
                        landed,
                        line_version,
                        pos,
                        merged_paths,
                        base,
                        via,
                    } => {
                        if g.json {
                            return print_json(&json!({
                                "outcome": "merged",
                                "src": src,
                                "dst": dst,
                                "landed": landed.to_string(),
                                "line_version": line_version,
                                "pos": pos,
                                "base": base.to_string(),
                                "via": via_name(via),
                                "merged_paths": merged_paths
                                    .iter()
                                    .map(|p| p.as_str())
                                    .collect::<Vec<_>>(),
                            }));
                        }
                        outln!(
                            "merged {src} into {dst}: {} (base {} via {})",
                            short(landed),
                            short(base),
                            via_name(via)
                        );
                        if via == MergeBaseVia::Ancestry {
                            outln!(
                                "note: no merge point or fork row answered within the search budget; the base came from the ancestry walk"
                            );
                        }
                    }
                    LineMergeOutcome::UpToDate { tip } => {
                        if g.json {
                            return print_json(&json!({
                                "outcome": "up_to_date",
                                "src": src,
                                "dst": dst,
                                "landed": null,
                                "line_version": null,
                                "pos": null,
                                "base": null,
                                "via": null,
                                "merged_paths": [],
                                "tip": tip.to_string(),
                            }));
                        }
                        outln!("{dst} already has {src}: nothing to merge");
                    }
                    _ => {
                        return Err(Failure::new(
                            crate::Exit::Failure,
                            "client",
                            "unknown merge outcome",
                        ));
                    }
                }
                Ok(())
            }
            LineAction::Merges {
                dst,
                from,
                limit,
                cursor,
            } => {
                let dst = dst.clone().unwrap_or_else(here);
                let page = conn
                    .remote
                    .line_merges(
                        &cx,
                        LineMerges {
                            repo: conn.repo.clone(),
                            dst: dst.clone(),
                            src: from.clone(),
                            page: Page {
                                limit: *limit,
                                token: cursor.clone(),
                            },
                        },
                    )
                    .await?;
                if g.json {
                    return print_json(&json!({
                        "dst": dst,
                        "merges": page.merges.iter().map(|m| json!({
                            "src": m.src,
                            "src_changeset": m.src_changeset.to_string(),
                            "src_pos": m.src_pos,
                            "dst_changeset": m.dst_changeset.to_string(),
                            "dst_pos": m.dst_pos,
                        })).collect::<Vec<_>>(),
                        "next": page.next,
                    }));
                }
                for m in &page.merges {
                    outln!(
                        "{:>6}  {}  merged {}@{} (pos {})",
                        m.dst_pos,
                        short(m.dst_changeset),
                        m.src,
                        short(m.src_changeset),
                        m.src_pos
                    );
                }
                if let Some(next) = page.next {
                    outln!("more: --cursor {next}");
                }
                Ok(())
            }
            LineAction::Create { name, from } => {
                let from = conn.resolve(&cx, from).await?;
                let view = conn
                    .remote
                    .create_line(
                        &cx,
                        CreateLine {
                            repo: conn.repo.clone(),
                            name: name.clone(),
                            from,
                        },
                    )
                    .await?;
                if g.json {
                    return print_json(&ref_view(&view));
                }
                outln!("created line {} at {}", view.name, short(view.target));
                Ok(())
            }
            LineAction::List {
                prefix,
                limit,
                cursor,
            } => {
                let refs = conn
                    .remote
                    .list_refs(
                        &cx,
                        ListRefs {
                            repo: conn.repo.clone(),
                            prefix: prefix.clone(),
                            page: Page {
                                limit: *limit,
                                token: cursor.clone(),
                            },
                        },
                    )
                    .await?;
                // A full page may have more after it; the last ref name
                // resumes, whatever its kind.
                let next = (refs.len() as u32 >= *limit && *limit > 0)
                    .then(|| refs.last().map(|r| r.name.clone()))
                    .flatten();
                let lines: Vec<_> = refs.iter().filter(|r| r.kind == RefKind::Line).collect();
                if g.json {
                    return print_json(&json!({
                        "lines": lines.iter().map(|r| ref_view(r)).collect::<Vec<_>>(),
                        "next": next,
                    }));
                }
                for r in &lines {
                    outln!("{:<24} {}  v{}", r.name, short(r.target), r.version);
                }
                if let Some(next) = next {
                    outln!("more: --cursor {next}");
                }
                Ok(())
            }
        }
    })
}

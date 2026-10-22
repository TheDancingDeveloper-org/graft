//! `graft-mirror import <git-dir> --repo <name>` (`docs/design.md` §11.1):
//! create the repository, import with the fidelity check, then set the line
//! refs and create the tag refs, each by compare-and-swap.
//!
//! Re-running is safe: commits the mirror DB already holds are skipped, a
//! line that is already at its tip is left alone, and a tag that already
//! names its changeset is too.

use graft_git::import::{ImportConfig, ImportReport, import};
use graft_git::map::MirrorMap;
use graft_model::ChangesetId;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::RefKind;
use graft_proto::{ApiError, CallContext};

use crate::{MirrorError, current_ref, move_ref, tag_ref};

/// What an import did beyond `graft-git`'s report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    /// `graft-git`'s report: commits, blobs, the line and tag tips.
    pub report: ImportReport,
    /// Lines whose ref this run created or moved.
    pub lines_moved: Vec<String>,
    /// Tag refs this run created.
    pub tags_created: Vec<String>,
    /// Tags already in graft naming another changeset (tags are immutable),
    /// and the changeset git names.
    pub tags_conflicting: Vec<(String, ChangesetId)>,
}

/// Imports `config.git_dir` into `config.repo` and sets its refs.
pub async fn import_repo<A, M>(
    api: &A,
    cx: &CallContext,
    map: &M,
    config: &ImportConfig,
) -> Result<ImportOutcome, MirrorError>
where
    A: AdminApi + ObjectApi + RepoApi,
    M: MirrorMap,
{
    api.create_repo(cx, config.repo.clone()).await?;
    let report = import(api, cx, map, config).await?;
    let mut out = ImportOutcome::default();
    for (line, id) in &report.lines {
        let current = current_ref(api, cx, &config.repo, line).await?;
        if current != Some(*id) {
            move_ref(api, cx, &config.repo, line, current, *id, RefKind::Line).await?;
            out.lines_moved.push(line.clone());
        }
    }
    for (tag, id) in &report.tags {
        let name = tag_ref(tag);
        match current_ref(api, cx, &config.repo, &name).await? {
            Some(existing) if existing == *id => {}
            Some(_) => out.tags_conflicting.push((tag.clone(), *id)),
            None => match move_ref(api, cx, &config.repo, &name, None, *id, RefKind::Tag).await {
                Ok(()) => out.tags_created.push(tag.clone()),
                // Created concurrently: report it as conflicting rather than
                // guess which is right.
                Err(ApiError::RefConflict { .. }) => out.tags_conflicting.push((tag.clone(), *id)),
                Err(e) => return Err(e.into()),
            },
        }
    }
    out.report = report;
    Ok(out)
}

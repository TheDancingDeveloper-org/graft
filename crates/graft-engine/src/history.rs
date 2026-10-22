//! `Log`, `IsAncestor`, `MergeBase` and `Blame` over `graft-index`
//! (`docs/design.md` §7.5, §7.7).
//!
//! The index does the walking with bounded reads; this module checks scopes,
//! resolves the repository and the named changesets, and turns ids into
//! summaries. `Log` pages by token: the next page starts at the first parent
//! of the last result (its text form is the token).

use std::collections::HashMap;

use graft_index::LogQuery;
use graft_model::{Changeset, ChangesetId};
use graft_proto::types::{Blame, BlameRange, BlameResult, ChangesetSummary, History, Log, Pair};
use graft_proto::{ApiError, CallContext};
use graft_store::{MetaStore, ObjectStore, RepoId, StoreError};

use crate::access::{Need, require};
use crate::{Engine, errors};

/// The first line of a message.
fn subject(message: &str) -> String {
    message.lines().next().unwrap_or("").to_owned()
}

fn summary(id: ChangesetId, cs: &Changeset) -> ChangesetSummary {
    ChangesetSummary {
        id,
        author: cs.author.clone(),
        authored_at: cs.authored_at,
        subject: subject(&cs.message),
        line_pos: None,
    }
}

/// An index failure. `Invalid` is the index saying the request names
/// nothing there (a path with no file); the rest are backend failures.
fn index_error(e: StoreError, what: impl FnOnce() -> String) -> ApiError {
    match e {
        StoreError::Invalid(_) => errors::not_found(what()),
        e => errors::store(e),
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_log(&self, cx: &CallContext, req: Log) -> Result<History, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        let from = match &req.page.token {
            Some(t) => ChangesetId::parse(t).map_err(|_| errors::invalid(None, "page-token"))?,
            None => req.from,
        };
        self.changeset(repo, from).await?;
        if let Some(until) = req.until {
            self.changeset(repo, until).await?;
        }
        let limit = self.page_size(req.page.limit);
        let ids = self
            .index
            .log(
                repo,
                LogQuery {
                    from,
                    until: req.until,
                    paths: req.paths,
                    limit,
                    after_pos: None,
                },
            )
            .await
            .map_err(|e| index_error(e, || format!("changeset {from}")))?;
        let mut changesets = Vec::with_capacity(ids.len());
        let mut next = None;
        for (i, id) in ids.iter().enumerate() {
            let (cs, _) = self.changeset(repo, *id).await?;
            if i + 1 == ids.len() && ids.len() == limit as usize {
                next = cs.first_parent().map(|p| p.to_string());
            }
            changesets.push(summary(*id, &cs));
        }
        Ok(History { changesets, next })
    }

    pub(crate) async fn do_is_ancestor(
        &self,
        cx: &CallContext,
        req: Pair,
    ) -> Result<bool, ApiError> {
        let repo = self.pair(cx, &req).await?;
        self.index
            .is_ancestor(repo, req.a, req.b)
            .await
            .map_err(|e| index_error(e, || "changeset".to_owned()))
    }

    pub(crate) async fn do_merge_base(
        &self,
        cx: &CallContext,
        req: Pair,
    ) -> Result<Option<ChangesetId>, ApiError> {
        let repo = self.pair(cx, &req).await?;
        self.index
            .merge_base(repo, req.a, req.b)
            .await
            .map_err(|e| index_error(e, || "changeset".to_owned()))
    }

    /// Checks the scope and that both changesets are stored.
    async fn pair(&self, cx: &CallContext, req: &Pair) -> Result<RepoId, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        self.changeset(repo, req.a).await?;
        self.changeset(repo, req.b).await?;
        Ok(repo)
    }

    pub(crate) async fn do_blame(
        &self,
        cx: &CallContext,
        req: Blame,
    ) -> Result<BlameResult, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        self.changeset(repo, req.at).await?;
        let runs = self
            .index
            .blame(repo, req.at, &req.path)
            .await
            .map_err(|e| index_error(e, || format!("file {}", req.path)))?;
        let (lo, hi) = req.lines.unwrap_or((0, u32::MAX));
        let mut origins: HashMap<ChangesetId, ChangesetSummary> = HashMap::new();
        let mut ranges = Vec::new();
        for run in runs {
            let (start, end) = (run.start.max(lo), run.end.min(hi));
            if start >= end {
                continue;
            }
            let origin = match origins.get(&run.origin) {
                Some(s) => s.clone(),
                None => {
                    let (cs, _) = self.changeset(repo, run.origin).await?;
                    let s = summary(run.origin, &cs);
                    origins.insert(run.origin, s.clone());
                    s
                }
            };
            ranges.push(BlameRange { start, end, origin });
        }
        Ok(BlameResult { ranges })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_is_the_first_line() {
        assert_eq!(subject("fix: x\n\nbody"), "fix: x");
        assert_eq!(subject(""), "");
    }
}

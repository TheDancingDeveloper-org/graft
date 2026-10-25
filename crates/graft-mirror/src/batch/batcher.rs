//! Follow, sync, export and the GitHub PR for one integration line.

use std::collections::{BTreeMap, HashMap, HashSet};

use graft_git::GitSha;
use graft_git::export::commit_object;
use graft_git::exporter::{Exporter, MERGED_FROM_KEY, SESSION_KEY, WORK_ITEM_KEY};
use graft_git::map::{ImportedCommit, MirrorMap};
use graft_model::{Author, AuthorKind, Change, ChangesetId, Metadata, Timestamp};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::{
    ChangesetView, CreateChangeset, Diff, GetChangesets, Log, MergeMode, Page, Pair, Rebase,
    RefKind, Revert, Watch,
};
use graft_proto::{ApiError, CallContext};

use crate::credentials::SecretToken;
use crate::db::MirrorDb;
use crate::follow::{Follower, LineOutcome};
use crate::git::{Mirror, PushOutcome};
use crate::github::{Forge, Pull};
use crate::text::{clean_message, clean_text};
use crate::{MirrorError, current_ref, move_ref};

use super::render::{BatchHeader, keep_with_reverts, land_commit, parse_commit, render};
use super::types::{
    Attested, BATCH, BRANCH_PREFIX, BatchCommit, BatchConfig, BatchLine, BatchOutcome, BatchRecord,
    BatchRun, BatchState, DEFAULT_DROP_REASON, Dropped, FETCH, FIELD_CHARS, LandLine,
    MAX_ATTEST_PAGES, MAX_COMMITS, MAX_DROPS_LISTED, MAX_LANDS, MESSAGE_CHARS, PAGE, REVERTS_KEY,
    SYNC_KEY, Stamp, TITLE_CHARS, config_err, git_time, is_sync, land_ident, short, system_ms,
};

/// Batches one integration line into squashed GitHub PRs.
pub struct Batcher<'a, A, F> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    forge: &'a F,
    config: BatchConfig,
    clock: fn() -> i64,
    calls: u64,
}

impl<'a, A, F> Batcher<'a, A, F>
where
    A: AdminApi + ObjectApi + RepoApi,
    F: Forge,
{
    /// A batcher for `config.line`. Claims the line for batch mode: a line
    /// another authority claims is refused, and so is a line that is its own
    /// base.
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        forge: &'a F,
        config: BatchConfig,
    ) -> Result<Self, MirrorError> {
        if config.line == config.base.line {
            return Err(config_err(format!(
                "line {} is the base line; batch an integration line forked from it",
                config.line
            )));
        }
        let holder = db.claim_authority(&config.line, BATCH)?;
        if holder != BATCH {
            return Err(config_err(format!(
                "line {} is claimed by {holder}; it cannot also be batched",
                config.line
            )));
        }
        Ok(Batcher {
            api,
            cx,
            db,
            mirror,
            forge,
            config,
            clock: system_ms,
            calls: 0,
        })
    }

    /// Reads time from `clock` (milliseconds since the epoch).
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// Follows the base, settles or advances the last batch, syncs the line
    /// and, when nothing is open, exports a new batch and opens its PR.
    pub async fn run(&mut self) -> Result<BatchRun, MirrorError> {
        let mut run = BatchRun {
            settled: None,
            synced: None,
            dropped: Vec::new(),
            outcome: BatchOutcome::NothingToExport,
        };
        if let Some(stopped) = self.follow().await? {
            run.outcome = BatchOutcome::BaseStopped(stopped);
            return Ok(run);
        }
        let mut open = None;
        if let Some(mut rec) = self.db.last_batch(&self.config.line)?
            && !rec.state.is_terminal()
        {
            self.advance(&mut rec).await?;
            if rec.state.is_terminal() {
                run.settled = Some(rec);
            } else {
                open = Some(rec);
            }
        }
        let mut conflict = None;
        if !self.config.exclude.is_empty()
            && let Err(c) = self.drop_lands(&mut run.dropped).await?
        {
            conflict = Some(c);
        }
        // Checked on every run, not only after this run's drops: a run that
        // died or conflicted part way may have left reverts on the line that
        // the open batch lacks.
        if let Some(mut rec) = open {
            let stale = self.stale_drops(&rec).await?;
            if stale.is_empty() || !self.supersede(&mut rec, &stale).await? {
                run.outcome = conflict.unwrap_or(BatchOutcome::Waiting(rec));
                return Ok(run);
            }
            run.settled = Some(rec);
        }
        if let Some(c) = conflict {
            run.outcome = c;
            return Ok(run);
        }
        let base_tip = self.tip(&self.config.base.line.clone()).await?;
        let mut tip = self.tip(&self.config.line.clone()).await?;
        let mut state = self.line_state(tip, base_tip).await?;
        if state.synced != base_tip {
            match self.sync(&mut state, tip, base_tip).await? {
                Ok(moved) => {
                    if let Some(m) = moved {
                        tip = m;
                        run.synced = Some(m);
                    }
                }
                Err(conflict) => {
                    run.outcome = conflict;
                    return Ok(run);
                }
            }
        }
        match self.plan(&state, tip, base_tip).await? {
            Ok(Some(mut rec)) => {
                self.advance(&mut rec).await?;
                run.outcome = BatchOutcome::Opened(rec);
            }
            Ok(None) => {}
            Err(outcome) => run.outcome = outcome,
        }
        Ok(run)
    }

    /// Advances the line's last batch without making a new one: push and
    /// open it if that was interrupted, then read its PR. Returns it.
    pub async fn track(&mut self) -> Result<Option<BatchRecord>, MirrorError> {
        let Some(mut rec) = self.db.last_batch(&self.config.line)? else {
            return Ok(None);
        };
        if !rec.state.is_terminal() {
            self.advance(&mut rec).await?;
        }
        Ok(Some(rec))
    }

    fn call(&mut self, what: &str) -> CallContext {
        self.calls += 1;
        let mut cx = self.cx.clone();
        cx.request_id = format!("batch-{}-{}-{what}", (self.clock)(), self.calls);
        cx
    }

    /// One follow poll of the base. `Some` (described) when the base line is
    /// stopped.
    async fn follow(&mut self) -> Result<Option<String>, MirrorError> {
        self.check_mapped().await?;
        let mut config = self.config.follow.clone();
        config.repo = self.config.repo.clone();
        config.lines = vec![self.config.base.clone()];
        let mut follower = Follower::new(
            self.api,
            self.cx.clone(),
            self.db,
            self.mirror.clone(),
            config,
        )
        .with_clock(self.clock);
        let outcomes = follower.poll_once().await?;
        match outcomes.into_iter().next().map(|(_, o)| o) {
            Some(LineOutcome::UpToDate | LineOutcome::Imported { .. }) => Ok(None),
            Some(LineOutcome::Diverged { reason, upstream }) => Ok(Some(format!(
                "follow stopped line {} ({}) on upstream {upstream}",
                self.config.base.line,
                reason.as_str()
            ))),
            Some(LineOutcome::MissingUpstream) => Err(config_err(format!(
                "upstream has no branch {}",
                self.config.base.branch
            ))),
            Some(LineOutcome::Failed(what)) => Err(config_err(format!(
                "following {} failed: {what}",
                self.config.base.branch
            ))),
            None => Err(config_err("follow reported no line")),
        }
    }

    /// Refuses a DB that cannot map the base: graft's base line has a tip
    /// the DB has no commit for, and the DB has never followed the line or
    /// followed it to a changeset this repo does not hold. Follow alone
    /// would call that `graft-moved`, which hides an empty or wrong DB.
    async fn check_mapped(&mut self) -> Result<(), MirrorError> {
        let base = self.config.base.line.clone();
        let cx = self.call("mapped");
        let repo = self.config.repo.clone();
        let Some(tip) = current_ref(self.api, &cx, &repo, &base).await? else {
            return Ok(());
        };
        if self.db.sha_of(tip)?.is_some() {
            return Ok(());
        }
        let followed = match self.db.line(&base)?.upstream {
            None => None,
            Some(up) => self.db.changeset_of(up)?,
        };
        if let Some(c) = followed
            && !self.views(&[c]).await?.is_empty()
        {
            // The DB belongs here and graft really moved: follow says so.
            return Ok(());
        }
        Err(MirrorError::Unmapped {
            db: self.db.path().to_owned(),
            line: base,
            empty: self.db.commit_count()? == 0,
        })
    }

    async fn tip(&mut self, line: &str) -> Result<ChangesetId, MirrorError> {
        let cx = self.call("tip");
        let repo = self.config.repo.clone();
        current_ref(self.api, &cx, &repo, line)
            .await?
            .ok_or_else(|| config_err(format!("line {line} has no tip")))
    }

    /// The line's record, started at its fork point from the base on the
    /// first run.
    async fn line_state(
        &mut self,
        tip: ChangesetId,
        base_tip: ChangesetId,
    ) -> Result<BatchLine, MirrorError> {
        if let Some(state) = self.db.batch_line(&self.config.line)? {
            return Ok(state);
        }
        let cx = self.call("merge-base");
        let fork = self
            .api
            .merge_base(
                &cx,
                Pair {
                    repo: self.config.repo.clone(),
                    a: tip,
                    b: base_tip,
                },
            )
            .await?
            .ok_or_else(|| {
                config_err(format!(
                    "line {} does not fork from {}",
                    self.config.line, self.config.base.line
                ))
            })?;
        let state = BatchLine {
            synced: fork,
            sync_from: fork,
            start: fork,
            start_base: Some(fork),
            sync_batch: None,
        };
        self.db.set_batch_line(&self.config.line, &state)?;
        tracing::info!(
            repo = %self.config.repo, line = %self.config.line, fork = %fork,
            "mirror batch mode started"
        );
        Ok(state)
    }

    async fn view(&mut self, id: ChangesetId) -> Result<ChangesetView, MirrorError> {
        let mut found = self.views(&[id]).await?;
        found
            .remove(&id)
            .ok_or_else(|| config_err(format!("changeset {id} is not in graft")))
    }

    /// The views of `ids`, batched. Missing ids are left out.
    async fn views(
        &mut self,
        ids: &[ChangesetId],
    ) -> Result<HashMap<ChangesetId, ChangesetView>, MirrorError> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(FETCH) {
            let cx = self.call("get");
            let got = self
                .api
                .get_changesets(
                    &cx,
                    GetChangesets {
                        repo: self.config.repo.clone(),
                        ids: chunk.to_vec(),
                    },
                )
                .await?;
            for v in got.found {
                out.insert(v.changeset.id(), v);
            }
        }
        Ok(out)
    }

    /// Every change from snapshot `from` to snapshot `to`, page by page.
    async fn diff(
        &mut self,
        from: ChangesetId,
        to: ChangesetId,
    ) -> Result<Vec<Change>, MirrorError> {
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let cx = self.call("diff");
            let page = self
                .api
                .diff(
                    &cx,
                    Diff {
                        repo: self.config.repo.clone(),
                        from,
                        to,
                        paths: Vec::new(),
                        patch: None,
                        page: Page {
                            limit: PAGE,
                            token: token.take(),
                        },
                    },
                )
                .await?;
            out.extend(page.files.into_iter().map(|f| f.change));
            match page.next {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }

    /// A sync changeset on `parents` with `changes`, by the mirror.
    async fn create(
        &mut self,
        parents: Vec<ChangesetId>,
        changes: Vec<Change>,
        message: String,
        absorbed: ChangesetId,
    ) -> Result<ChangesetId, MirrorError> {
        let mut metadata = Metadata::new();
        metadata.insert(SYNC_KEY.to_owned(), absorbed.to_string());
        let at = Timestamp {
            ms: (self.clock)(),
            offset_min: 0,
        };
        Ok(self
            .create_with(parents, changes, message, metadata, at)
            .await?)
    }

    /// A changeset on `parents` with `changes`, by the mirror.
    async fn create_with(
        &mut self,
        parents: Vec<ChangesetId>,
        changes: Vec<Change>,
        message: String,
        metadata: Metadata,
        authored_at: Timestamp,
    ) -> Result<ChangesetId, ApiError> {
        let cx = self.call("create");
        let created = self
            .api
            .create_changeset(
                &cx,
                CreateChangeset {
                    repo: self.config.repo.clone(),
                    parents,
                    changes,
                    author: Author {
                        name: self.config.committer.name.clone(),
                        email: self.config.committer.email.clone(),
                        kind: AuthorKind::Bot,
                    },
                    authored_at,
                    message,
                    metadata,
                    inline_blobs: Vec::new(),
                    workspace: None,
                },
            )
            .await?;
        Ok(created.id)
    }

    /// Carries the integration line onto `base_tip`. `Ok(Ok(Some(m)))` when
    /// the line moved to the sync changeset `m`, `Ok(Ok(None))` when only
    /// the record changed (the line already holds the base's snapshot), and
    /// `Ok(Err(_))` on a conflict, with nothing moved.
    async fn sync(
        &mut self,
        state: &mut BatchLine,
        tip: ChangesetId,
        base_tip: ChangesetId,
    ) -> Result<Result<Option<ChangesetId>, BatchOutcome>, MirrorError> {
        // The line's own change, as one changeset on the snapshot it holds
        // of the base, rebased onto the base tip.
        let from = match state.sync_batch {
            Some(number) => {
                let rec = self
                    .db
                    .batch(number)?
                    .ok_or_else(|| config_err(format!("batch {number} is not recorded")))?;
                self.merged_on_synced(&rec, state).await?
            }
            None => state.sync_from,
        };
        let net = self.diff(from, tip).await?;
        let target = if net.is_empty() {
            base_tip
        } else {
            let message = format!(
                "graft: net change of line {} since {from}",
                self.config.line
            );
            let squashed = self.create(vec![from], net, message, base_tip).await?;
            let cx = self.call("rebase");
            match self
                .api
                .rebase(
                    &cx,
                    Rebase {
                        repo: self.config.repo.clone(),
                        changeset: squashed,
                        onto: base_tip,
                        merge: MergeMode::Line,
                        dry_run: false,
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(ApiError::Conflict { paths, onto }) => {
                    let paths: Vec<String> = paths
                        .into_iter()
                        .map(|p| clean_text(&p.path, 300, false))
                        .collect();
                    tracing::warn!(
                        repo = %self.config.repo, line = %self.config.line, onto = %onto,
                        conflicts = paths.len(),
                        "mirror batch: the line does not rebase onto the moved base; nothing moved"
                    );
                    return Ok(Err(BatchOutcome::Conflict { onto, paths }));
                }
                Err(e) => return Err(e.into()),
            }
        };
        let changes = self.diff(tip, target).await?;
        let moved = if changes.is_empty() {
            None
        } else {
            let message = format!(
                "graft: sync line {} onto {} {base_tip}",
                self.config.line, self.config.base.line
            );
            let merge = self
                .create(vec![tip, base_tip], changes, message, base_tip)
                .await?;
            let cx = self.call("update-ref");
            let repo = self.config.repo.clone();
            let line = self.config.line.clone();
            match move_ref(self.api, &cx, &repo, &line, Some(tip), merge, RefKind::Line).await {
                Ok(()) => {}
                Err(ApiError::RefConflict { .. }) => {
                    return Err(config_err(format!(
                        "line {line} moved during the sync; run pr-batch again"
                    )));
                }
                Err(e) => return Err(e.into()),
            }
            Some(merge)
        };
        state.synced = base_tip;
        state.sync_from = base_tip;
        state.sync_batch = None;
        self.db.set_batch_line(&self.config.line, state)?;
        tracing::info!(
            repo = %self.config.repo, line = %self.config.line, onto = %base_tip,
            moved = ?moved, "mirror batch: line synced onto the base"
        );
        Ok(Ok(moved))
    }

    /// The lands on the integration line after `start`, newest first, at
    /// most [`MAX_LANDS`], with whether the count stopped at the cap. Sync
    /// changesets are left out.
    async fn lands(
        &mut self,
        tip: ChangesetId,
        start: ChangesetId,
    ) -> Result<(Vec<ChangesetId>, bool), MirrorError> {
        let mut ids = Vec::new();
        let mut token = None;
        let capped = loop {
            let cx = self.call("log");
            let page = self
                .api
                .log(
                    &cx,
                    Log {
                        repo: self.config.repo.clone(),
                        from: tip,
                        until: Some(start),
                        paths: Vec::new(),
                        page: Page {
                            limit: PAGE,
                            token: token.take(),
                        },
                    },
                )
                .await?;
            ids.extend(page.changesets.iter().map(|c| c.id));
            if ids.len() > MAX_LANDS {
                ids.truncate(MAX_LANDS);
                break true;
            }
            match page.next {
                Some(t) => token = Some(t),
                None => break false,
            }
        };
        Ok((ids, capped))
    }

    /// Each land's latest result per check, from the journal's `Attested`
    /// entries at or after `from_lsn`. A land rebased at landing takes its
    /// draft's attestations. `true` when the read stopped at its cap.
    async fn attestations(
        &mut self,
        from_lsn: u64,
        wanted: &HashMap<ChangesetId, ChangesetId>,
    ) -> Result<(HashMap<ChangesetId, BTreeMap<String, bool>>, bool), MirrorError> {
        let mut out: HashMap<ChangesetId, BTreeMap<String, bool>> = HashMap::new();
        let mut lsn = from_lsn;
        for _ in 0..MAX_ATTEST_PAGES {
            let cx = self.call("attested");
            let page = self
                .api
                .watch(
                    &cx,
                    Watch {
                        repo: self.config.repo.clone(),
                        from_lsn: lsn,
                        kinds: vec!["Attested".to_owned()],
                        limit: PAGE,
                        wait_ms: 0,
                    },
                )
                .await?;
            for e in &page.entries {
                let Ok(a) = serde_json::from_str::<Attested>(&e.body) else {
                    continue;
                };
                let Ok(id) = ChangesetId::parse(&a.changeset) else {
                    continue;
                };
                if let Some(land) = wanted.get(&id) {
                    out.entry(*land)
                        .or_default()
                        .insert(clean_text(&a.check, FIELD_CHARS, false), a.passed);
                }
            }
            if page.next_lsn <= lsn {
                return Ok((out, false));
            }
            lsn = page.next_lsn;
        }
        Ok((out, true))
    }

    /// The lands among `ids` (newest first), sync changesets left out, with
    /// each one's latest check results; `true` when the attestation read
    /// stopped at its cap.
    async fn describe(
        &mut self,
        ids: &[ChangesetId],
        views: &HashMap<ChangesetId, ChangesetView>,
    ) -> Result<(Vec<LandLine>, bool), MirrorError> {
        let mut lands = Vec::new();
        // Each attested id (a land, or the draft it was rebased from) and
        // the land it counts for.
        let mut wanted = HashMap::new();
        let mut from_lsn = u64::MAX;
        for id in ids {
            let Some(v) = views.get(id) else { continue };
            let cs = &v.changeset;
            if is_sync(&cs.metadata) {
                continue;
            }
            from_lsn = from_lsn.min(v.lsn);
            wanted.insert(*id, *id);
            if let Some(draft) = cs
                .metadata
                .get("graft.rebased-from")
                .and_then(|d| ChangesetId::parse(d).ok())
            {
                wanted.insert(draft, *id);
            }
            let meta = |key: &str| {
                cs.metadata
                    .get(key)
                    .map(|v| clean_text(v, FIELD_CHARS, false))
            };
            lands.push(LandLine {
                id: *id,
                author: clean_text(&cs.author.name, FIELD_CHARS, false),
                session: meta(SESSION_KEY),
                work_item: meta(WORK_ITEM_KEY),
                subject: clean_text(
                    cs.message.lines().next().unwrap_or("").trim(),
                    FIELD_CHARS,
                    false,
                ),
                checks: BTreeMap::new(),
                authored_ms: cs.authored_at.ms,
                reverts: cs
                    .metadata
                    .get(REVERTS_KEY)
                    .and_then(|r| ChangesetId::parse(r).ok()),
                parent: cs.first_parent(),
                ident: land_ident(&cs.author, &self.config.committer),
                authored: cs.authored_at,
                message: clean_message(&cs.message, MESSAGE_CHARS, true),
                trailer_session: cs
                    .metadata
                    .get(SESSION_KEY)
                    .map(|v| clean_message(v, FIELD_CHARS, false)),
                trailer_work_item: cs
                    .metadata
                    .get(WORK_ITEM_KEY)
                    .map(|v| clean_message(v, FIELD_CHARS, false)),
                trailer_merged_from: cs
                    .metadata
                    .get(MERGED_FROM_KEY)
                    .map(|v| clean_message(v, FIELD_CHARS, false)),
            });
        }
        if lands.is_empty() {
            return Ok((lands, false));
        }
        // Drafts rebased at landing were attested before the land existed:
        // read from the oldest of them too.
        let drafts: Vec<ChangesetId> = wanted
            .iter()
            .filter(|(d, l)| d != l)
            .map(|(d, _)| *d)
            .collect();
        for v in self.views(&drafts).await?.values() {
            from_lsn = from_lsn.min(v.lsn);
        }
        let (checks, truncated) = self.attestations(from_lsn, &wanted).await?;
        for land in &mut lands {
            if let Some(c) = checks.get(&land.id) {
                land.checks = c.clone();
            }
        }
        Ok((lands, truncated))
    }

    /// Reverts each land the operator named on the
    /// integration line, newest first, each a compare-and-swap move of the
    /// line, and records why. A land already reverted on the line is
    /// skipped. `Ok(Err(_))` on a revert that conflicts with later lands;
    /// the reverts before it stay.
    async fn drop_lands(
        &mut self,
        dropped: &mut Vec<(ChangesetId, ChangesetId)>,
    ) -> Result<Result<(), BatchOutcome>, MirrorError> {
        let base_tip = self.tip(&self.config.base.line.clone()).await?;
        let mut tip = self.tip(&self.config.line.clone()).await?;
        let state = self.line_state(tip, base_tip).await?;
        let (ids, _) = self.lands(tip, state.start).await?;
        let views = self.views(&ids).await?;
        // The lands (not sync changesets) and the ones already reverted.
        let mut lands = HashSet::new();
        let mut reverted = HashSet::new();
        for v in ids.iter().filter_map(|id| views.get(id)) {
            let meta = &v.changeset.metadata;
            if is_sync(meta) {
                continue;
            }
            lands.insert(v.changeset.id());
            if let Some(r) = meta
                .get(REVERTS_KEY)
                .and_then(|r| ChangesetId::parse(r).ok())
            {
                reverted.insert(r);
            }
        }
        let named = clean_text(
            self.config
                .drop_reason
                .as_deref()
                .unwrap_or(DEFAULT_DROP_REASON),
            FIELD_CHARS,
            false,
        );
        let mut targets: Vec<(ChangesetId, String)> = Vec::new();
        for id in self.config.exclude.clone() {
            if !lands.contains(&id) {
                return Err(config_err(format!(
                    "{id} is not a land in line {}'s next batch",
                    self.config.line
                )));
            }
            if !reverted.contains(&id) && !targets.iter().any(|(t, _)| *t == id) {
                targets.push((id, named.clone()));
            }
        }
        // Newest first, as the land list is.
        let order: HashMap<ChangesetId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        targets.sort_by_key(|(id, _)| order.get(id).copied().unwrap_or(usize::MAX));
        for (land, reason) in targets {
            let cx = self.call("revert");
            let revert = match self
                .api
                .revert(
                    &cx,
                    Revert {
                        repo: self.config.repo.clone(),
                        changeset: land,
                        onto: tip,
                        workspace: None,
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(ApiError::Conflict { paths, onto }) => {
                    let paths: Vec<String> = paths
                        .into_iter()
                        .map(|p| clean_text(&p.path, 300, false))
                        .collect();
                    tracing::warn!(
                        repo = %self.config.repo, line = %self.config.line, land = %land,
                        conflicts = paths.len(),
                        "mirror batch: the land does not revert cleanly on the line; not dropped"
                    );
                    return Ok(Err(BatchOutcome::Conflict { onto, paths }));
                }
                Err(e) => return Err(e.into()),
            };
            let repo = self.config.repo.clone();
            let line = self.config.line.clone();
            // The reason first: a run that dies after the move still names
            // it. A row whose revert never reached the line is overwritten
            // by the retry's revert.
            self.db.put_batch_drop(&line, land, revert, &reason)?;
            let cx = self.call("update-ref");
            match move_ref(
                self.api,
                &cx,
                &repo,
                &line,
                Some(tip),
                revert,
                RefKind::Line,
            )
            .await
            {
                Ok(()) => {}
                Err(ApiError::RefConflict { .. }) => {
                    return Err(config_err(format!(
                        "line {line} moved while dropping {land}; run pr-batch again"
                    )));
                }
                Err(e) => return Err(e.into()),
            }
            tracing::info!(
                repo = %repo, line = %line, land = %land, revert = %revert,
                "mirror batch: land dropped (reverted on the line)"
            );
            dropped.push((land, revert));
            tip = revert;
        }
        Ok(Ok(()))
    }

    /// The lands of open batch `rec` reverted on the line after its tip,
    /// each with its revert: what makes the batch stale. Reads the lands
    /// since the batch's tip, and the batch's own only when one of those is
    /// a revert.
    async fn stale_drops(
        &mut self,
        rec: &BatchRecord,
    ) -> Result<Vec<(ChangesetId, ChangesetId)>, MirrorError> {
        let tip = self.tip(&self.config.line.clone()).await?;
        if tip == rec.tip {
            return Ok(Vec::new());
        }
        let (newer, _) = self.lands(tip, rec.tip).await?;
        let views = self.views(&newer).await?;
        let mut reverts: Vec<(ChangesetId, ChangesetId)> = newer
            .iter()
            .filter_map(|id| {
                let target = views.get(id)?.changeset.metadata.get(REVERTS_KEY)?;
                Some((ChangesetId::parse(target).ok()?, *id))
            })
            .collect();
        if reverts.is_empty() {
            return Ok(reverts);
        }
        let (held, _) = self.lands(rec.tip, rec.start).await?;
        let held: HashSet<ChangesetId> = held.into_iter().collect();
        reverts.retain(|(land, _)| held.contains(land));
        reverts.reverse();
        Ok(reverts)
    }

    /// Closes an open batch whose lands changed under it (a land dropped),
    /// with a comment saying so, and rejects it: the next batch is cut from
    /// the same start. `false`, with nothing closed, when GitHub has it in
    /// the merge queue or set to auto-merge, or it merged meanwhile: the
    /// reverts then reach GitHub in the next batch.
    async fn supersede(
        &mut self,
        rec: &mut BatchRecord,
        dropped: &[(ChangesetId, ChangesetId)],
    ) -> Result<bool, MirrorError> {
        let Some(number) = rec.pr_number else {
            return Err(config_err("an open batch has no PR"));
        };
        let pull = self.forge.get_pull(number).await?;
        let busy = if pull.merged || !pull.open {
            Some("merged or closed on GitHub since this run read it")
        } else if self.forge.merging(&pull.node_id).await? {
            Some("in GitHub's merge queue or set to auto-merge")
        } else {
            None
        };
        if let Some(why) = busy {
            rec.detail =
                format!("not superseded: {why}; the dropped lands' reverts go in the next batch");
            self.db.put_batch(rec)?;
            tracing::warn!(
                line = %rec.line, batch = rec.number, pr = number,
                "mirror batch: a stale batch is merging; left as it is"
            );
            return Ok(false);
        }
        let names: Vec<String> = dropped
            .iter()
            .take(MAX_DROPS_LISTED)
            .map(|(l, _)| format!("`{}`", short(*l)))
            .collect();
        let detail = format!(
            "superseded: land{} {}{} dropped from line {} (reverted); recut as a new batch",
            if dropped.len() == 1 { "" } else { "s" },
            names.join(", "),
            if dropped.len() > MAX_DROPS_LISTED {
                format!(" and {} more", dropped.len() - MAX_DROPS_LISTED)
            } else {
                String::new()
            },
            clean_text(&rec.line, FIELD_CHARS, false)
        );
        self.forge.close_pr(number, &detail).await?;
        self.reject(rec, detail)?;
        Ok(true)
    }

    /// Builds and records the next batch: the land list, the cut, the
    /// squashed commit in the local mirror, then the record. `Ok(None)` when
    /// the line holds nothing the base lacks, `Err(NotDue)` when the batch
    /// is held for size or age.
    async fn plan(
        &mut self,
        state: &BatchLine,
        tip: ChangesetId,
        base_tip: ChangesetId,
    ) -> Result<Result<Option<BatchRecord>, BatchOutcome>, MirrorError> {
        let tip_view = self.view(tip).await?;
        let base_view = self.view(base_tip).await?;
        if tip_view.changeset.root == base_view.changeset.root {
            return Ok(Ok(None));
        }
        let (ids, capped) = self.lands(tip, state.start).await?;
        let views = self.views(&ids).await?;
        let (mut lands, truncated) = self.describe(&ids, &views).await?;
        if lands.is_empty() {
            return Ok(Ok(None));
        }
        // A batch carries at most MAX_COMMITS per-land commits (D26), so the
        // cut always applies that cap.
        let max_lands = self
            .config
            .max_lands
            .unwrap_or(MAX_COMMITS)
            .min(MAX_COMMITS);
        if let Some(age) = self.config.max_age_ms {
            let oldest_ms = lands.iter().map(|l| l.authored_ms).min().unwrap_or(0);
            let full = lands.len() >= max_lands;
            if !full && (self.clock)().saturating_sub(oldest_ms) < age {
                return Ok(Err(BatchOutcome::NotDue {
                    lands: lands.len(),
                    oldest_ms,
                }));
            }
        }
        let mut uncut = None;
        let (cut, base, held) = match Some(max_lands) {
            Some(max) if lands.len() > max.max(1) && capped => {
                uncut = Some(format!(
                    "the line has more than {MAX_LANDS} lands to batch, so the oldest are \
                     not read and the {max}-land cap is not applied: this batch takes them all"
                ));
                (tip, base_tip, 0)
            }
            Some(max) if lands.len() > max.max(1) => {
                let keep = keep_with_reverts(&lands, max.max(1));
                let cut = lands[lands.len() - keep].id;
                // The base the line held at the cut: the newest sync at or
                // before it, else the base at the batch start.
                let pos = ids.iter().position(|i| *i == cut).unwrap_or(0);
                let held_base = ids[pos..]
                    .iter()
                    .filter_map(|i| views.get(i))
                    .find_map(|v| v.changeset.metadata.get(SYNC_KEY))
                    .and_then(|b| ChangesetId::parse(b).ok())
                    .or(state.start_base);
                match held_base {
                    Some(base) => {
                        let held = lands.len() - keep;
                        lands.drain(..held);
                        (cut, base, held)
                    }
                    None => {
                        uncut = Some(format!(
                            "the line's record predates batch cuts, so the base at the cut \
                             is unknown and the {max}-land cap is not applied: this batch \
                             takes them all"
                        ));
                        (tip, base_tip, 0)
                    }
                }
            }
            _ => (tip, base_tip, 0),
        };
        if let Some(why) = &uncut {
            tracing::warn!(
                repo = %self.config.repo, line = %self.config.line, "mirror batch: {why}"
            );
        }
        let base_sha = self.db.sha_of(base)?.ok_or_else(|| {
            config_err(format!(
                "base {base} has no git commit: follow {} first",
                self.config.base.branch
            ))
        })?;
        let in_batch: HashMap<ChangesetId, ChangesetId> = lands
            .iter()
            .filter_map(|l| l.reverts.map(|r| (r, l.id)))
            .collect();
        let mut drops = Vec::new();
        for l in lands.iter().rev() {
            if let Some(revert) = in_batch.get(&l.id) {
                let reason = match self.db.batch_drop_reason(&self.config.line, l.id)? {
                    Some(r) => clean_text(&r, FIELD_CHARS, false),
                    None => "reverted on the line".to_owned(),
                };
                drops.push(Dropped {
                    land: l.id,
                    revert: *revert,
                    reason,
                });
            }
        }

        let number = self.db.next_batch_number()?;
        let created_ms = (self.clock)();
        let branch = format!("{BRANCH_PREFIX}{number}");
        let line = clean_text(&self.config.line, FIELD_CHARS, false);
        let title = clean_text(
            &format!(
                "graft batch {number}: {} land{} from {line}",
                lands.len(),
                if lands.len() == 1 { "" } else { "s" }
            ),
            TITLE_CHARS,
            false,
        );
        // The tree: the integration tip, exported deterministically.
        let tree = {
            let cx = self.call("export");
            let stamp = Stamp(created_ms);
            let repo = self.config.repo.clone();
            let committer = self.config.committer.clone();
            let mut exporter = Exporter::new(
                self.api,
                &cx,
                self.db,
                &repo,
                &committer,
                &stamp,
                self.mirror.dir(),
            )?;
            let tip_sha = exporter.export_changeset(cut).await?;
            self.mirror.commit_tree(tip_sha).await?
        };
        let (fallback, commits) = match self
            .chain(number, base, base_sha, tree, created_ms, &lands)
            .await?
        {
            Ok(commits) => (None, commits),
            Err(why) => {
                tracing::warn!(
                    repo = %self.config.repo, line = %self.config.line, batch = number,
                    "mirror batch: exported squashed: {why}"
                );
                (Some(why), Vec::new())
            }
        };
        let (body, message_body) = render(
            &BatchHeader {
                number,
                line: self.config.line.clone(),
                branch: self.config.base.branch.clone(),
                base,
                base_sha,
                start: state.start,
                tip: cut,
                capped,
                truncated,
                drops,
                held,
                uncut,
                max_lands,
                fallback: fallback.clone(),
            },
            &lands,
        );
        let squash_sha = match commits.last() {
            Some(c) => c.sha,
            None => {
                let ident = format!(
                    "{} <{}> {}",
                    self.config.committer.name,
                    self.config.committer.email,
                    git_time(created_ms)
                );
                let record = ImportedCommit {
                    sha: GitSha([0; 20]),
                    headers: format!("author {ident}\ncommitter {ident}\n\n").into_bytes(),
                    message: None,
                    raw_trees: Vec::new(),
                };
                // The body was cleaned for GitHub's markdown; a commit
                // message keeps `@` as written (emails, co-author lines).
                let message = format!("{title}\n\n{message_body}").replace("@\u{200b}", "@");
                let raw = commit_object(tree, &[base_sha], &record, &message);
                self.mirror.write_commit(&raw).await?
            }
        };

        let rec = BatchRecord {
            number,
            line: self.config.line.clone(),
            base_branch: self.config.base.branch.clone(),
            base,
            base_sha,
            start: state.start,
            tip: cut,
            lands: lands.len() as u64,
            created_ms,
            branch,
            title,
            body,
            squash_sha,
            pr_number: None,
            pr_url: None,
            state: BatchState::Planned,
            detail: String::new(),
            merge_sha: None,
            merged: None,
        };
        if !self.db.insert_batch(&rec, &commits)? {
            return Err(config_err(format!(
                "another run opened a batch for line {} first",
                self.config.line
            )));
        }
        tracing::info!(
            repo = %self.config.repo, line = %self.config.line, batch = number,
            lands = rec.lands, held, commits = commits.len(), head = %squash_sha,
            "mirror batch planned"
        );
        Ok(Ok(Some(rec)))
    }

    /// The batch's per-land commits on `base_sha`, oldest first, written to
    /// the mirror (D26). Each land's own change is replayed onto the base as
    /// an unlanded changeset, and its tree is the land's commit's tree; the
    /// last tree must be `tree`, the cut tip's. `Ok(Err(why))` when the
    /// batch is to be squashed instead. One diff, one create and one export
    /// per land, at most [`MAX_COMMITS`].
    async fn chain(
        &mut self,
        number: u64,
        base: ChangesetId,
        base_sha: GitSha,
        tree: GitSha,
        created_ms: i64,
        lands: &[LandLine],
    ) -> Result<Result<Vec<BatchCommit>, String>, MirrorError> {
        if lands.len() > MAX_COMMITS {
            return Ok(Err(format!(
                "the batch has {} lands, more than the {MAX_COMMITS} per-land commits a \
                 batch carries",
                lands.len()
            )));
        }
        // Oldest first: each land is rebased (line merge) onto the replay
        // before it. Its parent is landed, so `Rebase` moves the land alone:
        // a cherry-pick of its own change that keeps whatever the base
        // changed in the same files, never a whole-file overwrite.
        let mut replays = Vec::with_capacity(lands.len());
        let mut prev = base;
        for land in lands.iter().rev() {
            if land.parent.is_none() {
                return Ok(Err(format!("land {} has no parent", short(land.id))));
            }
            let cx = self.call("replay");
            prev = match self
                .api
                .rebase(
                    &cx,
                    Rebase {
                        repo: self.config.repo.clone(),
                        changeset: land.id,
                        onto: prev,
                        merge: MergeMode::Line,
                        dry_run: false,
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(ApiError::Conflict { paths, .. }) => {
                    return Ok(Err(format!(
                        "land {} does not merge onto the base and the lands before it \
                         ({} conflicting path{})",
                        short(land.id),
                        paths.len(),
                        if paths.len() == 1 { "" } else { "s" }
                    )));
                }
                Err(e) => return Err(e.into()),
            };
            replays.push(prev);
        }
        let mut trees = Vec::with_capacity(replays.len());
        {
            let cx = self.call("export-chain");
            let stamp = Stamp(created_ms);
            let repo = self.config.repo.clone();
            let committer = self.config.committer.clone();
            let mut exporter = Exporter::new(
                self.api,
                &cx,
                self.db,
                &repo,
                &committer,
                &stamp,
                self.mirror.dir(),
            )?;
            for r in &replays {
                let sha = exporter.export_changeset(*r).await?;
                trees.push(self.mirror.commit_tree(sha).await?);
            }
        }
        if trees.last() != Some(&tree) {
            return Ok(Err(
                "replaying the lands onto the base does not reproduce the integration tip"
                    .to_owned(),
            ));
        }
        let mut parent = base_sha;
        let mut out = Vec::with_capacity(trees.len());
        for (seq, (land, tree)) in lands.iter().rev().zip(&trees).enumerate() {
            let raw = land_commit(number, land, *tree, parent, &self.config.committer);
            let sha = self.mirror.write_commit(&raw).await?;
            out.push(BatchCommit {
                seq: seq as u32,
                land: land.id,
                sha,
                merged: None,
            });
            parent = sha;
        }
        Ok(Ok(out))
    }

    /// Where each of batch `rec`'s per-land commits reached the base
    /// branch, given the PR's merge commit `merge` (imported by follow, so
    /// in the mirror). A rebase-merge leaves one new commit per land, newest
    /// at `merge`, each naming its land in its only `Graft-Changeset`
    /// trailer; a merge commit has the branch head as its second parent and
    /// keeps the commits as they are. A squash-merge maps nothing. Reads at
    /// most one commit per land.
    async fn map_merged(
        &self,
        rec: &BatchRecord,
        merge: GitSha,
    ) -> Result<Vec<BatchCommit>, MirrorError> {
        let mut commits = self.db.batch_commits(rec.number)?;
        if commits.is_empty() {
            return Ok(commits);
        }
        let Ok(raw) = self.mirror.read_commit(merge).await else {
            return Ok(Vec::new());
        };
        let (parents, _) = parse_commit(&raw);
        if parents.len() == 2 && parents[1] == rec.squash_sha {
            for c in &mut commits {
                c.merged = Some(c.sha);
            }
            return Ok(commits);
        }
        let at: HashMap<ChangesetId, usize> = commits
            .iter()
            .enumerate()
            .map(|(i, c)| (c.land, i))
            .collect();
        let mut cur = merge;
        for _ in 0..commits.len() {
            let Ok(raw) = self.mirror.read_commit(cur).await else {
                break;
            };
            let (parents, land) = parse_commit(&raw);
            let Some(i) = land.and_then(|l| at.get(&l).copied()) else {
                break;
            };
            commits[i].merged = Some(cur);
            match parents.first() {
                Some(p) => cur = *p,
                None => break,
            }
        }
        Ok(commits)
    }

    /// Runs whichever step the batch is at, until it waits on the operator
    /// or is final.
    async fn advance(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        if rec.state == BatchState::Planned {
            self.push(rec).await?;
        }
        if rec.state == BatchState::Pushed {
            self.open(rec).await?;
        }
        if rec.state == BatchState::Open {
            self.observe(rec).await?;
        }
        Ok(())
    }

    /// Pushes the batch's head commit to `graft/batch-<n>`, on a lease that
    /// requires the branch not to exist. The same commit already there is
    /// fine (a retry); other commits are not overwritten, and the batch is
    /// rejected.
    async fn push(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        let token = if self.mirror.is_https() {
            Some(self.forge.token().await?)
        } else {
            None
        };
        let bearer = token.as_ref().map(SecretToken::expose);
        let outcome = self
            .mirror
            .push_lease(&rec.branch, rec.squash_sha, None, bearer)
            .await?;
        if outcome == PushOutcome::Rejected {
            let there = self.mirror.ls_remote(&[rec.branch.as_str()]).await?;
            if there.first().and_then(|(_, s)| *s) != Some(rec.squash_sha) {
                return self.reject(
                    rec,
                    format!(
                        "branch {} exists with other commits; not overwritten",
                        rec.branch
                    ),
                );
            }
        }
        rec.state = BatchState::Pushed;
        self.db.put_batch(rec)?;
        Ok(())
    }

    /// Opens the PR, or adopts the one already on the branch. Never merges
    /// or queues it.
    async fn open(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        let pull = match self.forge.find_pull(&rec.branch).await? {
            Some(pull) => pull,
            None => {
                let new = crate::github::NewPull {
                    title: rec.title.clone(),
                    body: rec.body.clone(),
                    head: rec.branch.clone(),
                    base: rec.base_branch.clone(),
                };
                self.forge.create_pull(&new).await?
            }
        };
        self.record_pull(rec, &pull);
        rec.state = BatchState::Open;
        rec.detail = "waiting for review; land-pr merges it once its checks pass".to_owned();
        self.db.put_batch(rec)?;
        tracing::info!(
            line = %rec.line, batch = rec.number, pr = pull.number,
            "mirror batch: PR open"
        );
        Ok(())
    }

    fn record_pull(&self, rec: &mut BatchRecord, pull: &Pull) {
        rec.pr_number = Some(pull.number);
        rec.pr_url = Some(clean_text(&pull.url, 300, false));
    }

    /// Reads the PR. Merged, with its merge commit imported by follow: the
    /// batch is merged, and the next one counts from its tip. Closed
    /// unmerged: rejected, and the line is left as it is.
    async fn observe(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        let number = rec
            .pr_number
            .ok_or_else(|| config_err("an open batch has no PR"))?;
        let pull = self.forge.get_pull(number).await?;
        self.record_pull(rec, &pull);
        if pull.merged {
            let merge = pull
                .merge_commit
                .as_deref()
                .and_then(GitSha::from_hex)
                .ok_or_else(|| config_err("GitHub merged the PR without a merge commit"))?;
            rec.merge_sha = Some(merge);
            let Some(merged) = self.db.changeset_of(merge)? else {
                rec.detail = format!("merged on GitHub as {merge}; waiting for follow");
                self.db.put_batch(rec)?;
                return Ok(());
            };
            let commits = self.map_merged(rec, merge).await?;
            rec.merged = Some(merged);
            rec.state = BatchState::Merged;
            rec.detail = format!("merged as {merge} (graft {merged})");
            // The merge commit's snapshot is the batch tip's (plus whatever
            // GitHub's main gained meanwhile), so the next sync diffs from
            // the tip and the next batch counts lands after it.
            let state = self
                .db
                .batch_line(&rec.line)?
                .ok_or_else(|| config_err(format!("line {} has no batch record", rec.line)))?;
            // A batch cut before the line's newest sync: the next sync
            // rebuilds its starting point from this batch (see
            // `merged_on_synced`) rather than keep an unlanded changeset
            // across runs, which nothing would protect from collection.
            let cut_before_sync = rec.base != state.synced;
            let next = BatchLine {
                synced: state.synced,
                sync_from: if cut_before_sync {
                    state.sync_from
                } else {
                    rec.tip
                },
                start: rec.tip,
                start_base: Some(rec.base),
                sync_batch: cut_before_sync.then_some(rec.number),
            };
            self.db.settle_batch(rec, &rec.line, &next, &commits)?;
            tracing::info!(
                line = %rec.line, batch = rec.number, merge = %merge,
                "mirror batch merged"
            );
            return Ok(());
        }
        if !pull.open {
            return self.reject(rec, format!("closed without merging: {}", pull.url));
        }
        if pull.head_sha != rec.squash_sha.to_hex() {
            rec.detail = format!(
                "waiting for the operator; the branch was changed outside graft (head {})",
                clean_text(&pull.head_sha, 40, false)
            );
        }
        self.db.put_batch(rec)?;
        Ok(())
    }

    /// The sync point after merging batch `rec`, cut before the line's
    /// newest sync: the batch's change rebased onto the base the line
    /// absorbed since. Built by the sync that uses it, from landed
    /// changesets only (`rec.base`, `rec.tip`, `state.synced`), so nothing
    /// unlanded outlives the run. The line past the cut holds that base plus the
    /// batch, so the next sync diffs only the lands after the batch, and
    /// none of the merged ones is applied again. When the batch does not
    /// rebase onto that base (the line's own sync merged it), the old sync
    /// point is kept and the next sync says where it conflicts.
    async fn merged_on_synced(
        &mut self,
        rec: &BatchRecord,
        state: &BatchLine,
    ) -> Result<ChangesetId, MirrorError> {
        let net = self.diff(rec.base, rec.tip).await?;
        if net.is_empty() {
            return Ok(state.synced);
        }
        let message = format!(
            "graft: batch {} of line {} on {}",
            rec.number, rec.line, state.synced
        );
        let squashed = self
            .create(vec![rec.base], net, message, state.synced)
            .await?;
        let cx = self.call("rebase");
        match self
            .api
            .rebase(
                &cx,
                Rebase {
                    repo: self.config.repo.clone(),
                    changeset: squashed,
                    onto: state.synced,
                    merge: MergeMode::Line,
                    dry_run: false,
                },
            )
            .await
        {
            Ok(id) => Ok(id),
            Err(ApiError::Conflict { paths, .. }) => {
                tracing::warn!(
                    line = %rec.line, batch = rec.number, conflicts = paths.len(),
                    "mirror batch: the merged batch does not rebase onto the line's base; \
                     the next sync starts from the old base"
                );
                Ok(state.sync_from)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn reject(&self, rec: &mut BatchRecord, detail: String) -> Result<(), MirrorError> {
        let detail = clean_text(&detail, 500, false);
        tracing::warn!(
            line = %rec.line, batch = rec.number, detail = %detail,
            "mirror batch rejected; the line is left as it is"
        );
        rec.state = BatchState::Rejected;
        rec.detail = detail;
        self.db.put_batch(rec)?;
        Ok(())
    }
}

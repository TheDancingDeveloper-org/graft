//! Importing GitHub PRs on a graft-primary line (`docs/design.md` §11.3,
//! mode B). PRs opened by people or external bots (Dependabot) become drafts
//! in a `github-pr/<n>` workspace. Once GitHub says the PR is approved, the
//! drafts land through graft's `Land`, export pushes the line, and the PR
//! closes as merged by the export.
//!
//! Each poll, at most [`MAX_PULLS`] open PRs into the line's branch are read:
//!
//! 1. A PR whose head branch is `graft/…` is the PR bridge's own and is
//!    skipped.
//! 2. A new PR, or one whose head moved, is fetched (`refs/pull/<n>/head`)
//!    into the local mirror and imported with `graft-git`. Its stack must
//!    sit on a changeset of the line. The workspace is opened there (or
//!    reopened, when the PR moved to a newer base) and its head set to the
//!    imported head. A head that cannot be imported is `refused`, with the
//!    reason, until it moves again.
//! 3. A held PR's workspace lease is renewed, so the drafts live as long as
//!    the PR is open.
//! 4. An approved held PR is landed through the [`Lander`].
//! 5. A landed PR is closed with a comment once export has pushed the
//!    landed changeset. When the landed commits are the PR's own (no rebase),
//!    GitHub has already marked it merged and nothing is closed.
//!
//! A held PR that is no longer open on GitHub (closed by its author) has
//! its workspace closed, and its drafts are abandoned. Every step is
//! recorded before the next one runs, and repeating one is safe.

use graft_git::GitSha;
use graft_git::import::{ImportConfig, import};
use graft_git::map::MirrorMap;
use graft_model::ChangesetId;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{Pair, WorkspaceRef};
use graft_proto::{ApiError, CallContext};

use crate::MirrorError;
use crate::bridge::clean_text;
use crate::db::MirrorDb;
use crate::follow::FollowLine;
use crate::git::Mirror;
use crate::github::{OpenPull, PullSource};
use crate::land::{Lander, hold, stack_base};

/// Open PRs read per poll.
pub const MAX_PULLS: u32 = 100;

/// The lease of a PR's workspace, renewed every poll while the PR is open.
pub const PR_LEASE_MS: u64 = 24 * 60 * 60 * 1000;

/// Where an imported PR is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportState {
    /// Its head is held as drafts, waiting for approval.
    Held,
    /// Landed through graft; waiting for export to push it.
    Landed,
    /// Landed, exported, and the PR closed (or merged by GitHub). Final.
    Done,
    /// Closed on GitHub without landing; the drafts were let go. Final.
    Closed,
    /// Its current head cannot be imported or held (`detail` says why).
    /// Retried when the head moves.
    Refused,
}

impl ImportState {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            ImportState::Held => "held",
            ImportState::Landed => "landed",
            ImportState::Done => "done",
            ImportState::Closed => "closed",
            ImportState::Refused => "refused",
        }
    }

    /// The state with this stored name.
    pub fn parse(s: &str) -> Option<ImportState> {
        Some(match s {
            "held" => ImportState::Held,
            "landed" => ImportState::Landed,
            "done" => ImportState::Done,
            "closed" => ImportState::Closed,
            "refused" => ImportState::Refused,
            _ => return None,
        })
    }
}

/// One imported PR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrImport {
    /// The PR number.
    pub number: u64,
    /// The head commit last imported (or refused).
    pub head_sha: GitSha,
    /// The workspace holding its drafts.
    pub workspace: String,
    /// The imported head changeset (all zero when refused before import).
    pub head: ChangesetId,
    /// Where it is.
    pub state: ImportState,
    /// Why it is where it is.
    pub detail: String,
    /// The changeset the line ended with when it landed.
    pub landed: Option<ChangesetId>,
}

/// The workspace that holds PR `number`'s drafts.
pub fn pr_workspace(number: u64) -> String {
    format!("github-pr/{number}")
}

/// The local ref PR `number`'s head is fetched to.
pub fn pr_ref(number: u64) -> String {
    format!("refs/graft-mirror/pull/{number}")
}

fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

/// Imports the PRs into one graft-primary line.
pub struct PrImporter<'a, A, P, L> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    source: &'a P,
    lander: &'a L,
    repo: String,
    line: FollowLine,
}

impl<'a, A, P, L> PrImporter<'a, A, P, L>
where
    A: AdminApi + ObjectApi + RepoApi + WorkspaceApi,
    P: PullSource,
    L: Lander,
{
    /// An importer of PRs into `line` of `repo`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        source: &'a P,
        lander: &'a L,
        repo: impl Into<String>,
        line: FollowLine,
    ) -> Self {
        PrImporter {
            api,
            cx,
            db,
            mirror,
            source,
            lander,
            repo: repo.into(),
            line,
        }
    }

    /// One poll: imports, renews, lands and closes as far as each PR goes.
    /// A PR whose step fails stays where it was and is retried next poll.
    /// Returns the records of the PRs this poll touched.
    pub async fn poll_once(&mut self) -> Result<Vec<PrImport>, MirrorError> {
        self.mirror.ensure().await?;
        let open = self.source.open_pulls(&self.line.branch, MAX_PULLS).await?;
        let mut touched = Vec::new();
        for pr in &open {
            if pr.head_ref.starts_with("graft/") || pr.base_ref != self.line.branch {
                continue;
            }
            match self.step(pr).await {
                Ok(Some(rec)) => touched.push(rec),
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    repo = %self.repo, pr = pr.number, error = %e,
                    "mirror PR import step failed; retried on the next poll"
                ),
            }
        }
        // Held PRs that are no longer open, and landed PRs waiting on
        // export. Only the records that are not final are read.
        for mut rec in self.db.open_pr_imports()? {
            if open.iter().any(|p| p.number == rec.number) {
                continue;
            }
            let result = match rec.state {
                ImportState::Held => self.let_go(&mut rec).await,
                ImportState::Landed => self.finish(&mut rec).await,
                _ => Ok(()),
            };
            match result {
                Ok(()) => touched.push(rec),
                Err(e) => tracing::warn!(
                    repo = %self.repo, pr = rec.number, error = %e,
                    "mirror PR import step failed; retried on the next poll"
                ),
            }
        }
        Ok(touched)
    }

    /// Advances one open PR. `None` when there was nothing to do.
    async fn step(&mut self, pr: &OpenPull) -> Result<Option<PrImport>, MirrorError> {
        let sha = GitSha::from_hex(&pr.head_sha)
            .ok_or_else(|| config_err("GitHub gave a malformed commit id"))?;
        let mut rec = match self.db.pr_import(pr.number)? {
            Some(rec) if rec.head_sha == sha && rec.state != ImportState::Closed => rec,
            Some(rec) if matches!(rec.state, ImportState::Landed | ImportState::Done) => {
                // Landed already; a later push to the PR is not taken.
                return Ok(Some(rec));
            }
            prev => match self.import_head(pr, sha, prev.as_ref()).await {
                Ok(rec) => rec,
                Err(MirrorError::Api(e)) if refusal(&e) => {
                    let rec = self.refuse(pr.number, sha, prev, format!("{e:?}"))?;
                    return Ok(Some(rec));
                }
                Err(MirrorError::Git(e)) => {
                    let rec = self.refuse(pr.number, sha, prev, e.to_string())?;
                    return Ok(Some(rec));
                }
                Err(e) => return Err(e),
            },
        };
        match rec.state {
            ImportState::Held => {
                self.renew(&rec).await?;
                if self.source.approved(pr.number).await? {
                    self.land(&mut rec).await?;
                }
                if rec.state == ImportState::Landed {
                    self.finish(&mut rec).await?;
                }
                Ok(Some(rec))
            }
            ImportState::Landed => {
                self.finish(&mut rec).await?;
                Ok(Some(rec))
            }
            _ => Ok(None),
        }
    }

    fn call(&self, what: &str, n: u64) -> CallContext {
        let mut cx = self.cx.clone();
        cx.request_id = format!("{}-pr-{n}-{what}", self.cx.request_id);
        cx
    }

    /// Fetches, imports and holds the PR's head.
    async fn import_head(
        &mut self,
        pr: &OpenPull,
        sha: GitSha,
        prev: Option<&PrImport>,
    ) -> Result<PrImport, MirrorError> {
        let local = pr_ref(pr.number);
        self.mirror
            .fetch_refs(&[(format!("refs/pull/{}/head", pr.number), local.clone())])
            .await?;
        if self.mirror.local_ref(&local).await? != Some(sha) {
            return Err(config_err("the PR moved while it was fetched"));
        }
        let cx = self.call("import", pr.number);
        let config = ImportConfig {
            git_dir: self.mirror.dir().to_owned(),
            repo: self.repo.clone(),
            lines: vec![(local, pr_workspace(pr.number))],
            tags: false,
            allow_lossy: Vec::new(),
            batch: 256,
        };
        let report = import(self.api, &cx, self.db, &config).await?;
        let head = report
            .lines
            .first()
            .map(|(_, id)| *id)
            .or(self.db.changeset_of(sha)?)
            .ok_or_else(|| config_err("the PR head did not import"))?;
        let base = stack_base(self.api, &cx, &self.repo, head).await?;
        let workspace = pr_workspace(pr.number);
        hold(
            self.api,
            &cx,
            &self.repo,
            &self.line.line,
            &workspace,
            base,
            head,
            PR_LEASE_MS,
        )
        .await?;
        let rec = PrImport {
            number: pr.number,
            head_sha: sha,
            workspace,
            head,
            state: ImportState::Held,
            detail: format!("held from {}", clean_text(&pr.url, 200, false)),
            landed: prev.and_then(|p| p.landed),
        };
        self.db.put_pr_import(&rec)?;
        tracing::info!(repo = %self.repo, pr = pr.number, head = %head, "mirror PR imported as drafts");
        Ok(rec)
    }

    fn refuse(
        &self,
        number: u64,
        sha: GitSha,
        prev: Option<PrImport>,
        detail: String,
    ) -> Result<PrImport, MirrorError> {
        let rec = PrImport {
            number,
            head_sha: sha,
            workspace: pr_workspace(number),
            head: prev.map_or(ChangesetId(graft_model::ObjectId([0; 32])), |p| p.head),
            state: ImportState::Refused,
            detail: clean_text(&detail, 500, false),
            landed: None,
        };
        self.db.put_pr_import(&rec)?;
        tracing::warn!(repo = %self.repo, pr = number, detail = %rec.detail, "mirror PR not imported");
        Ok(rec)
    }

    async fn renew(&self, rec: &PrImport) -> Result<(), MirrorError> {
        let cx = self.call("renew", rec.number);
        self.api
            .renew_lease(&cx, self.wref(rec), PR_LEASE_MS)
            .await?;
        Ok(())
    }

    fn wref(&self, rec: &PrImport) -> WorkspaceRef {
        WorkspaceRef {
            repo: self.repo.clone(),
            name: rec.workspace.clone(),
        }
    }

    /// Lands an approved PR's drafts.
    async fn land(&mut self, rec: &mut PrImport) -> Result<(), MirrorError> {
        let cx = self.call("land", rec.number);
        let landed = match self
            .lander
            .land(&cx, &self.repo, &self.line.line, rec.head)
            .await
        {
            Ok(landed) => landed,
            Err(e @ (ApiError::Conflict { .. } | ApiError::Rejected { .. })) => {
                // Stays held: the author can rebase or fix it, and a new head
                // is imported then.
                rec.detail = clean_text(&format!("land refused: {e:?}"), 500, false);
                self.db.put_pr_import(rec)?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        rec.landed = Some(landed);
        rec.state = ImportState::Landed;
        rec.detail = format!("landed as {landed}");
        self.db.put_pr_import(rec)?;
        tracing::info!(repo = %self.repo, pr = rec.number, landed = %landed, "mirror PR landed");
        match self.api.close_workspace(&cx, self.wref(rec), false).await {
            Ok(()) | Err(ApiError::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Once export has pushed the landed changeset, closes the PR (unless
    /// GitHub merged it already, seeing its own commits on the branch).
    async fn finish(&mut self, rec: &mut PrImport) -> Result<(), MirrorError> {
        let landed = rec
            .landed
            .ok_or_else(|| config_err("a landed PR has no landed changeset"))?;
        let Some(exported) = self
            .db
            .export_state(&self.line.line)?
            .and_then(|s| s.changeset)
        else {
            return Ok(());
        };
        let cx = self.call("exported", rec.number);
        let pushed = landed == exported
            || self
                .api
                .is_ancestor(
                    &cx,
                    Pair {
                        repo: self.repo.clone(),
                        a: landed,
                        b: exported,
                    },
                )
                .await?;
        if !pushed {
            return Ok(());
        }
        let pull = self.source.pull(rec.number).await?;
        if pull.open && !pull.merged {
            let sha = self
                .db
                .sha_of(landed)?
                .map(|s| s.to_hex())
                .unwrap_or_default();
            let comment = format!(
                "Landed through graft as {landed} (git {sha}) and exported to `{}`.\n\n\
                 Closed by graft-mirror: the landed commits were rebased, so GitHub does not \
                 see this PR's own commits on the branch.",
                self.line.branch
            );
            self.source.close_pull(rec.number, &comment).await?;
        }
        rec.state = ImportState::Done;
        self.db.put_pr_import(rec)?;
        Ok(())
    }

    /// A held PR closed on GitHub: its workspace closes and its drafts are
    /// abandoned.
    async fn let_go(&mut self, rec: &mut PrImport) -> Result<(), MirrorError> {
        let pull = self.source.pull(rec.number).await?;
        if pull.open {
            // Open, just not into this branch any more, or past MAX_PULLS:
            // left for a later poll.
            return Ok(());
        }
        let cx = self.call("close", rec.number);
        match self.api.close_workspace(&cx, self.wref(rec), false).await {
            Ok(()) | Err(ApiError::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        rec.state = ImportState::Closed;
        rec.detail = "closed on GitHub without landing".to_owned();
        self.db.put_pr_import(rec)?;
        Ok(())
    }
}

/// Whether graft refused the PR's content itself (not a passing failure).
fn refusal(e: &ApiError) -> bool {
    matches!(
        e,
        ApiError::Rejected { .. }
            | ApiError::InvalidPath { .. }
            | ApiError::InvalidChange { .. }
            | ApiError::UnknownParent { .. }
    )
}

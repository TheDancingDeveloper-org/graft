//! The PR bridge, git-primary write path (`docs/design.md` §11.3, mode A).
//!
//! A land request on a git-primary line becomes:
//!
//! 1. a **plan**: the stack from the request's head down to the line tip,
//!    every changeset a draft, the bottom one based on the tip, and the
//!    line caught up with upstream. The plan is written before anything else
//!    happens, so a restart resumes it;
//! 2. an **export** of the stack as commits on upstream's tip, pushed to the
//!    branch `graft/<workspace>/<short-id>`, never forced;
//! 3. a **PR** against the line's upstream branch, its body naming the
//!    workspace, work item and session. A PR already on that branch is
//!    adopted, so a retried open cannot make a second PR;
//! 4. an **enqueue** in the merge queue;
//! 5. **tracking**: each poll reads the PR. A PR closed without merging, a
//!    failing check, or a branch changed outside graft is `Rejected` with its
//!    URL. A merged PR is `Merged`, and once follow has imported its merge
//!    commit and that commit names the stack head by `Graft-Changeset`, the
//!    request is `Landed`.
//!
//! Every step is written to the mirror DB before the next one runs, and
//! every step is safe to run again. Text that comes from a draft (the PR
//! title and body) is data: it is cleaned, capped, and stripped of the
//! characters that could reroute or spoof it.
//!
//! What this module does not do yet: the server-side hand-off that creates a
//! request from `Land` (the engine's `land` is still a stub), rebasing a
//! stale stack (`rebase` is G2.7), and moving the drafts to `landed` or
//! `rejected` in graft itself (no API for that outside a line move). The
//! mirror keeps those outcomes in its own DB until those land.

use graft_git::export::Committer;
use graft_git::exporter::{Exporter, LandTimes, SESSION_KEY, WORK_ITEM_KEY};
use graft_git::map::MirrorMap;
use graft_git::{GitError, GitSha};
use graft_model::{Changeset, ChangesetId, Timestamp};
use graft_proto::CallContext;
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{DraftState, GetChangesets};

use crate::MirrorError;
use crate::credentials::SecretToken;
use crate::current_ref;
use crate::db::MirrorDb;
use crate::git::{Mirror, PushOutcome};
use crate::github::{Forge, ForgeError, NewPull, Pull};

/// The most changesets one request may carry.
pub const MAX_STACK: usize = 64;

/// Enqueue attempts that fail transiently before the request is rejected.
pub const MAX_ENQUEUE_ATTEMPTS: u32 = 5;

/// The authority this mirror claims for the lines it bridges.
pub const GIT_PRIMARY: &str = "git-primary";

const TITLE_CHARS: usize = 120;
const BODY_CHARS: usize = 8000;
const TRAILER: &str = "Graft-Changeset:";

/// Where a request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeState {
    /// Planned; nothing pushed yet.
    Requested,
    /// The branch holds the exported stack.
    Pushed,
    /// A PR is open on that branch.
    Open,
    /// The PR merged; waiting for follow to import the merge commit.
    Merged,
    /// The merge commit is on the line and names the stack head. Final.
    Landed,
    /// Closed, failed or refused, with the reason and any URL in `detail`.
    /// Final.
    Rejected,
}

impl BridgeState {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            BridgeState::Requested => "requested",
            BridgeState::Pushed => "pushed",
            BridgeState::Open => "open",
            BridgeState::Merged => "merged",
            BridgeState::Landed => "landed",
            BridgeState::Rejected => "rejected",
        }
    }

    /// The state with this stored name.
    pub fn parse(s: &str) -> Option<BridgeState> {
        Some(match s {
            "requested" => BridgeState::Requested,
            "pushed" => BridgeState::Pushed,
            "open" => BridgeState::Open,
            "merged" => BridgeState::Merged,
            "landed" => BridgeState::Landed,
            "rejected" => BridgeState::Rejected,
            _ => return None,
        })
    }

    /// Whether no further step will run.
    pub fn is_terminal(self) -> bool {
        matches!(self, BridgeState::Landed | BridgeState::Rejected)
    }
}

/// One land request and what the bridge has done for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRecord {
    /// The stack head: the request's identity.
    pub head: ChangesetId,
    /// The graft line it lands on.
    pub line: String,
    /// The workspace the stack came from.
    pub workspace: String,
    /// The branch the stack is pushed to.
    pub branch: String,
    /// When the request was planned (ms). Fixes the exported committer date.
    pub requested_ms: i64,
    /// The stack, bottom first, ending at `head`.
    pub covered: Vec<ChangesetId>,
    /// The exported head commit, once pushed.
    pub export_sha: Option<GitSha>,
    /// The PR number, once opened.
    pub pr_number: Option<u64>,
    /// The PR's web address.
    pub pr_url: Option<String>,
    /// The PR's GraphQL node id.
    pub node_id: Option<String>,
    /// Whether the PR is in the merge queue.
    pub enqueued: bool,
    /// Where the request is.
    pub state: BridgeState,
    /// Why it is where it is (a reason, and any URL, when rejected).
    pub detail: String,
    /// The merge commit, once merged.
    pub merge_sha: Option<GitSha>,
    /// The changeset the merge commit imported as, once landed.
    pub landed: Option<ChangesetId>,
    /// Failed enqueue attempts so far.
    pub attempts: u32,
}

/// What the bridge needs to know about the line and the graft repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeConfig {
    /// The graft repository.
    pub repo: String,
    /// The graft line, which must be git-primary.
    pub line: String,
    /// The upstream branch the line follows (`main`).
    pub base_branch: String,
}

/// Land requests on one git-primary line, bridged to GitHub PRs.
pub struct Bridge<'a, A, F> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    forge: &'a F,
    config: BridgeConfig,
    clock: fn() -> i64,
    calls: u64,
}

/// Exported commits are dated by the request's planning time, so a re-export
/// after a restart writes the same bytes.
struct Stamp(i64);

impl LandTimes for Stamp {
    fn landed_at(&self, _id: ChangesetId) -> Result<Timestamp, GitError> {
        Ok(Timestamp {
            ms: self.0,
            offset_min: 0,
        })
    }
}

fn committer() -> Committer {
    Committer {
        name: "graft".to_owned(),
        email: "graft@graft.invalid".to_owned(),
    }
}

fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

/// A workspace name, safe as a git branch segment.
fn check_workspace(ws: &str) -> Result<(), MirrorError> {
    let ok = !ws.is_empty()
        && ws.len() <= 64
        && !ws.starts_with('-')
        && ws
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(config_err(format!("not a workspace name: {ws:?}")))
    }
}

/// Whether `c` reorders or hides text (bidi controls and marks).
fn is_bidi(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Cleans text that GitHub will show or that comes from a draft: control
/// characters and bidi controls go, `@` gets a zero-width space so it does
/// not ping anyone, and the result is capped at `max` characters. With
/// `multiline`, newlines stay.
pub fn clean_text(text: &str, max: usize, multiline: bool) -> String {
    let mut out = String::new();
    for (n, c) in text.chars().enumerate() {
        if n >= max {
            break;
        }
        match c {
            '\n' if multiline => out.push('\n'),
            '\t' => out.push(' '),
            '@' => out.push_str("@\u{200b}"),
            c if c.is_control() || is_bidi(c) => {}
            c => out.push(c),
        }
    }
    out
}

impl<'a, A, F> Bridge<'a, A, F>
where
    A: RepoApi + ObjectApi,
    F: Forge,
{
    /// A bridge for `config.line`. Claims the line as git-primary: a line
    /// another authority already claims is refused, so two writers never
    /// both bridge it.
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        forge: &'a F,
        config: BridgeConfig,
    ) -> Result<Self, MirrorError> {
        let holder = db.claim_authority(&config.line, GIT_PRIMARY)?;
        if holder != GIT_PRIMARY {
            return Err(config_err(format!(
                "line {} is claimed by {holder}; it cannot also be bridged",
                config.line
            )));
        }
        Ok(Bridge {
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

    /// Accepts a land request for the stack headed by `head`, from
    /// `workspace`, and advances it as far as it will go. Asking again for
    /// the same head returns the same request and does not repeat finished
    /// steps.
    pub async fn submit(
        &mut self,
        workspace: &str,
        head: ChangesetId,
    ) -> Result<PullRecord, MirrorError> {
        check_workspace(workspace)?;
        let mut rec = match self.db.pull(head)? {
            Some(rec) => rec,
            None => {
                let rec = self.plan(workspace, head).await?;
                self.db.put_pull(&rec)?;
                rec
            }
        };
        self.advance(&mut rec).await?;
        self.db
            .pull(head)?
            .ok_or_else(|| config_err("the request vanished from the mirror DB"))
    }

    /// Moves every open request one step along, as far as it goes. A request
    /// whose step fails stays where it was, and the error is logged. Returns
    /// the requests that are not yet final.
    pub async fn track(&mut self) -> Result<Vec<PullRecord>, MirrorError> {
        let mut heads = Vec::new();
        for rec in self.db.open_pulls()? {
            let head = rec.head;
            let mut rec = rec;
            if let Err(e) = self.advance(&mut rec).await {
                tracing::warn!(
                    line = %self.config.line, head = %head, error = %e,
                    "mirror PR bridge step failed; retried on the next poll"
                );
            }
            heads.push(head);
        }
        let mut out = Vec::with_capacity(heads.len());
        for head in heads {
            if let Some(rec) = self.db.pull(head)? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// The request for `head`, if the bridge has one.
    pub fn request(&self, head: ChangesetId) -> Result<Option<PullRecord>, MirrorError> {
        Ok(self.db.pull(head)?)
    }

    fn call(&mut self, what: &str) -> CallContext {
        self.calls += 1;
        let mut cx = self.cx.clone();
        cx.request_id = format!("bridge-{}-{}-{what}", (self.clock)(), self.calls);
        cx
    }

    /// The line's tip, checked against upstream: the bridge writes only on a
    /// line that follow has caught up and not stopped.
    async fn line_tip(&mut self) -> Result<ChangesetId, MirrorError> {
        let repo = self.config.repo.clone();
        let cx = self.call("tip");
        let tip = current_ref(self.api, &cx, &repo, &self.config.line)
            .await
            .map_err(MirrorError::from)?
            .ok_or_else(|| config_err(format!("line {} has no tip", self.config.line)))?;
        let sha = self
            .db
            .sha_of(tip)?
            .ok_or_else(|| config_err(format!("line tip {tip} was not imported from upstream")))?;
        let state = self.db.line(&self.config.line)?;
        if state.diverged.is_some() {
            return Err(config_err(format!(
                "line {} is diverged from upstream; the bridge will not write on top of it",
                self.config.line
            )));
        }
        if state.upstream != Some(sha) {
            return Err(config_err(format!(
                "upstream is not where graft's line is ({}): follow first",
                self.config.line
            )));
        }
        Ok(tip)
    }

    async fn view(
        &mut self,
        id: ChangesetId,
    ) -> Result<graft_proto::types::ChangesetView, MirrorError> {
        let cx = self.call("get");
        let repo = self.config.repo.clone();
        let found = self
            .api
            .get_changesets(
                &cx,
                GetChangesets {
                    repo,
                    ids: vec![id],
                },
            )
            .await
            .map_err(MirrorError::from)?;
        found
            .found
            .into_iter()
            .next()
            .ok_or_else(|| config_err(format!("changeset {id} is not in graft")))
    }

    /// Walks the stack from `head` down to the line tip and checks each
    /// changeset on the way.
    async fn plan(
        &mut self,
        workspace: &str,
        head: ChangesetId,
    ) -> Result<PullRecord, MirrorError> {
        let tip = self.line_tip().await?;
        let mut chain = Vec::new();
        let mut next = head;
        loop {
            if chain.len() >= MAX_STACK {
                return Err(config_err(format!(
                    "the stack is longer than {MAX_STACK} changesets"
                )));
            }
            let view = self.view(next).await?;
            if view.state != DraftState::Draft {
                return Err(config_err(format!(
                    "{next} is {:?}, not a draft: the stack is not based on the line tip",
                    view.state
                )));
            }
            if view.changeset.parents.len() != 1 {
                return Err(config_err(format!(
                    "{next} has {} parents; the bridge exports single-parent stacks",
                    view.changeset.parents.len()
                )));
            }
            let parent = view.changeset.parents[0];
            chain.push((next, view.changeset));
            if parent == tip {
                break;
            }
            next = parent;
        }
        chain.reverse();
        let id_text = head.to_string();
        let short = &id_text[1..13];
        Ok(PullRecord {
            head,
            line: self.config.line.clone(),
            workspace: workspace.to_owned(),
            branch: format!("graft/{workspace}/{short}"),
            requested_ms: (self.clock)(),
            covered: chain.into_iter().map(|(id, _)| id).collect(),
            export_sha: None,
            pr_number: None,
            pr_url: None,
            node_id: None,
            enqueued: false,
            state: BridgeState::Requested,
            detail: String::new(),
            merge_sha: None,
            landed: None,
            attempts: 0,
        })
    }

    /// Runs whichever step the request is at, once each, until it waits on
    /// something or is final.
    async fn advance(&mut self, rec: &mut PullRecord) -> Result<(), MirrorError> {
        if !self.held(rec).await? {
            return Ok(());
        }
        if rec.state == BridgeState::Requested {
            self.push(rec).await?;
        }
        if rec.state == BridgeState::Pushed {
            self.open(rec).await?;
        }
        if rec.state == BridgeState::Open {
            self.observe(rec).await?;
        }
        if rec.state == BridgeState::Open && !rec.enqueued {
            self.enqueue(rec).await?;
        }
        if rec.state == BridgeState::Merged {
            self.land_check(rec).await?;
        }
        Ok(())
    }

    /// Checks, before a step that writes, that every changeset of the stack
    /// is still a draft. One batched read. A draft that was abandoned (its
    /// workspace closed), superseded, or collected can no longer be exported
    /// or landed as asked, so the request is rejected with that reason. A
    /// merged request is not checked: its landing reads the commit GitHub
    /// made, not the drafts. Returns `false` once the request is rejected.
    async fn held(&mut self, rec: &mut PullRecord) -> Result<bool, MirrorError> {
        if rec.state.is_terminal() || rec.state == BridgeState::Merged {
            return Ok(true);
        }
        let cx = self.call("held");
        let repo = self.config.repo.clone();
        let found = self
            .api
            .get_changesets(
                &cx,
                GetChangesets {
                    repo,
                    ids: rec.covered.clone(),
                },
            )
            .await
            .map_err(MirrorError::from)?;
        let not_draft = found
            .found
            .iter()
            .filter(|v| v.state != DraftState::Draft)
            .count();
        let gone = found.missing.len() + found.collected.len();
        if not_draft == 0 && gone == 0 && found.found.len() == rec.covered.len() {
            return Ok(true);
        }
        let detail = format!(
            "drafts abandoned: {} of {} changesets are no longer held as drafts",
            not_draft + gone,
            rec.covered.len()
        );
        self.reject(rec, detail)?;
        Ok(false)
    }

    /// Exports the stack onto upstream's tip and pushes its branch.
    async fn push(&mut self, rec: &mut PullRecord) -> Result<(), MirrorError> {
        let tip = self.line_tip().await?;
        let bottom = rec
            .covered
            .first()
            .copied()
            .ok_or_else(|| config_err("empty stack"))?;
        let view = self.view(bottom).await?;
        if view.changeset.first_parent() != Some(tip) {
            return Err(config_err(
                "the stack is no longer based on the line tip: sync it and land again",
            ));
        }
        let lands = Stamp(rec.requested_ms);
        let committer = committer();
        let cx = self.call("export");
        let repo = self.config.repo.clone();
        let sha = {
            let mut exporter = Exporter::new(
                self.api,
                &cx,
                self.db,
                &repo,
                &committer,
                &lands,
                self.mirror.dir(),
            )?;
            exporter.export_changeset(rec.head).await?
        };
        // The token travels to git in the environment, and only for HTTPS.
        let token = if self.mirror.is_https() {
            Some(self.forge.token().await?)
        } else {
            None
        };
        let outcome = self
            .mirror
            .push_branch(&rec.branch, sha, token.as_ref().map(SecretToken::expose))
            .await?;
        if outcome == PushOutcome::Rejected {
            // The branch exists. The same commit is fine; other commits are
            // someone else's work, and the bridge does not overwrite them.
            let there = self.mirror.ls_remote(&[rec.branch.as_str()]).await?;
            if there.first().and_then(|(_, s)| *s) != Some(sha) {
                return self.reject(
                    rec,
                    format!(
                        "branch {} exists with other commits; not overwritten",
                        rec.branch
                    ),
                );
            }
        }
        rec.export_sha = Some(sha);
        rec.state = BridgeState::Pushed;
        self.db.put_pull(rec)?;
        Ok(())
    }

    /// Opens the PR, or adopts the one already on the branch.
    async fn open(&mut self, rec: &mut PullRecord) -> Result<(), MirrorError> {
        let existing = self.forge.find_pull(&rec.branch).await?;
        let pull = match existing {
            Some(pull) => pull,
            None => {
                let head_view = self.view(rec.head).await?;
                let new = self.new_pull(rec, &head_view.changeset);
                self.forge.create_pull(&new).await?
            }
        };
        self.record_pull(rec, &pull);
        rec.state = BridgeState::Open;
        rec.detail.clear();
        self.db.put_pull(rec)?;
        Ok(())
    }

    fn new_pull(&self, rec: &PullRecord, head: &Changeset) -> NewPull {
        let first_line = head.message.lines().map(str::trim).find(|l| !l.is_empty());
        let title = clean_text(first_line.unwrap_or(""), TITLE_CHARS, false);
        let title = if title.is_empty() {
            format!("graft: {} {}", rec.workspace, &rec.head.to_string()[1..13])
        } else {
            title
        };
        let mut body = String::from("Opened by graft-mirror from a graft land request.\n\n");
        body.push_str(&format!("- Workspace: {}\n", rec.workspace));
        body.push_str(&format!("- Line: {}\n", clean_text(&rec.line, 100, false)));
        body.push_str(&format!(
            "- Changesets: {} (stack head {})\n",
            rec.covered.len(),
            rec.head
        ));
        if let Some(item) = head.metadata.get(WORK_ITEM_KEY) {
            body.push_str(&format!("- Work item: {}\n", clean_text(item, 100, false)));
        }
        if let Some(session) = head.metadata.get(SESSION_KEY) {
            body.push_str(&format!("- Session: {}\n", clean_text(session, 100, false)));
        }
        body.push('\n');
        body.push_str(&clean_text(&head.message, BODY_CHARS, true));
        // The trailer GitHub's squash message will carry (its default takes
        // the PR title and body). It goes last, as a trailer, after the
        // draft's own text, so it is never mistaken for that text.
        body.push_str(&format!("\n\n{TRAILER} {}\n", rec.head));
        NewPull {
            title,
            body,
            head: rec.branch.clone(),
            base: self.config.base_branch.clone(),
        }
    }

    fn record_pull(&self, rec: &mut PullRecord, pull: &Pull) {
        rec.pr_number = Some(pull.number);
        rec.pr_url = Some(pull.url.clone());
        rec.node_id = Some(pull.node_id.clone());
    }

    /// Reads the PR and checks it still says what graft pushed.
    async fn observe(&mut self, rec: &mut PullRecord) -> Result<(), MirrorError> {
        let number = rec
            .pr_number
            .ok_or_else(|| config_err("an open request has no PR"))?;
        let export = rec
            .export_sha
            .ok_or_else(|| config_err("an open request has no export"))?;
        let pull = self.forge.get_pull(number).await?;
        self.record_pull(rec, &pull);
        if pull.head_sha != export.to_hex() {
            let detail = format!(
                "branch changed outside graft (head is now {}): {}",
                pull.head_sha, pull.url
            );
            return self.reject(rec, detail);
        }
        if pull.merged {
            let merge = pull
                .merge_commit
                .as_deref()
                .and_then(GitSha::from_hex)
                .ok_or_else(|| config_err("GitHub merged the PR without a merge commit"))?;
            rec.merge_sha = Some(merge);
            rec.state = BridgeState::Merged;
            self.db.put_pull(rec)?;
            return Ok(());
        }
        if !pull.open {
            return self.reject(rec, format!("closed without merging: {}", pull.url));
        }
        for run in self.forge.check_runs(&pull.head_sha).await? {
            let failed = matches!(
                run.conclusion.as_deref(),
                Some("failure" | "timed_out" | "cancelled" | "action_required")
            );
            if failed {
                let detail = format!("check {} failed: {}", run.name, run.url);
                return self.reject(rec, detail);
            }
        }
        self.db.put_pull(rec)?;
        Ok(())
    }

    /// Puts the PR in the merge queue. A transient failure is counted; after
    /// [`MAX_ENQUEUE_ATTEMPTS`] of them, or on a refusal, the request is
    /// rejected with the reason.
    async fn enqueue(&mut self, rec: &mut PullRecord) -> Result<(), MirrorError> {
        let node = rec
            .node_id
            .clone()
            .ok_or_else(|| config_err("an open request has no PR id"))?;
        match self.forge.enqueue(&node).await {
            Ok(()) => {
                rec.enqueued = true;
                rec.detail.clear();
                self.db.put_pull(rec)?;
                Ok(())
            }
            Err(ForgeError::Transient(what)) => {
                rec.attempts += 1;
                if rec.attempts >= MAX_ENQUEUE_ATTEMPTS {
                    let detail = format!(
                        "merge queue unreachable after {} attempts ({what})",
                        rec.attempts
                    );
                    return self.reject(rec, detail);
                }
                rec.detail = format!("merge queue not reached yet ({what})");
                self.db.put_pull(rec)?;
                Ok(())
            }
            Err(ForgeError::Refused(what)) => {
                self.reject(rec, format!("merge queue refused: {what}"))
            }
        }
    }

    /// Once follow has the merge commit, the request is landed if that
    /// commit names the stack head. Otherwise it is rejected: the merge was
    /// not this request's.
    async fn land_check(&mut self, rec: &mut PullRecord) -> Result<(), MirrorError> {
        let merge = rec
            .merge_sha
            .ok_or_else(|| config_err("a merged request has no merge commit"))?;
        let Some(id) = self.db.changeset_of(merge)? else {
            return Ok(());
        };
        let view = self.view(id).await?;
        let trailer = format!("{TRAILER} {}", rec.head);
        let named = view.changeset.message.lines().any(|l| l.trim() == trailer);
        if !named {
            let detail = format!(
                "merge commit {merge} does not name the stack head {}",
                rec.head
            );
            return self.reject(rec, detail);
        }
        rec.landed = Some(id);
        rec.state = BridgeState::Landed;
        rec.detail = format!("landed as {id}");
        self.db.put_pull(rec)?;
        Ok(())
    }

    fn reject(&self, rec: &mut PullRecord, detail: String) -> Result<(), MirrorError> {
        tracing::warn!(
            line = %rec.line, head = %rec.head, detail = %clean_text(&detail, 200, false),
            "mirror PR bridge: request rejected"
        );
        rec.state = BridgeState::Rejected;
        rec.detail = clean_text(&detail, 500, false);
        self.db.put_pull(rec)?;
        Ok(())
    }
}

fn system_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

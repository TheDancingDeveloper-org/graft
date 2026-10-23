//! The authority switch (`docs/design.md` §11.3): hand one line from
//! git-primary (GitHub writes, graft follows) to graft-primary (graft lands,
//! GitHub is exported to), or back for a rollback.
//!
//! The steps, each a `graft-mirror authority` command, are the same in both
//! directions:
//!
//! 1. **freeze**: records the switch in the mirror DB and closes the
//!    upstream [`Gate`] to people. From git-primary the branch takes no
//!    pushes at all; follow imports what is already there. From
//!    graft-primary the gate stays open to the mirror alone, so export can
//!    push what graft has landed; graft's own lands are stopped on graft's
//!    side (the runbook says how).
//! 2. **catch up**: the running follow (or export) brings both sides level.
//! 3. **verify**: upstream's branch and graft's line name the same commit
//!    through the mirror map, nothing is diverged or in flight, and the last
//!    N changesets of the line re-export to the very commits upstream holds
//!    (the same tree changes, the same parents, the same sha).
//! 4. **flip**: re-reads both sides, refuses if either moved since verify,
//!    and moves the mirror DB's claim on the line by compare-and-swap, in
//!    one transaction with resetting follow's and export's records. Towards
//!    graft-primary the gate then lets the mirror alone write; towards
//!    git-primary it stays shut to everyone until unfreeze. A running export
//!    or follow re-reads the claim each poll and stops writing a line it no
//!    longer holds.
//! 5. **unfreeze**: opens the gate in the new authority's posture and ends
//!    the switch. Before a flip, unfreeze aborts the switch instead and
//!    restores the old posture.
//!
//! Nothing here forces a push or moves a graft ref: a switch only changes
//! who may write. Every step is idempotent, so a step that failed half way
//! is run again.

use std::future::Future;
use std::path::{Path, PathBuf};

use graft_git::GitSha;
use graft_git::export::commit_sha;
use graft_git::fidelity::check_changeset;
use graft_git::map::MirrorMap;
use graft_model::{Changeset, ChangesetId};
use graft_proto::CallContext;
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::GetChangesets;
use serde_json::{Value, json};

use crate::credentials::CredentialProvider;
use crate::db::{ExportState, LineState, MirrorDb, SwitchRecord};
use crate::export::GRAFT_PRIMARY;
use crate::follow::FollowLine;
use crate::git::Mirror;
use crate::github::HttpForge;
use crate::{MirrorError, current_ref};

/// Changesets `verify` re-exports when not told otherwise.
pub const DEFAULT_LAST: usize = 100;

/// The most changesets one `verify` re-exports, so the check stays bounded.
pub const MAX_LAST: usize = 10_000;

/// The authority claim for a git-primary line: GitHub accepts writes and
/// follow imports them.
pub const GIT_PRIMARY: &str = "git-primary";

/// Which side accepts writes to a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// GitHub is authoritative; graft follows (mode A).
    GitPrimary,
    /// graft is authoritative; GitHub is exported to (mode B).
    GraftPrimary,
}

impl Authority {
    /// The claim as the mirror DB stores it.
    pub fn as_str(self) -> &'static str {
        match self {
            Authority::GitPrimary => GIT_PRIMARY,
            Authority::GraftPrimary => GRAFT_PRIMARY,
        }
    }

    /// Parses a stored claim.
    pub fn parse(s: &str) -> Option<Authority> {
        match s {
            GIT_PRIMARY => Some(Authority::GitPrimary),
            GRAFT_PRIMARY => Some(Authority::GraftPrimary),
            _ => None,
        }
    }

    /// The other side.
    pub fn other(self) -> Authority {
        match self {
            Authority::GitPrimary => Authority::GraftPrimary,
            Authority::GraftPrimary => Authority::GitPrimary,
        }
    }

    /// The gate's posture while this side holds the line.
    pub fn gate(self) -> GateState {
        match self {
            Authority::GitPrimary => GateState::Open,
            Authority::GraftPrimary => GateState::MirrorOnly,
        }
    }
}

/// How far a switch has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Frozen; not verified since.
    Frozen,
    /// Verified at a recorded pair of tips.
    Verified,
    /// The claim has moved; unfreeze finishes the switch.
    Flipped,
}

impl Stage {
    /// The stage as the mirror DB stores it.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Frozen => "frozen",
            Stage::Verified => "verified",
            Stage::Flipped => "flipped",
        }
    }

    fn parse(s: &str) -> Option<Stage> {
        match s {
            "frozen" => Some(Stage::Frozen),
            "verified" => Some(Stage::Verified),
            "flipped" => Some(Stage::Flipped),
            _ => None,
        }
    }
}

/// A switch in progress on one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switch {
    /// The side that held the line when the switch began.
    pub from: Authority,
    /// The side the switch hands it to.
    pub to: Authority,
    /// How far it has got.
    pub stage: Stage,
    /// The graft tip verify checked.
    pub changeset: Option<ChangesetId>,
    /// The upstream commit verify checked.
    pub sha: Option<GitSha>,
}

impl Switch {
    fn from_record(rec: SwitchRecord) -> Result<Switch, MirrorError> {
        let bad = || refused("the mirror DB holds a malformed switch record");
        Ok(Switch {
            from: Authority::parse(&rec.from).ok_or_else(bad)?,
            to: Authority::parse(&rec.to).ok_or_else(bad)?,
            stage: Stage::parse(&rec.stage).ok_or_else(bad)?,
            changeset: rec.changeset,
            sha: rec.sha,
        })
    }

    fn to_record(&self) -> SwitchRecord {
        SwitchRecord {
            from: self.from.as_str().to_owned(),
            to: self.to.as_str().to_owned(),
            stage: self.stage.as_str().to_owned(),
            changeset: self.changeset,
            sha: self.sha,
        }
    }
}

/// Who may push to an upstream branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateState {
    /// No gate of the mirror's: the repository's own rules apply
    /// (git-primary).
    Open,
    /// Nobody pushes (an authority switch is under way).
    Frozen,
    /// Only the mirror pushes (graft-primary, decision D8).
    MirrorOnly,
}

impl GateState {
    /// A stable name for output.
    pub fn as_str(self) -> &'static str {
        match self {
            GateState::Open => "open",
            GateState::Frozen => "frozen",
            GateState::MirrorOnly => "mirror-only",
        }
    }
}

/// The write gate on upstream branches: a GitHub ruleset
/// ([`RulesetGate`]), or a hook on a local bare repository standing in for
/// GitHub ([`HookGate`]).
pub trait Gate: Sync {
    /// The gate on `branch` now.
    fn state(&self, branch: &str) -> impl Future<Output = Result<GateState, MirrorError>> + Send;
    /// Sets the gate on `branch`. Idempotent.
    fn set(
        &self,
        branch: &str,
        state: GateState,
    ) -> impl Future<Output = Result<(), MirrorError>> + Send;
}

/// The marker line that says a `pre-receive` hook is the gate's.
const HOOK_MARK: &str = "# graft-mirror authority gate";

/// The gate's state file in the bare repository: `<ref> <state>` lines.
const GATE_FILE: &str = "graft-gate";

/// The `pre-receive` hook. Bare repositories run hooks in the repository
/// directory, so the state file is found relative to it.
const HOOK: &str = r#"#!/bin/sh
# graft-mirror authority gate: a local stand-in for a GitHub ruleset.
# Written by `graft-mirror authority`; the state is in ./graft-gate.
status=0
while read -r old new ref; do
  state=$(awk -v r="$ref" '$1 == r { s = $2 } END { print s }' ./graft-gate 2>/dev/null)
  case "$state" in
    frozen)
      echo "graft-mirror: $ref is frozen for an authority switch" >&2
      status=1 ;;
    mirror-only)
      if [ "${GRAFT_MIRROR_PUSH:-}" != 1 ]; then
        echo "graft-mirror: $ref is graft-primary; only the mirror writes it" >&2
        status=1
      fi ;;
  esac
done
exit $status
"#;

/// The gate on a local bare repository that stands in for GitHub (the
/// rehearsal and tests): a `pre-receive` hook reading a state file. A
/// mirror-only branch admits pushes that carry
/// [`crate::git::MIRROR_PUSH_ENV`]; that marks the mirror's pushes, it does
/// not authenticate them, which is all a local stand-in needs.
#[derive(Clone, Debug)]
pub struct HookGate {
    dir: PathBuf,
}

impl HookGate {
    /// The gate on the bare repository at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> HookGate {
        HookGate { dir: dir.into() }
    }

    /// The gate for `upstream` when it is a local bare repository, not a
    /// URL.
    pub fn for_upstream(upstream: &str) -> Option<HookGate> {
        let path = upstream.strip_prefix("file://").unwrap_or(upstream);
        if path.contains("://") || !Path::new(path).join("HEAD").is_file() {
            return None;
        }
        Some(HookGate::new(path))
    }

    fn io(what: &str, e: std::io::Error) -> MirrorError {
        MirrorError::Config(format!("the local gate: {what}: {e}"))
    }

    fn entries(&self) -> Result<Vec<(String, String)>, MirrorError> {
        let text = match std::fs::read_to_string(self.dir.join(GATE_FILE)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(Self::io("reading the state", e)),
        };
        Ok(text
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(r, s)| (r.to_owned(), s.trim().to_owned()))
            .collect())
    }

    fn install_hook(&self) -> Result<(), MirrorError> {
        let hooks = self.dir.join("hooks");
        std::fs::create_dir_all(&hooks).map_err(|e| Self::io("creating hooks/", e))?;
        let path = hooks.join("pre-receive");
        match std::fs::read_to_string(&path) {
            Ok(t) if t == HOOK => return Ok(()),
            Ok(t) if !t.contains(HOOK_MARK) => {
                return Err(MirrorError::Config(
                    "the upstream has a pre-receive hook of its own; the gate will not replace it"
                        .to_owned(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Self::io("reading the hook", e)),
        }
        write_atomic(&path, HOOK.as_bytes()).map_err(|e| Self::io("writing the hook", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| Self::io("making the hook executable", e))?;
        }
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("graft-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn branch_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

impl Gate for HookGate {
    async fn state(&self, branch: &str) -> Result<GateState, MirrorError> {
        let r = branch_ref(branch);
        Ok(
            match self
                .entries()?
                .into_iter()
                .rfind(|(name, _)| *name == r)
                .map(|(_, s)| s)
                .as_deref()
            {
                Some("frozen") => GateState::Frozen,
                Some("mirror-only") => GateState::MirrorOnly,
                _ => GateState::Open,
            },
        )
    }

    async fn set(&self, branch: &str, state: GateState) -> Result<(), MirrorError> {
        self.install_hook()?;
        let r = branch_ref(branch);
        let mut entries: Vec<(String, String)> = self
            .entries()?
            .into_iter()
            .filter(|(name, _)| *name != r)
            .collect();
        if state != GateState::Open {
            entries.push((r, state.as_str().to_owned()));
        }
        let text: String = entries.iter().map(|(n, s)| format!("{n} {s}\n")).collect();
        write_atomic(&self.dir.join(GATE_FILE), text.as_bytes())
            .map_err(|e| Self::io("writing the state", e))
    }
}

/// The gate on GitHub: a repository ruleset named
/// `graft-mirror-authority-<branch>` that blocks updates, deletion and
/// force-pushes of the branch. Frozen, nobody bypasses it; mirror-only, the
/// mirror's GitHub App does (decision D8); open, the ruleset is deleted and
/// the repository's own rules (the merge queue) apply. The credential needs
/// the repository's administration permission.
pub struct RulesetGate<'a, C> {
    forge: &'a HttpForge<C>,
    app_id: Option<u64>,
}

impl<'a, C: CredentialProvider> RulesetGate<'a, C> {
    /// The gate through `forge`. `app_id` is the mirror's GitHub App, the
    /// only actor a mirror-only branch admits; without it a branch cannot be
    /// made mirror-only.
    pub fn new(forge: &'a HttpForge<C>, app_id: Option<u64>) -> Self {
        RulesetGate { forge, app_id }
    }
}

/// The name of the gate's ruleset for `branch`.
pub fn ruleset_name(branch: &str) -> String {
    format!("graft-mirror-authority-{branch}")
}

/// The gate's ruleset for `branch`, bypassed by `app` when given.
pub fn ruleset_body(branch: &str, app: Option<u64>) -> Value {
    let bypass: Vec<Value> = app
        .map(|id| json!({ "actor_id": id, "actor_type": "Integration", "bypass_mode": "always" }))
        .into_iter()
        .collect();
    json!({
        "name": ruleset_name(branch),
        "target": "branch",
        "enforcement": "active",
        "conditions": { "ref_name": { "include": [branch_ref(branch)], "exclude": [] } },
        "rules": [
            { "type": "update", "parameters": { "update_allows_fetch_and_merge": false } },
            { "type": "deletion" },
            { "type": "non_fast_forward" }
        ],
        "bypass_actors": bypass,
    })
}

impl<C: CredentialProvider> Gate for RulesetGate<'_, C> {
    async fn state(&self, branch: &str) -> Result<GateState, MirrorError> {
        Ok(
            match self.forge.ruleset_named(&ruleset_name(branch)).await? {
                None => GateState::Open,
                Some(r) if r.bypass.is_empty() => GateState::Frozen,
                Some(_) => GateState::MirrorOnly,
            },
        )
    }

    async fn set(&self, branch: &str, state: GateState) -> Result<(), MirrorError> {
        let existing = self.forge.ruleset_named(&ruleset_name(branch)).await?;
        let body = match state {
            GateState::Open => {
                if let Some(r) = existing {
                    self.forge.delete_ruleset(r.id).await?;
                }
                return Ok(());
            }
            GateState::Frozen => ruleset_body(branch, None),
            GateState::MirrorOnly => {
                let app = self.app_id.ok_or_else(|| {
                    MirrorError::Config(
                        "a graft-primary branch is writable by the mirror's GitHub App alone \
                         (decision D8): configure [github.github_app]"
                            .to_owned(),
                    )
                })?;
                ruleset_body(branch, Some(app))
            }
        };
        self.forge
            .put_ruleset(existing.map(|r| r.id), &body)
            .await?;
        Ok(())
    }
}

/// Where one line stands, for `authority status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The claim on the line in the mirror DB (`None`: never claimed).
    pub claim: Option<String>,
    /// The switch in progress, if any.
    pub switch: Option<Switch>,
    /// The upstream gate.
    pub gate: GateState,
    /// Upstream's branch.
    pub upstream: Option<GitSha>,
    /// graft's line.
    pub tip: Option<ChangesetId>,
    /// The git commit of graft's line, through the mirror map.
    pub tip_sha: Option<GitSha>,
    /// Follow's record of the line.
    pub follow: LineState,
    /// Export's record of the line, if export has started it.
    pub export: Option<ExportState>,
}

impl Status {
    /// Whether upstream and graft name the same commit.
    pub fn in_sync(&self) -> bool {
        self.upstream.is_some() && self.upstream == self.tip_sha
    }
}

/// What `verify` checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// graft's tip.
    pub tip: ChangesetId,
    /// Upstream's commit, the tip's export.
    pub sha: GitSha,
    /// Changesets re-exported and compared, newest first from the tip.
    pub checked: usize,
}

/// What `unfreeze` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unfrozen {
    /// Finished a flipped switch: the new authority holds the line.
    Finished(Authority),
    /// Aborted a switch that had not flipped: the old authority keeps it.
    Aborted(Authority),
    /// No switch: the gate was set to the holder's posture again.
    NoSwitch(Authority),
}

fn refused(what: impl Into<String>) -> MirrorError {
    MirrorError::Switch(what.into())
}

/// Runs the switch steps for one line.
pub struct Switcher<'a, A, G> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    gate: &'a G,
    repo: String,
    line: FollowLine,
}

impl<'a, A, G> Switcher<'a, A, G>
where
    A: RepoApi + ObjectApi,
    G: Gate,
{
    /// The steps for `line` of `repo`, calling `api` as `cx`, with the
    /// local mirror `mirror` of upstream and its gate `gate`.
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        gate: &'a G,
        repo: impl Into<String>,
        line: FollowLine,
    ) -> Self {
        Switcher {
            api,
            cx,
            db,
            mirror,
            gate,
            repo: repo.into(),
            line,
        }
    }

    fn claim(&self) -> Result<Authority, MirrorError> {
        let claim = self.db.authority(&self.line.line)?.ok_or_else(|| {
            refused(format!(
                "no authority holds line {}: start follow (git-primary) or export \
                 (graft-primary) first",
                self.line.line
            ))
        })?;
        Authority::parse(&claim)
            .ok_or_else(|| refused(format!("line {} is claimed by {claim}", self.line.line)))
    }

    fn switch(&self) -> Result<Option<Switch>, MirrorError> {
        self.db
            .switch(&self.line.line)?
            .map(Switch::from_record)
            .transpose()
    }

    fn put(&self, switch: &Switch) -> Result<(), MirrorError> {
        Ok(self.db.put_switch(&self.line.line, &switch.to_record())?)
    }

    async fn upstream(&self) -> Result<Option<GitSha>, MirrorError> {
        Ok(self
            .mirror
            .ls_remote(&[self.line.branch.as_str()])
            .await?
            .pop()
            .and_then(|(_, s)| s))
    }

    async fn tip(&self) -> Result<Option<ChangesetId>, MirrorError> {
        Ok(current_ref(self.api, &self.cx, &self.repo, &self.line.line).await?)
    }

    /// Where the line stands. Reads only.
    pub async fn status(&self) -> Result<Status, MirrorError> {
        self.mirror.ensure().await?;
        let tip = self.tip().await?;
        let tip_sha = match tip {
            Some(id) => self.db.sha_of(id)?,
            None => None,
        };
        Ok(Status {
            claim: self.db.authority(&self.line.line)?,
            switch: self.switch()?,
            gate: self.gate.state(&self.line.branch).await?,
            upstream: self.upstream().await?,
            tip,
            tip_sha,
            follow: self.db.line(&self.line.line)?,
            export: self.db.export_state(&self.line.line)?,
        })
    }

    /// Step 1: records the switch to the other side and closes the gate.
    /// Run again, it sets the gate again.
    pub async fn freeze(&self) -> Result<Switch, MirrorError> {
        let switch = match self.switch()? {
            Some(s) if s.stage == Stage::Flipped => {
                return Err(refused(
                    "the switch has flipped already; run unfreeze to finish it",
                ));
            }
            Some(s) => s,
            None => {
                let from = self.claim()?;
                let s = Switch {
                    from,
                    to: from.other(),
                    stage: Stage::Frozen,
                    changeset: None,
                    sha: None,
                };
                self.put(&s)?;
                s
            }
        };
        // From graft-primary the mirror still pushes what graft landed
        // before the freeze, so its own writes stay open.
        let gate = match switch.from {
            Authority::GitPrimary => GateState::Frozen,
            Authority::GraftPrimary => GateState::MirrorOnly,
        };
        self.gate.set(&self.line.branch, gate).await?;
        tracing::info!(
            repo = %self.repo, line = %self.line.line, from = switch.from.as_str(),
            to = switch.to.as_str(), gate = gate.as_str(), "authority switch frozen"
        );
        Ok(switch)
    }

    /// Step 3: checks the two sides are level and the last `last`
    /// changesets re-export to upstream's commits. A switch not yet flipped
    /// is marked verified at the tips checked. Without a switch it is a
    /// read-only check.
    pub async fn verify(&self, last: usize) -> Result<Verified, MirrorError> {
        if last == 0 || last > MAX_LAST {
            return Err(MirrorError::Config(format!(
                "--last must be between 1 and {MAX_LAST}"
            )));
        }
        self.mirror.ensure().await?;
        let up = self
            .upstream()
            .await?
            .ok_or_else(|| refused(format!("upstream has no branch {}", self.line.branch)))?;
        self.mirror.fetch(&[self.line.branch.as_str()]).await?;
        let tip = self
            .tip()
            .await?
            .ok_or_else(|| refused(format!("graft has no line {}", self.line.line)))?;
        let tip_sha = self.db.sha_of(tip)?.ok_or_else(|| {
            refused(format!(
                "graft's tip {tip} has no git commit yet: export (or follow) has not caught up"
            ))
        })?;
        if tip_sha != up {
            return Err(refused(format!(
                "not level: upstream is at {up}, graft at {tip} ({tip_sha}); \
                 let the mirror catch up and verify again"
            )));
        }
        match self.claim()? {
            Authority::GitPrimary => {
                if let Some(d) = self.db.line(&self.line.line)?.diverged {
                    return Err(refused(format!(
                        "follow is stopped on upstream commit {d}; resolve it first"
                    )));
                }
            }
            Authority::GraftPrimary => {
                if self
                    .db
                    .export_state(&self.line.line)?
                    .is_some_and(|s| s.diverged)
                {
                    return Err(refused(
                        "export is stopped on a divergence; run reconcile first",
                    ));
                }
            }
        }
        let checked = self.check_last(tip, last).await?;
        if let Some(mut s) = self.switch()?
            && s.stage != Stage::Flipped
        {
            s.stage = Stage::Verified;
            s.changeset = Some(tip);
            s.sha = Some(up);
            self.put(&s)?;
        }
        tracing::info!(
            repo = %self.repo, line = %self.line.line, tip = %tip, upstream = %up, checked,
            "authority verify passed"
        );
        Ok(Verified {
            tip,
            sha: up,
            checked,
        })
    }

    /// Re-exports up to `last` changesets along the first-parent chain from
    /// `tip` and compares each with the commit upstream holds.
    async fn check_last(&self, tip: ChangesetId, last: usize) -> Result<usize, MirrorError> {
        let mut next = Some(tip);
        let mut checked = 0;
        while let Some(id) = next
            && checked < last
        {
            let changeset = self.changeset(id).await?;
            self.check_one(id, &changeset).await?;
            checked += 1;
            next = changeset.parents.first().copied();
        }
        Ok(checked)
    }

    async fn changeset(&self, id: ChangesetId) -> Result<Changeset, MirrorError> {
        let mut got = self
            .api
            .get_changesets(
                &self.cx,
                GetChangesets {
                    repo: self.repo.clone(),
                    ids: vec![id],
                },
            )
            .await?;
        got.found
            .pop()
            .map(|v| v.changeset)
            .ok_or_else(|| refused(format!("changeset {id} is not on the server")))
    }

    /// One changeset against its commit: the commit is in the mirror, its
    /// tree changes and parents are graft's (the fidelity check), and
    /// building the commit again from the mirror record gives its sha.
    async fn check_one(&self, id: ChangesetId, changeset: &Changeset) -> Result<(), MirrorError> {
        let sha = self
            .db
            .sha_of(id)?
            .ok_or_else(|| refused(format!("changeset {id} has no git commit")))?;
        if !self.mirror.has_commit(sha).await? {
            return Err(refused(format!(
                "commit {sha} of {id} is not in the mirror"
            )));
        }
        check_changeset(self.mirror.dir(), self.db, id, changeset).map_err(|e| {
            refused(format!(
                "commit {sha} does not hold the changes of {id}: {e}"
            ))
        })?;
        let record = self
            .db
            .imported(id)?
            .ok_or_else(|| refused(format!("the mirror DB has no record of {id}")))?;
        let mut parents = Vec::with_capacity(changeset.parents.len());
        for p in &changeset.parents {
            parents.push(
                self.db
                    .sha_of(*p)?
                    .ok_or_else(|| refused(format!("parent {p} of {id} has no git commit")))?,
            );
        }
        let tree = self.mirror.commit_tree(sha).await?;
        let again = commit_sha(tree, &parents, &record, &changeset.message);
        if again != sha {
            return Err(refused(format!(
                "{id} re-exports to {again}, but upstream holds {sha}"
            )));
        }
        Ok(())
    }

    /// Step 4: moves the line to the other side. Refuses unless verified
    /// and neither side has moved since.
    pub async fn flip(&self) -> Result<Switch, MirrorError> {
        let mut s = self
            .switch()?
            .ok_or_else(|| refused("no switch in progress: freeze first"))?;
        match s.stage {
            Stage::Frozen => return Err(refused("not verified: run verify first")),
            Stage::Flipped => {
                return Err(refused(
                    "the switch has flipped already; run unfreeze to finish it",
                ));
            }
            Stage::Verified => {}
        }
        let up = self.upstream().await?;
        let tip = self.tip().await?;
        if up != s.sha || tip != s.changeset {
            s.stage = Stage::Frozen;
            self.put(&s)?;
            return Err(refused(format!(
                "a side moved since verify (upstream {}, graft {}); verify again",
                up.map_or("none".to_owned(), |u| u.to_hex()),
                tip.map_or("none".to_owned(), |t| t.to_string()),
            )));
        }
        let sha = s
            .sha
            .ok_or_else(|| refused("the verified switch has no sha"))?;
        if !self
            .db
            .flip_authority(&self.line.line, s.from.as_str(), s.to.as_str(), sha)?
        {
            return Err(refused(format!(
                "line {} is no longer claimed by {}",
                self.line.line,
                s.from.as_str()
            )));
        }
        s.stage = Stage::Flipped;
        // Towards graft-primary the mirror alone writes from now on (D8);
        // towards git-primary nobody does until unfreeze opens the branch.
        let gate = match s.to {
            Authority::GraftPrimary => GateState::MirrorOnly,
            Authority::GitPrimary => GateState::Frozen,
        };
        self.gate.set(&self.line.branch, gate).await?;
        tracing::info!(
            repo = %self.repo, line = %self.line.line, to = s.to.as_str(), upstream = %sha,
            "authority switch flipped"
        );
        Ok(s)
    }

    /// Step 5: opens the gate in the holder's posture and ends the switch.
    /// Before a flip this aborts the switch.
    pub async fn unfreeze(&self) -> Result<Unfrozen, MirrorError> {
        let (posture, out) = match self.switch()? {
            Some(s) if s.stage == Stage::Flipped => (s.to, Unfrozen::Finished(s.to)),
            Some(s) => (s.from, Unfrozen::Aborted(s.from)),
            None => {
                let claim = self.claim()?;
                (claim, Unfrozen::NoSwitch(claim))
            }
        };
        self.gate.set(&self.line.branch, posture.gate()).await?;
        self.db.end_switch(&self.line.line)?;
        tracing::info!(
            repo = %self.repo, line = %self.line.line, holder = posture.as_str(),
            "authority switch unfrozen"
        );
        Ok(out)
    }
}

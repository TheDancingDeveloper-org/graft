//! Per-line land policies (`docs/design.md` §8.3, decision D6).
//!
//! Every line has a [`LineConfig`]: its gate ([`LandPolicy`]), the path
//! rules that add requirements, whether it is post-land tested (with an
//! automatic revert on failure), and which side of the mirror is
//! authoritative. Lines the configuration does not name get
//! [`EngineConfig::default_line`](crate::EngineConfig::default_line).
//!
//! The CI side is out of the engine. It reaches the engine through
//! `Attest`, and the engine reaches it through [`LandHooks`] and the
//! journal (`RefMoved` starts post-land CI, `RevertRequested` asks for a
//! revert).

use std::future::Future;
use std::pin::Pin;

use graft_model::{ChangesetId, RepoPath};

/// The check a human review is recorded under. A path rule with
/// `human_review` needs a passing attestation of this check posted by a
/// person (`PrincipalKind::Human`).
pub const HUMAN_REVIEW: &str = "review/human";

/// How a line gates landing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandPolicy {
    /// Land at once; post-land CI catches breakage.
    Direct,
    /// The stack head must carry passing attestations for these checks.
    Attested {
        /// Required check names.
        checks: Vec<String>,
        /// Whether line-merged results must be re-attested (G3.7).
        attest_merged: bool,
    },
    /// Test the rebased candidate before the CAS (release lines). The
    /// candidate passes when it carries passing attestations for every
    /// check, or when [`LandHooks::test_candidate`] says so.
    Queued {
        /// Required check names.
        checks: Vec<String>,
    },
}

impl LandPolicy {
    /// The policy's name, as `Rejected { policy }` reports it.
    pub fn name(&self) -> &'static str {
        match self {
            LandPolicy::Direct => "direct",
            LandPolicy::Attested { .. } => "attested",
            LandPolicy::Queued { .. } => "queued",
        }
    }
}

/// Which side of a mirrored line accepts writes (`docs/design.md` §11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// GitHub is authoritative; lands go through the PR bridge.
    GitPrimary,
    /// graft is authoritative; GitHub is an exported mirror.
    GraftPrimary,
}

/// A requirement that applies when a land changes a path under `prefix`
/// (for example `require_human_review` on `deploy/**`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRule {
    /// The rule covers this path and everything beneath it.
    pub prefix: RepoPath,
    /// Checks the stack head must pass, whatever the line's policy.
    pub checks: Vec<String>,
    /// Whether a person must attest [`HUMAN_REVIEW`] on the stack head.
    pub human_review: bool,
    /// How a rebase merges a path under the rule, when the rule says. The
    /// rule with the longest prefix that says wins.
    pub merge: Option<PathMerge>,
}

/// How a rebase merges a path both sides changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMerge {
    /// Three-way line merge (decision D16).
    Line,
    /// No merge: any concurrent change is a conflict, so the client
    /// regenerates the file. For lockfiles and generated files, whose
    /// clean line merge can still be wrong.
    None,
}

/// File names that are [`PathMerge::None`] at any depth unless a path rule
/// says otherwise: lockfiles, which are derived rather than written.
pub const NO_MERGE_NAMES: &[&str] = &["Cargo.lock", "package-lock.json"];

/// Post-land testing of a line (decision D6): CI runs on each line move
/// (the `RefMoved` journal entry, or the mirror's push), and a failing
/// attestation of one of `checks` on a landed changeset asks for a revert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostLand {
    /// The post-land checks.
    pub checks: Vec<String>,
    /// Whether a failure journals `RevertRequested` and calls
    /// [`LandHooks::revert_requested`].
    pub auto_revert: bool,
}

/// Post-land check tracking of a line (v0.2, D25, D37): which checks a
/// landing waits on for its `land_checks` state. It never gates a land: no
/// land is refused, delayed or queued because of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LineChecks {
    /// No land-checks rows (the default, and every git-primary line).
    #[default]
    Off,
    /// Each landing gets a `pending` row, made `good` when every required
    /// check passed, or `bad` once a required check failed more than
    /// `retries` times.
    PostLand {
        /// Check names a landing must pass.
        required: Vec<String>,
        /// Failures of one check tolerated (re-runs) before it is `bad`.
        retries: u32,
    },
}

impl LineChecks {
    /// Whether landings on the line get land-checks rows.
    pub fn is_post_land(&self) -> bool {
        matches!(self, LineChecks::PostLand { .. })
    }
}

/// A line's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineConfig {
    /// Line name.
    pub name: String,
    /// Land gate.
    pub policy: LandPolicy,
    /// Mirror authority.
    pub authority: Authority,
    /// Extra requirements by path.
    pub path_rules: Vec<PathRule>,
    /// Post-land testing, if any.
    pub post_land: Option<PostLand>,
    /// Post-land check tracking (non-gating).
    pub checks: LineChecks,
}

impl LineConfig {
    /// A graft-primary `direct` line with no rules.
    pub fn direct(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            policy: LandPolicy::Direct,
            authority: Authority::GraftPrimary,
            path_rules: Vec::new(),
            post_land: None,
            checks: LineChecks::Off,
        }
    }

    /// Decision D6 for `main`: `attested` on `ci/build` and `ci/test`, and
    /// the same checks post-land with an automatic revert on failure.
    pub fn d6(name: &str) -> Self {
        let checks = vec!["ci/build".to_owned(), "ci/test".to_owned()];
        Self {
            name: name.to_owned(),
            policy: LandPolicy::Attested {
                checks: checks.clone(),
                attest_merged: false,
            },
            authority: Authority::GraftPrimary,
            path_rules: Vec::new(),
            post_land: Some(PostLand {
                checks,
                auto_revert: true,
            }),
            checks: LineChecks::Off,
        }
    }

    /// How a rebase merges `path`: the [`PathRule::merge`] of the rule with
    /// the longest matching prefix that sets one, else
    /// [`PathMerge::None`] for a name in [`NO_MERGE_NAMES`], else
    /// [`PathMerge::Line`].
    pub fn path_merge(&self, path: &RepoPath) -> PathMerge {
        let ruled = self
            .path_rules
            .iter()
            .filter(|r| path.starts_with(&r.prefix))
            .filter_map(|r| Some((r.prefix.as_str().len(), r.merge?)))
            .max_by_key(|(len, _)| *len);
        match ruled {
            Some((_, merge)) => merge,
            None if NO_MERGE_NAMES.contains(&path.name()) => PathMerge::None,
            None => PathMerge::Line,
        }
    }

    /// What the path rules add for a land changing `paths`: the checks the
    /// stack head must pass, each once in rule order, and whether a person
    /// must attest [`HUMAN_REVIEW`] on it.
    pub fn rule_checks<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a RepoPath>,
    ) -> (Vec<String>, bool) {
        if self.path_rules.is_empty() {
            return (Vec::new(), false);
        }
        let mut hit = vec![false; self.path_rules.len()];
        for path in paths {
            for (i, rule) in self.path_rules.iter().enumerate() {
                hit[i] |= path.starts_with(&rule.prefix);
            }
        }
        let mut checks: Vec<String> = Vec::new();
        let mut human = false;
        for (rule, _) in self.path_rules.iter().zip(hit).filter(|(_, h)| *h) {
            for check in &rule.checks {
                if !checks.contains(check) {
                    checks.push(check.clone());
                }
            }
            human |= rule.human_review;
        }
        (checks, human)
    }
}

/// A future returned by a hook.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A rebased candidate a `queued` line tests before its CAS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Repository.
    pub repo: String,
    /// Line.
    pub line: String,
    /// The stack head being landed.
    pub head: ChangesetId,
    /// The candidate: the head rebased onto `tip` (or the head itself).
    pub candidate: ChangesetId,
    /// The line tip the candidate sits on.
    pub tip: ChangesetId,
    /// The checks to run.
    pub checks: Vec<String>,
}

/// A candidate test's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Land it.
    Pass,
    /// Refuse the land, with a reason for people and agents.
    Fail(String),
}

/// A completed land, for post-land CI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedEvent {
    /// Repository.
    pub repo: String,
    /// Line.
    pub line: String,
    /// The stack head that was landed.
    pub head: ChangesetId,
    /// The new line tip.
    pub landed: ChangesetId,
    /// Line version after the move.
    pub version: u64,
    /// The post-land checks to run.
    pub checks: Vec<String>,
}

/// A request to revert a landed changeset whose post-land check failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertRequest {
    /// Repository.
    pub repo: String,
    /// Line.
    pub line: String,
    /// The landed changeset to revert.
    pub changeset: ChangesetId,
    /// The failed check.
    pub check: String,
    /// The run that failed.
    pub url: String,
}

/// The engine's calls out to CI. Each call is made after the transaction
/// it reports has committed, so a hook never sees a land that did not
/// happen; the journal entry written in that transaction is the durable
/// record a consumer resumes from when a call is lost.
pub trait LandHooks: Send + Sync {
    /// Tests a `queued` line's candidate before its CAS. The default
    /// refuses, so a `queued` line without a CI hook lands only candidates
    /// that already carry their attestations.
    fn test_candidate<'a>(&'a self, candidate: &'a Candidate) -> HookFuture<'a, Verdict> {
        let checks = candidate.checks.join(", ");
        Box::pin(async move { Verdict::Fail(format!("no CI hook ran {checks}")) })
    }

    /// A land committed on a line with post-land testing.
    fn landed(&self, _event: &LandedEvent) {}

    /// A post-land check failed on a line with `auto_revert`.
    fn revert_requested(&self, _request: &RevertRequest) {}
}

/// The hooks an engine has when none are installed: every default.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHooks;

impl LandHooks for NoHooks {}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).expect("path")
    }

    #[test]
    fn path_rules_add_checks_once() {
        let mut line = LineConfig::d6("main");
        line.path_rules = vec![
            PathRule {
                prefix: p("deploy"),
                checks: vec!["ci/deploy".to_owned(), "ci/lint".to_owned()],
                human_review: true,
                merge: None,
            },
            PathRule {
                prefix: p("deploy/k8s"),
                checks: vec!["ci/lint".to_owned()],
                human_review: false,
                merge: None,
            },
            PathRule {
                prefix: p("docs"),
                checks: vec!["ci/docs".to_owned()],
                human_review: false,
                merge: None,
            },
        ];
        assert_eq!(line.rule_checks([&p("src/a.rs")]), (Vec::new(), false));
        let (checks, human) = line.rule_checks([&p("deploy/k8s/app.yaml"), &p("src/a.rs")]);
        assert_eq!(checks, ["ci/deploy", "ci/lint"]);
        assert!(human);
        // A prefix matches whole segments only.
        assert_eq!(line.rule_checks([&p("deployment.md")]), (Vec::new(), false));
        let (checks, human) = line.rule_checks([&p("docs/x.md")]);
        assert_eq!(checks, ["ci/docs"]);
        assert!(!human);
    }

    #[test]
    fn the_d6_line_is_attested_with_post_land_revert() {
        let line = LineConfig::d6("main");
        assert_eq!(line.policy.name(), "attested");
        assert_eq!(line.authority, Authority::GraftPrimary);
        let post = line.post_land.expect("post-land testing");
        assert!(post.auto_revert);
        assert_eq!(post.checks, ["ci/build", "ci/test"]);
        assert_eq!(LineConfig::direct("dev").policy, LandPolicy::Direct);
    }

    #[test]
    fn path_merge_defaults_and_rules() {
        let mut line = LineConfig::direct("main");
        assert_eq!(line.path_merge(&p("src/a.rs")), PathMerge::Line);
        assert_eq!(line.path_merge(&p("Cargo.lock")), PathMerge::None);
        assert_eq!(
            line.path_merge(&p("web/package-lock.json")),
            PathMerge::None
        );
        assert_eq!(line.path_merge(&p("Cargo.lock.md")), PathMerge::Line);
        let rule = |prefix: &str, merge| PathRule {
            prefix: p(prefix),
            checks: Vec::new(),
            human_review: false,
            merge,
        };
        line.path_rules = vec![
            rule("gen", Some(PathMerge::None)),
            rule("gen/keep", Some(PathMerge::Line)),
            rule("gen/keep/x", None),
            rule("vendor/Cargo.lock", Some(PathMerge::Line)),
        ];
        assert_eq!(line.path_merge(&p("gen/api.ts")), PathMerge::None);
        assert_eq!(line.path_merge(&p("gen/keep/x/a.rs")), PathMerge::Line);
        assert_eq!(line.path_merge(&p("generated.rs")), PathMerge::Line);
        assert_eq!(line.path_merge(&p("vendor/Cargo.lock")), PathMerge::Line);
        assert_eq!(line.path_merge(&p("gen/Cargo.lock")), PathMerge::None);
    }
}

//! Per-line land policy from `server.toml` (`[[lines]]`, `[default_line]`).
//!
//! The engine gates each land by the line's [`LineConfig`]. Without these
//! sections the server runs [`EngineConfig::default`]: every line, `main`
//! included, is `direct` (decision D22: phase 1 is a git alternative, with no
//! build, test or CI in the land path). A `[[lines]]` entry opts its line into
//! `attested`, `queued` or `post_land` checks, the phase-2 setting; decision
//! D6 for `main` (attested on `ci/build` and `ci/test`, post-land with
//! auto-revert) is one such entry and is no longer implicit. Lines no entry
//! names stay `direct`. `[default_line]` replaces the config of every line no
//! entry names.
//!
//! The config is global to the server: every repository sees the same lines.
//! Per-repository configs can come later.

use graft_engine::EngineConfig;
use graft_engine::policy::{Authority, LandPolicy, LineConfig, PathRule, PostLand};
use graft_model::RepoPath;
use serde::Deserialize;

/// The land policy a line gates with.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PolicySpec {
    /// Land at once.
    #[default]
    Direct,
    /// The stack head must carry passing attestations for `checks`.
    Attested,
    /// The rebased candidate is tested before the CAS.
    Queued,
}

/// Which side of a mirrored line accepts writes.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AuthoritySpec {
    /// GitHub is authoritative.
    GitPrimary,
    /// graft is authoritative.
    #[default]
    GraftPrimary,
}

/// A `[[lines]].path_rules` entry.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathRuleSpec {
    pub(crate) prefix: String,
    #[serde(default)]
    pub(crate) checks: Vec<String>,
    #[serde(default)]
    pub(crate) human_review: bool,
}

/// A `post_land` table.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostLandSpec {
    #[serde(default)]
    pub(crate) checks: Vec<String>,
    #[serde(default)]
    pub(crate) auto_revert: bool,
}

/// One `[[lines]]` entry, or the `[default_line]` table (which has no name).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LineSpec {
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) policy: PolicySpec,
    #[serde(default)]
    pub(crate) checks: Vec<String>,
    #[serde(default)]
    pub(crate) attest_merged: bool,
    #[serde(default)]
    pub(crate) authority: AuthoritySpec,
    #[serde(default)]
    pub(crate) path_rules: Vec<PathRuleSpec>,
    pub(crate) post_land: Option<PostLandSpec>,
}

fn check_names(what: &str, checks: &[String]) -> Result<(), String> {
    for (i, c) in checks.iter().enumerate() {
        if c.is_empty() || c.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err(format!("{what}: check {c:?} is not a valid check name"));
        }
        if checks[..i].contains(c) {
            return Err(format!("{what}: check {c:?} is listed twice"));
        }
    }
    Ok(())
}

impl LineSpec {
    /// Refuses invalid combinations. `named` is whether this is a
    /// `[[lines]]` entry (which needs a name) or `[default_line]` (which
    /// must not have one).
    pub(crate) fn check(&self, named: bool) -> Result<(), String> {
        let what = match (&self.name, named) {
            (Some(n), true) => {
                if n.is_empty() || n.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(format!("lines: name {n:?} is not a valid line name"));
                }
                format!("line {n}")
            }
            (None, true) => return Err("lines: every entry needs a name".into()),
            (Some(_), false) => return Err("default_line: name is not allowed".into()),
            (None, false) => "default_line".to_owned(),
        };
        check_names(&what, &self.checks)?;
        match self.policy {
            PolicySpec::Direct => {
                if !self.checks.is_empty() {
                    return Err(format!("{what}: a direct line takes no checks"));
                }
                if self.attest_merged {
                    return Err(format!("{what}: attest_merged needs policy = \"attested\""));
                }
            }
            PolicySpec::Attested => {
                if self.checks.is_empty() {
                    return Err(format!("{what}: an attested line needs checks"));
                }
            }
            PolicySpec::Queued => {
                if self.checks.is_empty() {
                    return Err(format!("{what}: a queued line needs checks"));
                }
                if self.attest_merged {
                    return Err(format!("{what}: attest_merged needs policy = \"attested\""));
                }
            }
        }
        for (i, rule) in self.path_rules.iter().enumerate() {
            let rule_what = format!("{what}: path_rules[{i}]");
            RepoPath::new(&rule.prefix)
                .map_err(|e| format!("{rule_what}: prefix {:?}: {e}", rule.prefix))?;
            check_names(&rule_what, &rule.checks)?;
            if rule.checks.is_empty() && !rule.human_review {
                return Err(format!("{rule_what}: needs checks or human_review"));
            }
        }
        if let Some(post) = &self.post_land {
            check_names(&format!("{what}: post_land"), &post.checks)?;
            if post.checks.is_empty() {
                return Err(format!("{what}: post_land needs checks"));
            }
        }
        Ok(())
    }

    /// The engine's configuration for this line (`name` replaces the spec's).
    /// Call after [`LineSpec::check`].
    fn to_engine(&self, name: &str) -> LineConfig {
        LineConfig {
            name: name.to_owned(),
            policy: match self.policy {
                PolicySpec::Direct => LandPolicy::Direct,
                PolicySpec::Attested => LandPolicy::Attested {
                    checks: self.checks.clone(),
                    attest_merged: self.attest_merged,
                },
                PolicySpec::Queued => LandPolicy::Queued {
                    checks: self.checks.clone(),
                },
            },
            authority: match self.authority {
                AuthoritySpec::GitPrimary => Authority::GitPrimary,
                AuthoritySpec::GraftPrimary => Authority::GraftPrimary,
            },
            path_rules: self
                .path_rules
                .iter()
                .map(|r| PathRule {
                    prefix: RepoPath::new(&r.prefix).expect("checked prefix"),
                    checks: r.checks.clone(),
                    human_review: r.human_review,
                })
                .collect(),
            post_land: self.post_land.as_ref().map(|p| PostLand {
                checks: p.checks.clone(),
                auto_revert: p.auto_revert,
            }),
        }
    }
}

/// Checks the `[[lines]]` and `[default_line]` sections.
pub(crate) fn check(lines: &[LineSpec], default_line: Option<&LineSpec>) -> Result<(), String> {
    for (i, line) in lines.iter().enumerate() {
        line.check(true)?;
        if lines[..i].iter().any(|l| l.name == line.name) {
            return Err(format!(
                "lines: {:?} is configured twice",
                line.name.as_deref().unwrap_or_default()
            ));
        }
    }
    default_line.map_or(Ok(()), |d| d.check(false))
}

/// The engine configuration with the server's line sections applied. With
/// neither section this is exactly [`EngineConfig::default`].
pub(crate) fn engine_config(lines: &[LineSpec], default_line: Option<&LineSpec>) -> EngineConfig {
    let mut config = EngineConfig::default();
    for spec in lines {
        let name = spec.name.as_deref().expect("checked name");
        let line = spec.to_engine(name);
        match config.lines.iter_mut().find(|l| l.name == name) {
            Some(existing) => *existing = line,
            None => config.lines.push(line),
        }
    }
    if let Some(spec) = default_line {
        config.default_line = spec.to_engine("");
    }
    config
}

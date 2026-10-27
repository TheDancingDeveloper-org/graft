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
use graft_engine::policy::{
    Authority, LandPolicy, LineChecks, LineConfig, PathMerge, PathRule, PostLand,
};
use graft_model::RepoPath;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess};

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
    #[serde(default)]
    pub(crate) merge: Option<MergeSpec>,
}

/// How a rebase merges paths under a rule (`merge = "line" | "none"`).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MergeSpec {
    /// Three-way line merge.
    Line,
    /// Any concurrent change is a conflict; the client regenerates.
    None,
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
///
/// `checks` is either the land-policy array (`["ci/fmt", ...]`) or the
/// tracking string `"post-land"`. Those are different fields on the engine
/// (`LandPolicy` versus [`LineChecks`]), so this type deserializes the key
/// itself. An absent `checks` key is the policy default (no checks) and
/// [`LineChecks::Off`].
#[derive(Clone, Debug, Default)]
pub(crate) struct LineSpec {
    pub(crate) name: Option<String>,
    pub(crate) policy: PolicySpec,
    /// Land-policy check names when `checks` is an array.
    pub(crate) checks: Vec<String>,
    /// Non-gating tracking when `checks = "post-land"`.
    pub(crate) tracking: Option<TrackingMode>,
    /// Check names a post-land landing must pass.
    pub(crate) required: Vec<String>,
    /// Failures of one post-land check tolerated before it is `bad`.
    /// Absent means 0. Ignored unless tracking is post-land.
    pub(crate) retries: u32,
    pub(crate) attest_merged: bool,
    pub(crate) authority: AuthoritySpec,
    pub(crate) path_rules: Vec<PathRuleSpec>,
    pub(crate) post_land: Option<PostLandSpec>,
}

/// `checks = "post-land"`: non-gating land-check tracking (D25).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrackingMode {
    /// Each landing gets a `pending` land-checks row.
    PostLand,
}

impl<'de> Deserialize<'de> for LineSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> de::Visitor<'de> for Visit {
            type Value = LineSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a line table")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<LineSpec, A::Error> {
                let mut spec = LineSpec::default();
                let mut seen = std::collections::HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!("duplicate field `{key}`")));
                    }
                    match key.as_str() {
                        "name" => spec.name = Some(map.next_value()?),
                        "policy" => spec.policy = map.next_value()?,
                        "checks" => {
                            let parsed: ChecksField = map.next_value()?;
                            spec.checks = parsed.policy;
                            spec.tracking = parsed.tracking;
                        }
                        "required" => spec.required = map.next_value()?,
                        "retries" => spec.retries = map.next_value()?,
                        "attest_merged" => spec.attest_merged = map.next_value()?,
                        "authority" => spec.authority = map.next_value()?,
                        "path_rules" => spec.path_rules = map.next_value()?,
                        "post_land" => spec.post_land = Some(map.next_value()?),
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &[
                                    "name",
                                    "policy",
                                    "checks",
                                    "required",
                                    "retries",
                                    "attest_merged",
                                    "authority",
                                    "path_rules",
                                    "post_land",
                                ],
                            ));
                        }
                    }
                }
                Ok(spec)
            }
        }
        deserializer.deserialize_struct(
            "LineSpec",
            &[
                "name",
                "policy",
                "checks",
                "required",
                "retries",
                "attest_merged",
                "authority",
                "path_rules",
                "post_land",
            ],
            Visit,
        )
    }
}

/// `checks` is either the historical policy array or the tracking string.
struct ChecksField {
    policy: Vec<String>,
    tracking: Option<TrackingMode>,
}

impl<'de> Deserialize<'de> for ChecksField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> de::Visitor<'de> for Visit {
            type Value = ChecksField;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of check names or the string \"post-land\"")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "post-land" => Ok(ChecksField {
                        policy: Vec::new(),
                        tracking: Some(TrackingMode::PostLand),
                    }),
                    other => Err(E::custom(format!(
                        "unknown checks value {other:?}; expected \"post-land\" or a list"
                    ))),
                }
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut names = Vec::new();
                while let Some(name) = seq.next_element::<String>()? {
                    names.push(name);
                }
                Ok(ChecksField {
                    policy: names,
                    tracking: None,
                })
            }
        }
        deserializer.deserialize_any(Visit)
    }
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
        check_names(&format!("{what}: required"), &self.required)?;
        if self.required.len() > graft_proto::types::MAX_LINE_CHECKS {
            return Err(format!(
                "{what}: required lists more than {} checks",
                graft_proto::types::MAX_LINE_CHECKS
            ));
        }
        match self.tracking {
            Some(TrackingMode::PostLand) => {
                if self.required.is_empty() {
                    return Err(format!(
                        "{what}: checks = \"post-land\" needs a non-empty required list"
                    ));
                }
            }
            None => {
                if !self.required.is_empty() || self.retries != 0 {
                    return Err(format!(
                        "{what}: required and retries need checks = \"post-land\""
                    ));
                }
            }
        }
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
            if rule.checks.is_empty() && !rule.human_review && rule.merge.is_none() {
                return Err(format!("{rule_what}: needs checks, human_review or merge"));
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
                    merge: r.merge.map(|m| match m {
                        MergeSpec::Line => PathMerge::Line,
                        MergeSpec::None => PathMerge::None,
                    }),
                })
                .collect(),
            post_land: self.post_land.as_ref().map(|p| PostLand {
                checks: p.checks.clone(),
                auto_revert: p.auto_revert,
            }),
            checks: match self.tracking {
                None => LineChecks::Off,
                Some(TrackingMode::PostLand) => LineChecks::PostLand {
                    required: self.required.clone(),
                    retries: self.retries,
                },
            },
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

/// `GetLineConfig`: line `req.line`'s check settings, as the engine applies
/// them. The line must exist; resolving it checks read scope.
pub(crate) async fn get_line_config(
    engine: &crate::app::ServerEngine,
    cx: &graft_proto::CallContext,
    req: graft_proto::types::GetLineConfig,
) -> Result<graft_proto::types::LineConfig, graft_proto::ApiError> {
    use graft_proto::RepoApi as _;
    use graft_proto::types::{LineChecksMode, RefKind, ResolveRef};
    let line = engine
        .resolve_ref(
            cx,
            ResolveRef {
                repo: req.repo,
                name: req.line.clone(),
            },
        )
        .await?;
    if line.kind != RefKind::Line {
        return Err(graft_proto::ApiError::NotFound {
            what: format!("line {}", req.line),
        });
    }
    let config = engine.line_config(&req.line);
    let (checks, required, retries) = match config.checks {
        LineChecks::Off => (LineChecksMode::None, Vec::new(), 0),
        LineChecks::PostLand { required, retries } => (LineChecksMode::PostLand, required, retries),
    };
    Ok(graft_proto::types::LineConfig {
        line: req.line,
        checks,
        required,
        retries,
    })
}

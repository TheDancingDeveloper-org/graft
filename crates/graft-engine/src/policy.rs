//! Per-line land policies (`docs/design.md` §8.3, decision D6).

/// How a line gates landing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandPolicy {
    /// Land at once; post-land CI catches breakage.
    Direct,
    /// The stack head must carry passing attestations for these checks.
    Attested {
        /// Required check names.
        checks: Vec<String>,
        /// Whether line-merged results must be re-attested.
        attest_merged: bool,
    },
    /// Test the rebased candidate before the CAS (release lines).
    Queued {
        /// Required check names.
        checks: Vec<String>,
    },
}

/// Which side of a mirrored line accepts writes (`docs/design.md` §11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// GitHub is authoritative; lands go through the PR bridge.
    GitPrimary,
    /// graft is authoritative; GitHub is an exported mirror.
    GraftPrimary,
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
}

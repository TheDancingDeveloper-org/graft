//! How `graft version`'s `+<rev>` suffix is chosen.
//!
//! `build.rs` includes this file. The image build has no `.git`
//! (`.dockerignore` drops it), so CI passes the commit through
//! `GRAFT_BUILD_REV`.

/// `GRAFT_BUILD_REV` when set and non-blank, then `git describe`, then
/// `unknown`. A 40-character hex commit is shortened to eight, matching
/// `git describe --abbrev=8`.
#[allow(unreachable_pub)]
pub fn revision(env_rev: Option<String>, git_rev: Option<String>) -> String {
    env_rev
        .map(|rev| rev.trim().to_owned())
        .map(|rev| short_sha(&rev).unwrap_or(rev))
        .filter(|rev| !rev.is_empty())
        .or(git_rev)
        .unwrap_or_else(|| "unknown".to_owned())
}

fn short_sha(rev: &str) -> Option<String> {
    (rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())).then(|| rev[..8].to_owned())
}

#[cfg(test)]
mod tests {
    use super::revision;

    #[test]
    fn revision_prefers_the_env_var_then_git_then_unknown() {
        assert_eq!(
            revision(Some(" f84f0a8 ".into()), Some("from-git".into())),
            "f84f0a8"
        );
        assert_eq!(
            revision(
                Some("f84f0a8fc07ad7fcdceb49f0fd97d25716eddfb3".into()),
                None
            ),
            "f84f0a8f"
        );
        assert_eq!(
            revision(Some("   ".into()), Some("from-git".into())),
            "from-git"
        );
        assert_eq!(revision(None, Some("from-git".into())), "from-git");
        assert_eq!(revision(None, None), "unknown");
    }
}

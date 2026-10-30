//! Re-landing an outside contributor's GitHub PR through graft, with their
//! credit (WI-2716).
//!
//! A git-primary line lands only through graft, so an outside PR cannot be
//! merged on GitHub as it is. [`import_pr`] fetches the PR read-only into a
//! scratch bare repository, applies its net change (merge base to head) to
//! the files of a graft working copy, and returns the commit message: the
//! PR title as the subject, then a trailer block with `Source-PR: <url>` and
//! `Co-authored-by: <login> <id+login@users.noreply.github.com>`. The project
//! identity stays the author; the contributor is a co-author. The operator
//! then commits and lands as usual.
//!
//! When the batch holding that land merges, the batcher calls
//! [`source_prs`] on each landed commit and closes every source PR of the
//! same repository with [`close_comment`]: GitHub cannot mark the PR merged,
//! because the commit is re-created.
//!
//! PR content is untrusted data: nothing from it runs. git fetches and
//! diffs it with no hooks, diff drivers or textconv, and applies it only
//! under the working copy, never to a `.git` or `.graft` path.

use std::path::Path;

use graft_git::GitSha;
use serde_json::Value;

use crate::MirrorError;
use crate::credentials::CredentialProvider;
use crate::git::Mirror;
use crate::github::HttpForge;
use crate::text::{clean_message, clean_text};

/// The trailer naming the PR a land re-lands.
pub const SOURCE_PR_TRAILER: &str = "Source-PR:";

/// The longest subject kept from a PR title.
const SUBJECT_CHARS: usize = 200;

/// The most paths an imported change may touch.
pub const MAX_PATHS: usize = 5_000;

/// The largest binary patch an import applies.
pub const MAX_PATCH_BYTES: usize = 64 * 1024 * 1024;

/// The local refs a PR's head and base are fetched to.
const HEAD_REF: &str = "refs/graft-mirror/import-pr/head";
const BASE_REF: &str = "refs/graft-mirror/import-pr/base";

fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

/// A PR named as `owner/repo#n` or by its web address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSpec {
    /// The repository owner.
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// The PR number.
    pub number: u64,
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !s.starts_with('.')
}

impl PrSpec {
    /// Parses `owner/repo#n` or `https://github.com/owner/repo/pull/n`.
    pub fn parse(s: &str) -> Option<PrSpec> {
        let s = s.trim();
        let (owner, repo, number) = if let Some(rest) = s.strip_prefix("https://github.com/") {
            let mut parts = rest.trim_end_matches('/').split('/');
            let (owner, repo, pull, n) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            if pull != "pull" || parts.next().is_some() {
                return None;
            }
            (owner, repo, n)
        } else {
            let (path, n) = s.split_once('#')?;
            let (owner, repo) = path.split_once('/')?;
            (owner, repo, n)
        };
        let number = number.parse::<u64>().ok().filter(|n| *n > 0)?;
        (is_slug(owner) && is_slug(repo)).then(|| PrSpec {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number,
        })
    }

    /// The PR's web address.
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repo, self.number
        )
    }
}

/// What the import reads of a PR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePull {
    /// The PR's web address.
    pub url: String,
    /// The PR number.
    pub number: u64,
    /// Its title, as GitHub stores it (cleaned before use).
    pub title: String,
    /// Its head commit.
    pub head: GitSha,
    /// The branch it merges into.
    pub base_ref: String,
    /// The author's GitHub login.
    pub login: String,
    /// The author's numeric GitHub id.
    pub user_id: u64,
    /// Whether it is open.
    pub open: bool,
}

/// Whether `login` is a GitHub login (or an App's `name[bot]`).
fn is_login(login: &str) -> bool {
    let name = login.strip_suffix("[bot]").unwrap_or(login);
    !name.is_empty()
        && name.len() <= 39
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
}

fn branch_ok(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= 255
        && !b.starts_with('-')
        && !b.contains("..")
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

impl SourcePull {
    /// Reads GitHub's REST answer for a PR. Refused when the author's id or
    /// login is missing or malformed: without them the contributor cannot
    /// be credited, and the import stops rather than land without credit.
    pub fn from_json(v: &Value) -> Result<SourcePull, MirrorError> {
        let s = |p: &str| v.pointer(p).and_then(Value::as_str);
        let user_id = v
            .pointer("/user/id")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                config_err("GitHub did not return the PR author's id; cannot credit them")
            })?;
        let login = s("/user/login").filter(|l| is_login(l)).ok_or_else(|| {
            config_err("GitHub did not return a valid PR author login; cannot credit them")
        })?;
        let number = v
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| config_err("GitHub's PR has no number"))?;
        let url = s("/html_url")
            .filter(|u| PrSpec::parse(u).is_some_and(|p| p.number == number))
            .ok_or_else(|| config_err("GitHub's PR has no valid web address"))?;
        let head = s("/head/sha")
            .and_then(GitSha::from_hex)
            .ok_or_else(|| config_err("GitHub's PR has no valid head commit"))?;
        let base_ref = s("/base/ref")
            .filter(|b| branch_ok(b))
            .ok_or_else(|| config_err("GitHub's PR has no valid base branch"))?;
        Ok(SourcePull {
            url: url.to_owned(),
            number,
            title: s("/title").unwrap_or("").to_owned(),
            head,
            base_ref: base_ref.to_owned(),
            login: login.to_owned(),
            user_id,
            open: s("/state") == Some("open"),
        })
    }

    /// The author's GitHub noreply address, which GitHub links to them.
    pub fn noreply_email(&self) -> String {
        format!("{}+{}@users.noreply.github.com", self.user_id, self.login)
    }

    /// The `Co-authored-by` trailer crediting the author.
    pub fn coauthor_trailer(&self) -> String {
        format!("Co-authored-by: {} <{}>", self.login, self.noreply_email())
    }

    /// The re-land's commit message: the PR title as the subject, then one
    /// trailer block with `Source-PR` and `Co-authored-by`. graft-mirror's
    /// batch trailers later join this block (see `land_message`).
    pub fn message(&self) -> String {
        let title = clean_message(&self.title, SUBJECT_CHARS, false);
        let title = title.trim();
        let subject = if title.is_empty() {
            format!("Re-land PR #{}", self.number)
        } else {
            title.to_owned()
        };
        format!(
            "{subject}\n\n{SOURCE_PR_TRAILER} {}\n{}\n",
            self.url,
            self.coauthor_trailer()
        )
    }
}

/// The `Source-PR` addresses in a commit message's final trailer block.
/// A `Source-PR:` line in the body is not a trailer and is ignored.
pub fn source_prs(message: &str) -> Vec<PrSpec> {
    let text = message.replace("\r\n", "\n");
    let Some((_, last)) = text.trim_end().rsplit_once("\n\n") else {
        return Vec::new();
    };
    last.lines()
        .filter_map(|l| l.strip_prefix(SOURCE_PR_TRAILER))
        .filter_map(PrSpec::parse)
        .collect()
}

/// The message of a raw git commit object (after its header).
pub fn commit_message(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    text.split_once("\n\n")
        .map(|(_, m)| m.to_owned())
        .unwrap_or_default()
}

/// The comment left on a source PR when it is closed: where it landed, and
/// that its author is credited. `batch_pr` is the batch PR's number.
pub fn close_comment(base_branch: &str, sha: GitSha, batch_pr: u64) -> String {
    let base = clean_text(base_branch, 100, false);
    format!(
        "Landed on {base} as {} via #{batch_pr}, credited as co-author — thanks!",
        sha.to_hex()
    )
}

/// Codepoints HFS+ ignores in names, so `.g\u{200c}it` opens `.git`.
fn hfs_ignorable(c: char) -> bool {
    matches!(c,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
}

/// Whether one path component names `.git` or `.graft` on some filesystem:
/// ASCII case, HFS+ ignorable codepoints, NTFS trailing dots and spaces,
/// alternate data streams (`.git::$INDEX_ALLOCATION`) and 8.3 short names
/// (`GIT~1`, `GRAFT~1`).
fn metadata_component(c: &str) -> bool {
    let c: String = c.chars().filter(|c| !hfs_ignorable(*c)).collect();
    let c = c.split(':').next().unwrap_or("");
    let c = c.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    if c == ".git" || c == ".graft" {
        return true;
    }
    ["git~", "graft~"].iter().any(|p| {
        c.strip_prefix(p)
            .is_some_and(|n| !n.is_empty() && n.chars().all(|d| d.is_ascii_digit()))
    })
}

/// Whether a path from the PR would write graft's or git's own metadata.
fn forbidden_path(path: &str) -> bool {
    path.split(['/', '\\']).any(metadata_component)
}

/// Refuses a PR whose web address is not the one asked for: GitHub's
/// answer must name the same owner, repository and number as `spec`.
fn check_same_pr(spec: &PrSpec, pull: &SourcePull) -> Result<(), MirrorError> {
    let same = PrSpec::parse(&pull.url).is_some_and(|got| {
        got.owner.eq_ignore_ascii_case(&spec.owner)
            && got.repo.eq_ignore_ascii_case(&spec.repo)
            && got.number == spec.number
    });
    if same {
        Ok(())
    } else {
        Err(config_err(format!(
            "GitHub answered {} for {}; refused",
            clean_text(&pull.url, 200, false),
            spec.url()
        )))
    }
}

/// Imports the PR `spec` into the working copy at `worktree`: reads it
/// from `forge`, fetches its head and base read-only into `mirror` (a
/// new scratch bare repository, which must not exist yet), and applies its
/// net change (merge base to head) to `worktree`'s files. A change of more
/// than [`MAX_PATHS`] paths or [`MAX_PATCH_BYTES`] bytes, or one touching
/// repository metadata, a symlink or a submodule, is refused.
/// Returns the PR and the re-land's message. Nothing is committed.
pub async fn import_pr<C: CredentialProvider>(
    forge: &HttpForge<C>,
    spec: &PrSpec,
    mirror: &Mirror,
    worktree: &Path,
) -> Result<(SourcePull, String), MirrorError> {
    let pull = SourcePull::from_json(&forge.pull_json(spec.number).await?)?;
    check_same_pr(spec, &pull)?;
    if !pull.open {
        return Err(config_err(format!("{} is not open", pull.url)));
    }
    mirror.init_fresh().await?;
    mirror
        .fetch_refs(&[
            (
                format!("refs/pull/{}/head", spec.number),
                HEAD_REF.to_owned(),
            ),
            (format!("refs/heads/{}", pull.base_ref), BASE_REF.to_owned()),
        ])
        .await
        .map_err(|e| MirrorError::Fetch(e.to_string()))?;
    let head = mirror
        .local_ref(HEAD_REF)
        .await?
        .ok_or_else(|| config_err("the PR head was not fetched"))?;
    if head != pull.head {
        return Err(config_err(format!(
            "{} moved while it was read (API {}, fetched {}); run again",
            pull.url,
            pull.head.to_hex(),
            head.to_hex()
        )));
    }
    let base = mirror
        .local_ref(BASE_REF)
        .await?
        .ok_or_else(|| config_err("the PR base was not fetched"))?;
    let from = mirror
        .merge_base(base, head)
        .await?
        .ok_or_else(|| config_err("the PR shares no history with its base"))?;
    let paths = mirror.changed_paths(from, head, MAX_PATHS).await?;
    if paths.is_empty() {
        return Err(config_err(format!("{} changes nothing", pull.url)));
    }
    if let Some(bad) = paths.iter().find(|p| forbidden_path(&p.path)) {
        return Err(config_err(format!(
            "{} writes {:?}, which is repository metadata; refused",
            pull.url,
            clean_text(&bad.path, 200, false)
        )));
    }
    if let Some(bad) = paths.iter().find(|p| p.is_link()) {
        return Err(config_err(format!(
            "{} changes {:?}, a symlink or submodule; refused",
            pull.url,
            clean_text(&bad.path, 200, false)
        )));
    }
    let patch = mirror.binary_diff(from, head, MAX_PATCH_BYTES).await?;
    mirror.apply_to(worktree, &patch).await?;
    let message = pull.message();
    Ok((pull, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pr_json() -> Value {
        json!({
            "number": 296,
            "html_url": "https://github.com/acme/widget/pull/296",
            "title": "Fix the frobnicator",
            "state": "open",
            "user": { "login": "xover-rs", "id": 12345 },
            "head": { "sha": "1111111111111111111111111111111111111111" },
            "base": { "ref": "main" },
        })
    }

    #[test]
    fn specs_parse_short_and_url_forms() {
        let want = PrSpec {
            owner: "acme".into(),
            repo: "widget".into(),
            number: 296,
        };
        assert_eq!(PrSpec::parse("acme/widget#296"), Some(want.clone()));
        assert_eq!(
            PrSpec::parse("https://github.com/acme/widget/pull/296"),
            Some(want.clone())
        );
        assert_eq!(want.url(), "https://github.com/acme/widget/pull/296");
        for bad in [
            "acme#1",
            "acme/widget#0",
            "acme/widget#x",
            "../x/y#1",
            "https://github.com/acme/widget/issues/1",
            "https://evil.example/acme/widget/pull/1",
        ] {
            assert_eq!(PrSpec::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_message_carries_source_pr_and_coauthor_in_one_trailer_block() {
        let pull = SourcePull::from_json(&pr_json()).unwrap();
        assert_eq!(
            pull.noreply_email(),
            "12345+xover-rs@users.noreply.github.com"
        );
        assert_eq!(
            pull.message(),
            "Fix the frobnicator\n\n\
             Source-PR: https://github.com/acme/widget/pull/296\n\
             Co-authored-by: xover-rs <12345+xover-rs@users.noreply.github.com>\n"
        );
        // The block survives the batch trailers joining it.
        assert!(crate::batch::ends_in_trailers(pull.message().trim_end()));
        assert_eq!(
            source_prs(&pull.message()),
            vec![PrSpec::parse("acme/widget#296").unwrap()]
        );
    }

    #[test]
    fn an_untrusted_title_is_cleaned_and_an_empty_one_replaced() {
        let mut v = pr_json();
        v["title"] = json!("Evil\ntitle\u{202E}\u{7}");
        let pull = SourcePull::from_json(&v).unwrap();
        assert!(
            pull.message().starts_with("Eviltitle\n\n"),
            "{}",
            pull.message()
        );
        v["title"] = json!("   ");
        let pull = SourcePull::from_json(&v).unwrap();
        assert!(pull.message().starts_with("Re-land PR #296\n\n"));
    }

    #[test]
    fn a_missing_author_id_is_refused_rather_than_landed_without_credit() {
        let mut v = pr_json();
        v["user"] = json!({ "login": "xover-rs" });
        let err = SourcePull::from_json(&v).unwrap_err().to_string();
        assert!(err.contains("author's id"), "{err}");
        let mut v = pr_json();
        v["user"] = json!({ "login": "bad login <x@y>", "id": 1 });
        let err = SourcePull::from_json(&v).unwrap_err().to_string();
        assert!(err.contains("login"), "{err}");
        let mut v = pr_json();
        v.as_object_mut().unwrap().remove("user");
        assert!(SourcePull::from_json(&v).is_err());
    }

    #[test]
    fn bot_logins_are_credited() {
        let mut v = pr_json();
        v["user"] = json!({ "login": "dependabot[bot]", "id": 49699333 });
        let pull = SourcePull::from_json(&v).unwrap();
        assert_eq!(
            pull.coauthor_trailer(),
            "Co-authored-by: dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>"
        );
    }

    #[test]
    fn source_prs_reads_only_the_final_trailer_block() {
        let body = "Subject\n\nSource-PR: https://github.com/acme/widget/pull/1\n\nMore text\n";
        assert!(source_prs(body).is_empty());
        let landed = "Subject\r\n\r\nSource-PR: https://github.com/acme/widget/pull/7\r\n\
                      Co-authored-by: a <1+a@users.noreply.github.com>\r\n\
                      Graft-Changeset: cs\r\nGraft-Batch: 3\r\n";
        assert_eq!(
            source_prs(landed),
            vec![PrSpec::parse("acme/widget#7").unwrap()]
        );
        assert!(source_prs("Subject only").is_empty());
    }

    #[test]
    fn a_pr_answered_for_another_repository_is_refused() {
        let spec = PrSpec::parse("acme/widget#296").unwrap();
        let pull = SourcePull::from_json(&pr_json()).unwrap();
        assert!(check_same_pr(&spec, &pull).is_ok());
        let mut v = pr_json();
        v["html_url"] = json!("https://github.com/evil/widget/pull/296");
        let pull = SourcePull::from_json(&v).unwrap();
        let err = check_same_pr(&spec, &pull).unwrap_err().to_string();
        assert!(err.contains("evil/widget"), "{err}");
        let other = PrSpec::parse("acme/gadget#296").unwrap();
        assert!(check_same_pr(&other, &SourcePull::from_json(&pr_json()).unwrap()).is_err());
    }

    #[test]
    fn the_close_comment_names_the_sha_and_batch() {
        assert_eq!(
            close_comment("main", GitSha([0xab; 20]), 302),
            format!(
                "Landed on main as {} via #302, credited as co-author — thanks!",
                "ab".repeat(20)
            )
        );
    }

    #[test]
    fn metadata_paths_are_forbidden() {
        assert!(forbidden_path(".git/hooks/pre-commit"));
        assert!(forbidden_path("sub/.GRAFT/state"));
        for bad in [
            ".g\u{200c}it/config",
            "\u{feff}.graft/state",
            ".git./config",
            ".graft. /state",
            ".git::$INDEX_ALLOCATION/config",
            "GIT~1/config",
            "a/GRAFT~1/state",
            "a\\.git\\config",
        ] {
            assert!(forbidden_path(bad), "{bad:?}");
        }
        assert!(!forbidden_path("git~x/file"));
        assert!(!forbidden_path("a/graft~/file"));
        assert!(!forbidden_path("src/.gitignore"));
        assert!(!forbidden_path("docs/graft.md"));
    }
}

//! The git CLI, for what talks to the upstream: `ls-remote`, `fetch` into the
//! local bare mirror, and ancestry checks (`docs/design.md` §11.1 step 1).
//! Reading the mirror for import is `graft-git`'s job.
//!
//! Every command runs with a timeout, without a terminal prompt, and is killed
//! if the caller gives up. The upstream URL is never logged (it may carry a
//! credential); errors name the command and git's first stderr line only.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use graft_git::GitSha;

use crate::credentials::SecretToken;
use tokio::process::Command;

/// The longest any one git command may run.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// The ref namespace in the bare mirror that fetch writes upstream branches
/// to. Kept apart from `refs/heads/` so a force-push upstream never rewrites
/// anything but this copy.
pub const UPSTREAM_NS: &str = "refs/graft-mirror/upstream/";

/// The ref namespace in the bare mirror that follow fetches upstream tags
/// to, for their tag objects.
pub const TAGS_NS: &str = "refs/graft-mirror/tags/";

/// The deepest tag-of-tag chain follow records.
pub const TAG_CHAIN_MAX: usize = 16;

/// One upstream tag, as `git ls-remote` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamTag {
    /// The name, without `refs/tags/`.
    pub name: String,
    /// The commit it peels to.
    pub commit: GitSha,
    /// The object the ref names: `commit` for a lightweight tag, else the
    /// outermost tag object.
    pub object: GitSha,
}

/// Set to `1` in the environment of every push the mirror makes. The local
/// stand-in for GitHub's ruleset ([`crate::authority::HookGate`]) lets only
/// such pushes through on a graft-primary branch. It is not a credential:
/// the real gate on GitHub is the ruleset's bypass for the mirror's App.
pub const MIRROR_PUSH_ENV: &str = "GRAFT_MIRROR_PUSH";

/// One entry of a tree diff: its path and its git modes before and after
/// (`000000` where it is absent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    /// The path, `/`-separated.
    pub path: String,
    /// The mode in the old tree.
    pub old_mode: String,
    /// The mode in the new tree.
    pub new_mode: String,
}

impl ChangedPath {
    /// Whether either side is a symlink (`120000`) or a gitlink (`160000`).
    pub fn is_link(&self) -> bool {
        [&self.old_mode, &self.new_mode]
            .iter()
            .any(|m| *m == "120000" || *m == "160000")
    }
}

/// A git CLI failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCliError(pub String);

impl std::fmt::Display for GitCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitCliError {}

/// What a push that did not force came back as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// The branch now names the commit.
    Pushed,
    /// The branch holds other commits (or moved): nothing was changed.
    Rejected,
}

/// Whether a failed push's lowercased stderr says upstream's branch holds
/// other commits: a lease that no longer matches, or a non-fast-forward.
/// A `[remote rejected]` for any other reason (a ref lock held by upstream's
/// own housekeeping, a hook) says nothing about the branch, so it stays an
/// error for the caller to retry, never a divergence.
fn is_rejection(stderr: &str) -> bool {
    stderr.contains("[rejected]")
        || stderr.contains("stale info")
        || stderr.contains("non-fast-forward")
}

/// The local bare mirror of one upstream.
#[derive(Clone, Debug)]
pub struct Mirror {
    dir: PathBuf,
    upstream: String,
    /// The HTTPS credential every network command carries (WI-2571).
    credential: Option<SecretToken>,
    /// Whether git ignores the user's and the system's config here (the
    /// import-pr scratch repository, WI-2716). See [`Mirror::scratch`].
    isolated: bool,
}

/// The exit status and stdout of a finished command.
struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn git(dir: Option<&Path>, args: &[&str]) -> Result<Output, GitCliError> {
    git_env(dir, args, &[]).await
}

/// [`git`] with extra environment variables. Secrets go here, never in argv.
async fn git_env(
    dir: Option<&Path>,
    args: &[&str],
    env: &[(String, String)],
) -> Result<Output, GitCliError> {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let what = args.first().copied().unwrap_or("git");
    let child = cmd
        .spawn()
        .map_err(|e| GitCliError(format!("git {what}: cannot run git: {e}")))?;
    let out = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| GitCliError(format!("git {what}: timed out")))?
        .map_err(|e| GitCliError(format!("git {what}: {e}")))?;
    Ok(Output {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// Runs `git` in `dir` with `input` on stdin and `env` added; succeeds only
/// on exit 0.
async fn git_input(
    dir: &Path,
    args: &[&str],
    input: &[u8],
    env: &[(String, String)],
) -> Result<Vec<u8>, GitCliError> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let what = args.first().copied().unwrap_or("git");
    let mut child = cmd
        .spawn()
        .map_err(|e| GitCliError(format!("git {what}: cannot run git: {e}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitCliError(format!("git {what}: no stdin")))?;
    let run = async {
        stdin.write_all(input).await?;
        drop(stdin);
        child.wait_with_output().await
    };
    let out = tokio::time::timeout(GIT_TIMEOUT, run)
        .await
        .map_err(|_| GitCliError(format!("git {what}: timed out")))?
        .map_err(|e| GitCliError(format!("git {what}: {e}")))?;
    let out = Output {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    };
    if out.code == Some(0) {
        Ok(out.stdout)
    } else {
        Err(failed(what, &out))
    }
}

fn failed(what: &str, out: &Output) -> GitCliError {
    let line = String::from_utf8_lossy(&out.stderr);
    let line: String = line
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    GitCliError(format!("git {what} failed ({:?}): {line}", out.code))
}

async fn ok(dir: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, GitCliError> {
    ok_env(dir, args, &[]).await
}

/// [`ok`] with extra environment variables.
async fn ok_env(
    dir: Option<&Path>,
    args: &[&str],
    env: &[(String, String)],
) -> Result<Vec<u8>, GitCliError> {
    let out = git_env(dir, args, env).await?;
    if out.code == Some(0) {
        Ok(out.stdout)
    } else {
        Err(failed(args.first().copied().unwrap_or("git"), &out))
    }
}

impl Mirror {
    /// Clones `url` as a bare mirror into `dir`.
    ///
    /// `url` is passed to git as an argument, never rewritten with a
    /// credential. Callers refuse userinfo first, so a password cannot
    /// arrive here or land in git's error text.
    pub async fn clone_mirror(url: &str, dir: &Path) -> Result<(), GitCliError> {
        if dir.exists() {
            return Err(GitCliError(format!(
                "clone destination {} already exists",
                dir.display()
            )));
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| GitCliError(format!("creating {}: {e}", parent.display())))?;
        }
        ok(
            None,
            &["clone", "--mirror", "--", url, &dir.display().to_string()],
        )
        .await?;
        Ok(())
    }

    /// The mirror at `dir` of `upstream` (a URL or a local path).
    pub fn new(dir: impl Into<PathBuf>, upstream: impl Into<String>) -> Mirror {
        Mirror {
            dir: dir.into(),
            upstream: upstream.into(),
            credential: None,
            isolated: false,
        }
    }

    /// The mirror at `dir` for a scratch repository that reads untrusted
    /// content (import-pr). Its git commands ignore the user's and the
    /// system's config (`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL`), so a
    /// `url.<x>.insteadOf`, an `includeIf` or a credential helper there
    /// cannot redirect a fetch or see the token. `HOME` is unchanged, and
    /// the token travels in its own `GIT_CONFIG_*` variables, which this
    /// does not touch (WI-2716).
    pub fn scratch(dir: impl Into<PathBuf>, upstream: impl Into<String>) -> Mirror {
        Mirror {
            isolated: true,
            ..Mirror::new(dir, upstream)
        }
    }

    /// The environment that keeps this mirror's git off the user's and the
    /// system's config: empty unless it is a [`Mirror::scratch`].
    fn isolation_env(&self) -> Vec<(String, String)> {
        if !self.isolated {
            return Vec::new();
        }
        vec![
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ]
    }

    /// This mirror, with `credential` sent on every git network command
    /// (ls-remote, fetch and push) to an HTTPS upstream. It travels in the
    /// environment as a git `http.extraHeader`, never in argv (WI-2571).
    pub fn with_credential(mut self, credential: Option<SecretToken>) -> Mirror {
        self.credential = credential;
        self
    }

    /// Whether the upstream is reached over HTTPS (where a token is needed).
    pub fn is_https(&self) -> bool {
        self.upstream.starts_with("https://")
    }

    /// The bare repository's directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The mirror ref fetch writes `branch` to.
    pub fn upstream_ref(branch: &str) -> String {
        format!("{UPSTREAM_NS}{branch}")
    }

    /// Creates the bare repository if it does not exist yet.
    pub async fn ensure(&self) -> Result<(), GitCliError> {
        if self.dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| GitCliError(format!("creating the mirror directory: {e}")))?;
        ok(Some(&self.dir), &["init", "-q", "--bare"]).await?;
        Ok(())
    }

    /// Creates the bare repository in a new directory, made exclusively with
    /// mode 0700: an existing directory is an error and is never reused, so
    /// nobody else can plant its config. Hooks and fsmonitor are switched
    /// off in its own config, which overrides the user's and the system's.
    /// For scratch repositories that read untrusted content.
    pub async fn init_fresh(&self) -> Result<(), GitCliError> {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&self.dir).map_err(|e| {
            GitCliError(format!(
                "creating the scratch repository {}: {e}",
                self.dir.display()
            ))
        })?;
        let env = self.isolation_env();
        ok_env(Some(&self.dir), &["init", "-q", "--bare"], &env).await?;
        for (key, value) in [("core.hooksPath", "/dev/null"), ("core.fsmonitor", "false")] {
            ok_env(Some(&self.dir), &["config", key, value], &env).await?;
        }
        Ok(())
    }

    /// `git ls-remote` for `branches`: each branch's upstream commit, or
    /// `None` when upstream has no such branch.
    pub async fn ls_remote(
        &self,
        branches: &[&str],
    ) -> Result<Vec<(String, Option<GitSha>)>, GitCliError> {
        let refs: Vec<String> = branches.iter().map(|b| format!("refs/heads/{b}")).collect();
        let mut args = vec!["ls-remote", self.upstream.as_str()];
        args.extend(refs.iter().map(String::as_str));
        let out = self.remote(&args).await?;
        let text = String::from_utf8_lossy(&out);
        let mut found = Vec::new();
        for line in text.lines() {
            let mut parts = line.split('\t');
            let (Some(sha), Some(name)) = (parts.next(), parts.next()) else {
                continue;
            };
            if let Some(sha) = GitSha::from_hex(sha) {
                found.push((name.to_owned(), sha));
            }
        }
        Ok(branches
            .iter()
            .zip(&refs)
            .map(|(b, r)| {
                let sha = found.iter().find(|(n, _)| n == r).map(|(_, s)| *s);
                ((*b).to_owned(), sha)
            })
            .collect())
    }

    /// `git ls-remote` for upstream tags matching `pattern` under
    /// `refs/tags/`, sorted by name: each tag's commit (peeled through
    /// annotated tags) and the object its ref names.
    pub async fn ls_remote_tags(&self, pattern: &str) -> Result<Vec<UpstreamTag>, GitCliError> {
        let spec = format!("refs/tags/{pattern}");
        let out = self
            .remote(&["ls-remote", "--tags", self.upstream.as_str(), &spec])
            .await?;
        let text = String::from_utf8_lossy(&out);
        let mut objects: std::collections::BTreeMap<String, GitSha> = Default::default();
        let mut peeled: std::collections::HashMap<String, GitSha> = Default::default();
        for line in text.lines() {
            let mut parts = line.split('\t');
            let (Some(sha), Some(name)) = (parts.next(), parts.next()) else {
                continue;
            };
            let (Some(sha), Some(name)) = (GitSha::from_hex(sha), name.strip_prefix("refs/tags/"))
            else {
                continue;
            };
            match name.strip_suffix("^{}") {
                Some(base) => peeled.insert(base.to_owned(), sha),
                None => objects.insert(name.to_owned(), sha),
            };
        }
        Ok(objects
            .into_iter()
            .map(|(name, object)| UpstreamTag {
                commit: peeled.get(&name).copied().unwrap_or(object),
                name,
                object,
            })
            .collect())
    }

    /// Fetches upstream's `refs/tags/<name>` for each of `names` into
    /// [`TAGS_NS`], forced, so their tag objects are in the mirror.
    pub async fn fetch_tags(&self, names: &[&str]) -> Result<(), GitCliError> {
        let refs: Vec<(String, String)> = names
            .iter()
            .map(|n| (format!("refs/tags/{n}"), format!("{TAGS_NS}{n}")))
            .collect();
        self.fetch_refs(&refs).await
    }

    /// The raw tag objects from `object` down to the first non-tag, outermost
    /// first: empty when `object` is not a tag. At most [`TAG_CHAIN_MAX`]
    /// deep (rule 7); `None` for a longer chain.
    pub async fn tag_chain(&self, object: GitSha) -> Result<Option<Vec<Vec<u8>>>, GitCliError> {
        let mut chain = Vec::new();
        let mut sha = object;
        loop {
            let hex = sha.to_hex();
            let kind = ok(Some(&self.dir), &["cat-file", "-t", &hex]).await?;
            if kind.trim_ascii() != b"tag" {
                return Ok(Some(chain));
            }
            if chain.len() == TAG_CHAIN_MAX {
                return Ok(None);
            }
            let data = ok(Some(&self.dir), &["cat-file", "tag", &hex]).await?;
            let next = data
                .strip_prefix(b"object ")
                .and_then(|l| l.get(..40))
                .and_then(|l| std::str::from_utf8(l).ok())
                .and_then(GitSha::from_hex);
            chain.push(data);
            match next {
                Some(n) => sha = n,
                None => return Ok(Some(chain)),
            }
        }
    }

    /// Fetches `branches` from the configured upstream into [`UPSTREAM_NS`],
    /// forced: the copy always equals upstream, and follow decides what
    /// graft may take.
    pub async fn fetch(&self, branches: &[&str]) -> Result<(), GitCliError> {
        let specs: Vec<String> = branches
            .iter()
            .map(|b| format!("+refs/heads/{b}:{}", Mirror::upstream_ref(b)))
            .collect();
        let mut args = vec!["fetch", "-q", "--no-tags", self.upstream.as_str()];
        args.extend(specs.iter().map(String::as_str));
        self.remote(&args).await?;
        Ok(())
    }

    /// Pushes `sha` to `refs/heads/<branch>` on upstream, never forced. A
    /// branch that already holds other commits is [`PushOutcome::Rejected`],
    /// and the caller decides. A bearer token (for HTTPS upstreams) travels
    /// in the environment as a git `http.extraHeader`, never in argv.
    pub async fn push_branch(
        &self,
        branch: &str,
        sha: GitSha,
        bearer: Option<&str>,
    ) -> Result<PushOutcome, GitCliError> {
        let spec = format!("{}:refs/heads/{branch}", sha.to_hex());
        let out = git_env(
            Some(&self.dir),
            &["push", "-q", "--no-force", self.upstream.as_str(), &spec],
            &self.auth_env(bearer),
        )
        .await?;
        if out.code == Some(0) {
            return Ok(PushOutcome::Pushed);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if is_rejection(&stderr) {
            return Ok(PushOutcome::Rejected);
        }
        Err(failed("push", &out))
    }

    /// Pushes `sha` to `refs/heads/<branch>` on upstream with a lease
    /// (`docs/design.md` §11.3): the push happens only if upstream's branch
    /// is `expected` now (`None`: the branch must not exist). That is a
    /// compare-and-swap on upstream's side, never a blind force. Anything
    /// else upstream holds is [`PushOutcome::Rejected`], and nothing changes.
    pub async fn push_lease(
        &self,
        branch: &str,
        sha: GitSha,
        expected: Option<GitSha>,
        bearer: Option<&str>,
    ) -> Result<PushOutcome, GitCliError> {
        let target = format!("refs/heads/{branch}");
        let lease = format!(
            "--force-with-lease={target}:{}",
            expected.map(|s| s.to_hex()).unwrap_or_default()
        );
        let spec = format!("{}:{target}", sha.to_hex());
        let out = git_env(
            Some(&self.dir),
            &["push", "-q", &lease, self.upstream.as_str(), &spec],
            &self.auth_env(bearer),
        )
        .await?;
        if out.code == Some(0) {
            return Ok(PushOutcome::Pushed);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if is_rejection(&stderr) {
            return Ok(PushOutcome::Rejected);
        }
        Err(failed("push", &out))
    }

    /// Deletes `refs/heads/<branch>` on upstream with a lease: only if it
    /// names `expected` now. A branch that is absent or names another
    /// commit is [`PushOutcome::Rejected`], and nothing changes. The bearer
    /// travels as in [`Mirror::push_lease`], never in argv.
    pub async fn delete_lease(
        &self,
        branch: &str,
        expected: GitSha,
        bearer: Option<&str>,
    ) -> Result<PushOutcome, GitCliError> {
        let target = format!("refs/heads/{branch}");
        let lease = format!("--force-with-lease={target}:{}", expected.to_hex());
        let spec = format!(":{target}");
        let out = git_env(
            Some(&self.dir),
            &["push", "-q", &lease, self.upstream.as_str(), &spec],
            &self.auth_env(bearer),
        )
        .await?;
        if out.code == Some(0) {
            return Ok(PushOutcome::Pushed);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if is_rejection(&stderr) {
            return Ok(PushOutcome::Rejected);
        }
        Err(failed("push", &out))
    }

    /// Fetches `(remote ref, local ref)` pairs from upstream, forced into the
    /// local refs, which must sit under the mirror's own namespace.
    pub async fn fetch_refs(&self, refs: &[(String, String)]) -> Result<(), GitCliError> {
        let specs: Vec<String> = refs
            .iter()
            .map(|(remote, local)| format!("+{remote}:{local}"))
            .collect();
        let mut args = vec!["fetch", "-q", "--no-tags", self.upstream.as_str()];
        args.extend(specs.iter().map(String::as_str));
        self.remote(&args).await?;
        Ok(())
    }

    /// The best common ancestor of `a` and `b`, if they share history.
    pub async fn merge_base(&self, a: GitSha, b: GitSha) -> Result<Option<GitSha>, GitCliError> {
        let (a, b) = (a.to_hex(), b.to_hex());
        let out = git_env(
            Some(&self.dir),
            &["merge-base", "--end-of-options", &a, &b],
            &self.isolation_env(),
        )
        .await?;
        if out.code != Some(0) {
            return Ok(None);
        }
        Ok(GitSha::from_hex(
            String::from_utf8_lossy(&out.stdout).trim(),
        ))
    }

    /// The entries that differ between `from` and `to`, at most
    /// `max_paths` of them; more is an error. No diff driver or textconv
    /// runs: the trees may be untrusted.
    pub async fn changed_paths(
        &self,
        from: GitSha,
        to: GitSha,
        max_paths: usize,
    ) -> Result<Vec<ChangedPath>, GitCliError> {
        let (from, to) = (from.to_hex(), to.to_hex());
        let out = ok_env(
            Some(&self.dir),
            &[
                "diff",
                "--raw",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--end-of-options",
                &from,
                &to,
            ],
            &self.isolation_env(),
        )
        .await?;
        let malformed = || GitCliError("git diff: malformed --raw output".to_owned());
        let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
        let mut paths = Vec::new();
        while let Some(meta) = fields.next() {
            let path = fields.next().ok_or_else(malformed)?;
            if paths.len() == max_paths {
                return Err(GitCliError(format!(
                    "the change touches more than {max_paths} paths; refused"
                )));
            }
            let meta = String::from_utf8_lossy(meta);
            let mut parts = meta.trim_start_matches(':').split(' ');
            let (old_mode, new_mode) = (
                parts.next().ok_or_else(malformed)?.to_owned(),
                parts.next().ok_or_else(malformed)?.to_owned(),
            );
            paths.push(ChangedPath {
                path: String::from_utf8_lossy(path).into_owned(),
                old_mode,
                new_mode,
            });
        }
        Ok(paths)
    }

    /// The binary patch from `from` to `to`, with no diff driver or
    /// textconv: the trees may be untrusted. A patch over `max_bytes` is
    /// an error.
    pub async fn binary_diff(
        &self,
        from: GitSha,
        to: GitSha,
        max_bytes: usize,
    ) -> Result<Vec<u8>, GitCliError> {
        let (from, to) = (from.to_hex(), to.to_hex());
        let patch = ok_env(
            Some(&self.dir),
            &[
                "diff",
                "--binary",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--end-of-options",
                &from,
                &to,
            ],
            &self.isolation_env(),
        )
        .await?;
        if patch.len() > max_bytes {
            return Err(GitCliError(format!(
                "the change's patch is {} bytes, over the {max_bytes}-byte cap; refused",
                patch.len()
            )));
        }
        Ok(patch)
    }

    /// Applies `patch` to the files under `worktree` (not to any index),
    /// using this bare repository only as git's directory, so no repository
    /// that `worktree` sits in is touched. git refuses paths outside the
    /// tree and writes through symlinks.
    pub async fn apply_to(&self, worktree: &Path, patch: &[u8]) -> Result<(), GitCliError> {
        let git_dir = format!("--git-dir={}", self.dir.display());
        let work_tree = format!("--work-tree={}", worktree.display());
        git_input(
            worktree,
            &[&git_dir, &work_tree, "apply", "--binary", "-"],
            patch,
            &self.isolation_env(),
        )
        .await?;
        Ok(())
    }

    /// The commit a local ref of the mirror names, if any.
    pub async fn local_ref(&self, name: &str) -> Result<Option<GitSha>, GitCliError> {
        let spec = format!("{name}^{{commit}}");
        let out = git_env(
            Some(&self.dir),
            &["rev-parse", "-q", "--verify", "--end-of-options", &spec],
            &self.isolation_env(),
        )
        .await?;
        if out.code != Some(0) {
            return Ok(None);
        }
        Ok(GitSha::from_hex(
            String::from_utf8_lossy(&out.stdout).trim(),
        ))
    }

    /// Runs a read-only network command (ls-remote, fetch) against
    /// upstream with the mirror's credential; succeeds only on exit 0.
    async fn remote(&self, args: &[&str]) -> Result<Vec<u8>, GitCliError> {
        let mut env = self.credential_env(None);
        env.extend(self.isolation_env());
        let out = git_env(Some(&self.dir), args, &env).await?;
        if out.code == Some(0) {
            Ok(out.stdout)
        } else {
            Err(failed(args.first().copied().unwrap_or("git"), &out))
        }
    }

    /// The environment of a push: [`MIRROR_PUSH_ENV`], and the credential.
    fn auth_env(&self, bearer: Option<&str>) -> Vec<(String, String)> {
        let mut env = vec![(MIRROR_PUSH_ENV.to_owned(), "1".to_owned())];
        env.extend(self.credential_env(bearer));
        env
    }

    /// The `http.extraHeader` for an HTTPS upstream: `bearer` if given,
    /// else the mirror's own credential. Every network command builds its
    /// credential here, so follow, export and pr-batch agree (WI-2571).
    fn credential_env(&self, bearer: Option<&str>) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(token) = bearer.or(self.credential.as_ref().map(SecretToken::expose))
            && self.is_https()
        {
            let basic =
                base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
            env.push(("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()));
            env.push(("GIT_CONFIG_KEY_0".to_owned(), "http.extraHeader".to_owned()));
            env.push((
                "GIT_CONFIG_VALUE_0".to_owned(),
                format!("Authorization: Basic {basic}"),
            ));
        }
        env
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors. A commit
    /// the mirror does not hold is not an ancestor.
    pub async fn is_ancestor(
        &self,
        ancestor: GitSha,
        descendant: GitSha,
    ) -> Result<bool, GitCliError> {
        let (a, d) = (ancestor.to_hex(), descendant.to_hex());
        let out = git(Some(&self.dir), &["merge-base", "--is-ancestor", &a, &d]).await?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            // 128: an object is missing (upstream rewrote history the mirror
            // never fetched): not an ancestor.
            Some(128) if !self.has_commit(ancestor).await? => Ok(false),
            _ => Err(failed("merge-base", &out)),
        }
    }

    /// The root tree of commit `sha` in the mirror.
    pub async fn commit_tree(&self, sha: GitSha) -> Result<GitSha, GitCliError> {
        let spec = format!("{}^{{tree}}", sha.to_hex());
        let out = ok(
            Some(&self.dir),
            &["rev-parse", "-q", "--verify", "--end-of-options", &spec],
        )
        .await?;
        GitSha::from_hex(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| GitCliError("git rev-parse: unexpected output".to_owned()))
    }

    /// Stores the raw commit object `raw` in the mirror and returns its sha.
    /// The bytes are written as given, so the sha depends on nothing but
    /// them (no git configuration, identity or signing).
    pub async fn write_commit(&self, raw: &[u8]) -> Result<GitSha, GitCliError> {
        let out = git_input(
            &self.dir,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            raw,
            &[],
        )
        .await?;
        GitSha::from_hex(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| GitCliError("git hash-object: unexpected output".to_owned()))
    }

    /// The raw commit object `sha` in the mirror (headers, a blank line,
    /// the message).
    pub async fn read_commit(&self, sha: GitSha) -> Result<Vec<u8>, GitCliError> {
        ok(Some(&self.dir), &["cat-file", "commit", &sha.to_hex()]).await
    }

    /// Whether the mirror holds `sha` as a commit.
    pub async fn has_commit(&self, sha: GitSha) -> Result<bool, GitCliError> {
        let spec = format!("{}^{{commit}}", sha.to_hex());
        let out = git(Some(&self.dir), &["cat-file", "-e", &spec]).await?;
        Ok(out.code == Some(0))
    }

    /// Commits reachable from `to` and not from `from`
    /// (`git rev-list --count`): what graft lacks. O(the new commits).
    pub async fn count_between(
        &self,
        from: Option<GitSha>,
        to: GitSha,
    ) -> Result<u64, GitCliError> {
        let range = match from {
            Some(f) => format!("{}..{}", f.to_hex(), to.to_hex()),
            None => to.to_hex(),
        };
        let out = ok(Some(&self.dir), &["rev-list", "--count", &range]).await?;
        String::from_utf8_lossy(&out)
            .trim()
            .parse()
            .map_err(|_| GitCliError("git rev-list: unexpected output".to_owned()))
    }
}

//! `graft-mirror import-config` (D41, `docs/notes/mirror-service.md` §6
//! step 3): moves today's per-repository `follow` configs onto the server's
//! mirror rows, so one `graft-mirror serve` runs them all.
//!
//! For each `mirror-<repo>.toml`:
//!
//! 1. Build the row the config describes and compare it with the server's
//!    row for `(repo, upstream)`. No row: create it (`SetMirror`, which
//!    needs `mirror:bind`). An identical row is left alone. A differing row
//!    is reported, field by field, and neither it nor its directory is
//!    touched: the operator settles it with `graft repo mirror set`.
//! 2. Seed the row's credential from `<credential-dir>/<repo>` through
//!    `SetMirrorCredential`, when the file exists and the row has none. The
//!    value is read from the file, never argv, and is never printed: the
//!    report names the file only.
//! 3. Adopt `<data-dir>/<repo>/`: rename (move, never copy) the old
//!    `<repo>.git` and `<repo>.sqlite` to the service's names, then write
//!    the migration marker naming the repository. `serve` adopts no
//!    directory without a marker, and refuses one whose marker names
//!    another repository, so a directory copied under another name cannot
//!    run. A copy under the same name is fenced by the repository's lease:
//!    only one process holds it.
//!
//! A second run is a no-op. `--dry-run` reads the server and the files'
//! presence and prints the same report, changing nothing.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use graft_proto::ApiError;
use graft_proto::error::ApiResult;
use graft_proto::pb;

use crate::MirrorError;
use crate::config::FileConfig;
use crate::credentials::SecretToken;
use crate::follow::POLL_INTERVAL;
use crate::service::{DB_FILE, GIT_DIR, MIGRATION_MARKER, marker_repo};

/// The server calls `import-config` makes, as the operator.
pub trait MirrorAdmin {
    /// The row for `(repo, upstream)`, or `None` if there is none.
    fn get_mirror(
        &self,
        repo: &str,
        upstream: &str,
    ) -> impl Future<Output = ApiResult<Option<pb::Mirror>>>;
    /// Creates or replaces a row's settings.
    fn set_mirror(&self, req: pb::SetMirrorRequest) -> impl Future<Output = ApiResult<pb::Mirror>>;
    /// Seals `credential` into the row at `expected_version`.
    fn set_credential(
        &self,
        repo: &str,
        upstream: &str,
        credential: &SecretToken,
        expected_version: u64,
    ) -> impl Future<Output = ApiResult<pb::Mirror>>;
}

/// One config to import.
#[derive(Clone, Debug)]
pub struct Planned {
    /// The config file.
    pub source: PathBuf,
    /// The row it describes.
    pub row: pb::SetMirrorRequest,
    /// `<credential-dir>/<repo>`, when a credential dir was given.
    pub credential: Option<PathBuf>,
    /// `<data-dir>/<repo>`.
    pub dir: PathBuf,
    /// The old bare mirror's and DB's file names in `dir`.
    pub old_git: Option<String>,
    /// See [`old_git`](Self::old_git).
    pub old_db: Option<String>,
}

/// Reads each config and plans its row. Two configs for one repository
/// are refused: the service runs one row per repository.
pub fn plan(
    configs: &[PathBuf],
    data_dir: &Path,
    credential_dir: Option<&Path>,
) -> Result<Vec<Planned>, MirrorError> {
    let mut out: Vec<Planned> = Vec::new();
    for source in configs {
        let file = FileConfig::load(source)?;
        if !crate::service::safe_dir_name(&file.repo) {
            return Err(MirrorError::Config(format!(
                "{}: repo {:?} is not a safe directory name",
                source.display(),
                file.repo
            )));
        }
        if out.iter().any(|p| p.row.repo == file.repo) {
            return Err(MirrorError::Config(format!(
                "{}: a second config for repo {}",
                source.display(),
                file.repo
            )));
        }
        let interval = file
            .poll_interval_secs
            .unwrap_or(POLL_INTERVAL.as_secs())
            .try_into()
            .map_err(|_| MirrorError::Config("poll_interval_secs is too large".into()))?;
        let lines = file
            .lines
            .iter()
            .map(|l| pb::MirrorLine {
                branch: l.branch.clone(),
                line: l.line.clone().unwrap_or_else(|| l.branch.clone()),
                direction: "follow".to_owned(),
            })
            .collect();
        let export = file.export.as_ref().map(|e| pb::MirrorExport {
            default_author: e.default_author.clone(),
            authors: e
                .authors
                .iter()
                .map(|(principal, a)| pb::MirrorAuthor {
                    principal: principal.clone(),
                    identity: format!("{} <{}>", a.name, a.email),
                })
                .collect(),
        });
        let file_name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
        out.push(Planned {
            source: source.clone(),
            credential: credential_dir.map(|d| d.join(&file.repo)),
            dir: data_dir.join(&file.repo),
            old_git: file_name(&file.mirror_dir),
            old_db: file_name(&file.db),
            row: pb::SetMirrorRequest {
                repo: file.repo,
                upstream: file.upstream,
                lines,
                interval_s: interval,
                enabled: true,
                export,
                expected_version: None,
            },
        });
    }
    Ok(out)
}

/// What happened, or would happen, to a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAction {
    /// Created.
    Created,
    /// An identical row exists.
    Unchanged,
    /// A row exists with these fields different; left alone.
    Differs(Vec<&'static str>),
}

/// What happened, or would happen, to a row's credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialAction {
    /// Seeded from this file.
    Seeded(PathBuf),
    /// The row already has one; the file is not read.
    AlreadySet,
    /// No credential file for this repository.
    NoFile,
    /// The row differs, so nothing was done.
    Skipped,
}

/// What happened, or would happen, to the data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirAction {
    /// The marker was written (old files moved into place first).
    Marked,
    /// The marker for this repository was already there.
    AlreadyMarked,
    /// The directory does not exist; `serve` would start it from empty.
    Missing,
    /// Refused, described (a marker naming another repository, or a
    /// rename that would overwrite).
    Refused(String),
    /// The row differs, so nothing was done.
    Skipped,
}

/// One config's outcome.
#[derive(Clone, Debug)]
pub struct Report {
    /// The repository.
    pub repo: String,
    /// The upstream.
    pub upstream: String,
    /// The row.
    pub row: RowAction,
    /// The credential.
    pub credential: CredentialAction,
    /// The data directory.
    pub dir: DirAction,
}

impl Report {
    /// Whether this needs the operator: a differing row, a missing or
    /// refused directory.
    pub fn needs_attention(&self) -> bool {
        matches!(self.row, RowAction::Differs(_))
            || matches!(self.dir, DirAction::Missing | DirAction::Refused(_))
    }

    /// Whether nothing was (or would be) changed.
    pub fn is_noop(&self) -> bool {
        self.row == RowAction::Unchanged
            && !matches!(self.credential, CredentialAction::Seeded(_))
            && self.dir != DirAction::Marked
    }

    /// The report line, with `would ` before each change on a dry run.
    /// Names files, never their contents.
    pub fn line(&self, dry_run: bool) -> String {
        let w = if dry_run { "would " } else { "" };
        let row = match &self.row {
            RowAction::Created => format!("row: {w}create"),
            RowAction::Unchanged => "row: unchanged".to_owned(),
            RowAction::Differs(f) => format!("row: DIFFERS ({}), left alone", f.join(", ")),
        };
        let credential = match &self.credential {
            CredentialAction::Seeded(p) => format!("credential: {w}seed from {}", p.display()),
            CredentialAction::AlreadySet => "credential: already set".to_owned(),
            CredentialAction::NoFile => "credential: none".to_owned(),
            CredentialAction::Skipped => "credential: skipped".to_owned(),
        };
        let dir = match &self.dir {
            DirAction::Marked => format!("dir: {w}mark"),
            DirAction::AlreadyMarked => "dir: marked".to_owned(),
            DirAction::Missing => "dir: MISSING".to_owned(),
            DirAction::Refused(why) => format!("dir: REFUSED ({why})"),
            DirAction::Skipped => "dir: skipped".to_owned(),
        };
        format!(
            "{} {}: {row}; {credential}; {dir}",
            self.repo, self.upstream
        )
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.line(false))
    }
}

/// The fields of `existing` that differ from `want`. Line and author order
/// do not count.
pub fn differences(want: &pb::SetMirrorRequest, existing: &pb::Mirror) -> Vec<&'static str> {
    let mut out = Vec::new();
    let lines = |ls: &[pb::MirrorLine]| {
        let mut v: Vec<_> = ls
            .iter()
            .map(|l| (l.branch.clone(), l.line.clone(), l.direction.clone()))
            .collect();
        v.sort();
        v
    };
    if lines(&want.lines) != lines(&existing.lines) {
        out.push("lines");
    }
    if want.interval_s != existing.interval_s {
        out.push("interval_s");
    }
    if want.enabled != existing.enabled {
        out.push("enabled");
    }
    let export = |e: Option<&pb::MirrorExport>| {
        let e = e.cloned().unwrap_or_default();
        let mut a: Vec<_> = e
            .authors
            .into_iter()
            .map(|a| (a.principal, a.identity))
            .collect();
        a.sort();
        (e.default_author, a)
    };
    if export(want.export.as_ref()) != export(existing.export.as_ref()) {
        out.push("export");
    }
    out
}

/// Imports every planned config, or reports what it would do when
/// `dry_run`. Errors from the server stop the run; per-repository
/// conditions are in the reports.
pub async fn import_config<A: MirrorAdmin>(
    admin: &A,
    plans: &[Planned],
    dry_run: bool,
) -> Result<Vec<Report>, MirrorError> {
    let mut reports = Vec::with_capacity(plans.len());
    for p in plans {
        reports.push(import_one(admin, p, dry_run).await?);
    }
    Ok(reports)
}

async fn import_one<A: MirrorAdmin>(
    admin: &A,
    p: &Planned,
    dry_run: bool,
) -> Result<Report, MirrorError> {
    let repo = p.row.repo.clone();
    let upstream = p.row.upstream.clone();
    let existing = admin.get_mirror(&repo, &upstream).await?;
    let (row, current) = match existing {
        None if dry_run => (RowAction::Created, None),
        None => (
            RowAction::Created,
            Some(admin.set_mirror(p.row.clone()).await?),
        ),
        Some(m) => {
            let diff = differences(&p.row, &m);
            if !diff.is_empty() {
                return Ok(Report {
                    repo,
                    upstream,
                    row: RowAction::Differs(diff),
                    credential: CredentialAction::Skipped,
                    dir: DirAction::Skipped,
                });
            }
            (RowAction::Unchanged, Some(m))
        }
    };

    let credential = match (&p.credential, &current) {
        (_, Some(m)) if m.credential_set => CredentialAction::AlreadySet,
        (Some(path), _) if path.is_file() => {
            if !dry_run {
                let token = read_credential(path)?;
                let version = current.as_ref().map_or(0, |m| m.version);
                admin
                    .set_credential(&repo, &upstream, &token, version)
                    .await?;
            }
            CredentialAction::Seeded(path.clone())
        }
        _ => CredentialAction::NoFile,
    };

    let dir = adopt_dir(p, dry_run);
    Ok(Report {
        repo,
        upstream,
        row,
        credential,
        dir,
    })
}

/// Reads one credential file. The error names the file, never the value.
fn read_credential(path: &Path) -> Result<SecretToken, MirrorError> {
    let text = std::fs::read_to_string(path).map_err(|_| {
        MirrorError::Config(format!(
            "the credential file {} is unreadable",
            path.display()
        ))
    })?;
    SecretToken::new(text.trim().to_owned()).ok_or_else(|| {
        MirrorError::Config(format!("the credential file {} is empty", path.display()))
    })
}

/// Moves the old files into the service's layout and writes the marker.
fn adopt_dir(p: &Planned, dry_run: bool) -> DirAction {
    let repo = &p.row.repo;
    if !p.dir.is_dir() {
        return DirAction::Missing;
    }
    let marker = p.dir.join(MIGRATION_MARKER);
    if marker.is_file() {
        return match marker_repo(&p.dir) {
            Some(other) if other != *repo => {
                DirAction::Refused(format!("its marker names repository {other}"))
            }
            _ => DirAction::AlreadyMarked,
        };
    }
    let moves: Vec<(PathBuf, PathBuf)> = [(&p.old_git, GIT_DIR), (&p.old_db, DB_FILE)]
        .into_iter()
        .filter_map(|(old, new)| {
            let old = p.dir.join(old.as_deref()?);
            (old.exists() && old != p.dir.join(new)).then(|| (old, p.dir.join(new)))
        })
        .collect();
    for (_, to) in &moves {
        if to.exists() {
            return DirAction::Refused(format!(
                "both the old file and {} exist",
                to.file_name().and_then(|n| n.to_str()).unwrap_or("")
            ));
        }
    }
    if dry_run {
        return DirAction::Marked;
    }
    for (from, to) in &moves {
        if std::fs::rename(from, to).is_err() {
            return DirAction::Refused(format!("{} could not be moved", from.display()));
        }
    }
    if std::fs::write(&marker, marker_text("import-config", repo)).is_err() {
        return DirAction::Refused("the marker could not be written".into());
    }
    DirAction::Marked
}

/// The marker's contents: who wrote it and the repository it belongs to.
pub fn marker_text(writer: &str, repo: &str) -> String {
    format!("graft-mirror {writer}\nrepo={repo}\n")
}

/// [`MirrorAdmin`] over gRPC, as the operator.
impl MirrorAdmin for crate::server::Server {
    async fn get_mirror(&self, repo: &str, upstream: &str) -> ApiResult<Option<pb::Mirror>> {
        let req = self.request(
            &admin_cx("get-mirror"),
            pb::MirrorKey {
                repo: repo.to_owned(),
                upstream: upstream.to_owned(),
            },
        )?;
        match self.mirror().get_mirror(req).await {
            Ok(r) => Ok(Some(r.into_inner())),
            Err(s) => match crate::server::status_error(s) {
                ApiError::NotFound { .. } => Ok(None),
                e => Err(e),
            },
        }
    }

    async fn set_mirror(&self, req: pb::SetMirrorRequest) -> ApiResult<pb::Mirror> {
        let req = self.request(&admin_cx("set-mirror"), req)?;
        let resp = self
            .mirror()
            .set_mirror(req)
            .await
            .map_err(crate::server::status_error)?;
        Ok(resp.into_inner())
    }

    async fn set_credential(
        &self,
        repo: &str,
        upstream: &str,
        credential: &SecretToken,
        expected_version: u64,
    ) -> ApiResult<pb::Mirror> {
        let req = self.request(
            &admin_cx("set-credential"),
            pb::SetMirrorCredentialRequest {
                repo: repo.to_owned(),
                upstream: upstream.to_owned(),
                credential: credential.expose().as_bytes().to_vec(),
                expected_version,
            },
        )?;
        let resp = self
            .mirror()
            .set_mirror_credential(req)
            .await
            .map_err(crate::server::status_error)?;
        Ok(resp.into_inner())
    }
}

fn admin_cx(what: &str) -> graft_proto::CallContext {
    graft_proto::CallContext {
        principal: graft_proto::Principal {
            subject: "graft-mirror".to_owned(),
            kind: graft_proto::PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec!["mirror:bind".to_owned()],
        },
        request_id: format!("mirror-import-config-{what}"),
        deadline_ms: None,
    }
}

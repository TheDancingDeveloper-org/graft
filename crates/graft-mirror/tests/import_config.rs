//! `graft-mirror import-config` (D41 §6 step 3) against an in-memory fake
//! of the operator's mirror calls: a rerun is a no-op, a differing row is
//! reported and left alone, the dry run changes nothing, and no credential
//! reaches the report.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use graft_mirror::credentials::SecretToken;
use graft_mirror::migrate::{
    CredentialAction, DirAction, MirrorAdmin, Report, RowAction, import_config, plan,
};
use graft_mirror::service::{MIGRATION_MARKER, marker_repo, prepare_dir};
use graft_proto::error::ApiResult;
use graft_proto::pb;

const SECRET: &str = "ghp_not-a-real-token-but-secret-shaped-0123456789";
const UPSTREAM: &str = "https://github.com/o/alpha.git";

#[derive(Default)]
struct Fake {
    rows: Mutex<Vec<pb::Mirror>>,
    /// Every credential sealed, by repo.
    sealed: Mutex<Vec<(String, String)>>,
    writes: Mutex<usize>,
}

impl MirrorAdmin for Fake {
    async fn get_mirror(&self, repo: &str, upstream: &str) -> ApiResult<Option<pb::Mirror>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .iter()
            .find(|r| r.repo == repo && r.upstream == upstream)
            .cloned())
    }

    async fn set_mirror(&self, req: pb::SetMirrorRequest) -> ApiResult<pb::Mirror> {
        *self.writes.lock().unwrap() += 1;
        let row = pb::Mirror {
            repo: req.repo,
            upstream: req.upstream,
            lines: req.lines,
            interval_s: req.interval_s,
            enabled: req.enabled,
            export: req.export,
            version: 1,
            ..Default::default()
        };
        self.rows.lock().unwrap().push(row.clone());
        Ok(row)
    }

    async fn set_credential(
        &self,
        repo: &str,
        upstream: &str,
        credential: &SecretToken,
        expected_version: u64,
    ) -> ApiResult<pb::Mirror> {
        *self.writes.lock().unwrap() += 1;
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|r| r.repo == repo && r.upstream == upstream)
            .expect("the row exists before its credential");
        assert_eq!(row.version, expected_version, "CAS on the row version");
        row.version += 1;
        row.credential_set = true;
        self.sealed
            .lock()
            .unwrap()
            .push((repo.to_owned(), credential.expose().to_owned()));
        Ok(row.clone())
    }
}

struct Setup {
    _tmp: tempfile::TempDir,
    configs: Vec<PathBuf>,
    data: PathBuf,
    creds: PathBuf,
}

/// One `mirror-alpha.toml` in today's layout, its old volume moved to
/// `<data>/alpha`, and its credential file.
fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let config = root.join("mirror-alpha.toml");
    std::fs::write(
        &config,
        format!(
            r#"
server = "http://graft-server:7878"
repo = "alpha"
upstream = "{UPSTREAM}"
mirror_dir = "/var/lib/graft/mirror/alpha.git"
db = "/var/lib/graft/mirror/alpha.sqlite"
token_file = "/run/secrets/graft-mirror-token"
poll_interval_secs = 30

[export]
default_author = "Bot <bot@example.com>"

[[lines]]
branch = "main"

[[lines]]
branch = "rust-core"
line = "rust-core-gh"
"#
        ),
    )
    .unwrap();
    let data = root.join("data");
    let dir = data.join("alpha");
    std::fs::create_dir_all(dir.join("alpha.git")).unwrap();
    std::fs::write(dir.join("alpha.git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(dir.join("alpha.sqlite"), b"db").unwrap();
    let creds = root.join("creds");
    std::fs::create_dir(&creds).unwrap();
    std::fs::write(creds.join("alpha"), format!("{SECRET}\n")).unwrap();
    Setup {
        _tmp: tmp,
        configs: vec![config],
        data,
        creds,
    }
}

async fn run(s: &Setup, fake: &Fake, dry_run: bool) -> Vec<Report> {
    let plans = plan(&s.configs, &s.data, Some(&s.creds)).unwrap();
    import_config(fake, &plans, dry_run).await.unwrap()
}

fn printed(reports: &[Report], dry_run: bool) -> String {
    let lines: Vec<String> = reports
        .iter()
        .map(|r| format!("{} {:?}", r.line(dry_run), r))
        .collect();
    lines.join("\n")
}

fn assert_no_secret(text: &str) {
    assert!(!text.contains(SECRET), "the credential leaked: {text}");
    assert!(!text.contains("ghp_"), "the credential leaked: {text}");
}

#[tokio::test]
async fn a_second_run_is_a_no_op() {
    let s = setup();
    let fake = Fake::default();
    let first = run(&s, &fake, false).await;
    assert_eq!(first.len(), 1);
    let r = &first[0];
    assert_eq!(r.row, RowAction::Created);
    assert_eq!(
        r.credential,
        CredentialAction::Seeded(s.creds.join("alpha"))
    );
    assert_eq!(r.dir, DirAction::Marked);
    assert!(!r.needs_attention());
    assert_no_secret(&printed(&first, false));

    // The row is what the config says.
    let row = fake.rows.lock().unwrap()[0].clone();
    let mut lines: Vec<_> = row
        .lines
        .iter()
        .map(|l| (l.branch.as_str(), l.line.as_str(), l.direction.as_str()))
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            ("main", "main", "follow"),
            ("rust-core", "rust-core-gh", "follow")
        ]
    );
    assert_eq!(row.interval_s, 30);
    assert!(row.enabled && row.credential_set);
    assert_eq!(
        row.export.unwrap().default_author.as_deref(),
        Some("Bot <bot@example.com>")
    );
    // The credential was sealed once, trimmed, from the file.
    assert_eq!(
        *fake.sealed.lock().unwrap(),
        [("alpha".to_owned(), SECRET.to_owned())]
    );
    // The old files were moved (not copied) to the service's names, and
    // the marker names the repository.
    let dir = s.data.join("alpha");
    assert!(dir.join("mirror.git").join("HEAD").is_file());
    assert!(dir.join("mirror.sqlite").is_file());
    assert!(!dir.join("alpha.git").exists() && !dir.join("alpha.sqlite").exists());
    assert_eq!(marker_repo(&dir).as_deref(), Some("alpha"));
    prepare_dir(&dir, "alpha").expect("serve adopts it");

    let writes = *fake.writes.lock().unwrap();
    let second = run(&s, &fake, false).await;
    let r = &second[0];
    assert_eq!(r.row, RowAction::Unchanged);
    assert_eq!(r.credential, CredentialAction::AlreadySet);
    assert_eq!(r.dir, DirAction::AlreadyMarked);
    assert!(r.is_noop() && !r.needs_attention());
    assert_eq!(*fake.writes.lock().unwrap(), writes, "no server writes");
    assert_eq!(fake.sealed.lock().unwrap().len(), 1);
    assert_no_secret(&printed(&second, false));
}

#[tokio::test]
async fn a_differing_row_is_reported_and_left_alone() {
    let s = setup();
    let fake = Fake::default();
    let existing = pb::Mirror {
        repo: "alpha".into(),
        upstream: UPSTREAM.into(),
        lines: vec![pb::MirrorLine {
            branch: "main".into(),
            line: "main".into(),
            direction: "follow".into(),
        }],
        interval_s: 60,
        enabled: true,
        version: 7,
        ..Default::default()
    };
    fake.rows.lock().unwrap().push(existing.clone());

    let reports = run(&s, &fake, false).await;
    let r = &reports[0];
    assert_eq!(
        r.row,
        RowAction::Differs(vec!["lines", "interval_s", "export"])
    );
    assert_eq!(r.credential, CredentialAction::Skipped);
    assert_eq!(r.dir, DirAction::Skipped);
    assert!(r.needs_attention());
    let line = r.line(false);
    assert!(
        line.contains("DIFFERS (lines, interval_s, export)"),
        "{line}"
    );
    assert_no_secret(&printed(&reports, false));

    // Nothing was overwritten: not the row, its credential or the directory.
    assert_eq!(*fake.writes.lock().unwrap(), 0);
    assert_eq!(fake.rows.lock().unwrap()[0], existing);
    let dir = s.data.join("alpha");
    assert!(!dir.join(MIGRATION_MARKER).exists());
    assert!(dir.join("alpha.git").is_dir());
}

#[tokio::test]
async fn the_dry_run_prints_the_plan_changes_nothing_and_no_secret() {
    let s = setup();
    let fake = Fake::default();
    let reports = run(&s, &fake, true).await;
    let r = &reports[0];
    assert_eq!(r.row, RowAction::Created);
    assert_eq!(
        r.credential,
        CredentialAction::Seeded(s.creds.join("alpha"))
    );
    assert_eq!(r.dir, DirAction::Marked);
    let out = printed(&reports, true);
    assert!(out.contains("row: would create"), "{out}");
    assert!(out.contains("credential: would seed from"), "{out}");
    assert!(out.contains("dir: would mark"), "{out}");
    assert_no_secret(&out);

    assert!(fake.rows.lock().unwrap().is_empty());
    assert_eq!(*fake.writes.lock().unwrap(), 0);
    let dir = s.data.join("alpha");
    assert!(!dir.join(MIGRATION_MARKER).exists());
    assert!(dir.join("alpha.git").is_dir() && !dir.join("mirror.git").exists());

    // The real run afterwards does what the dry run said.
    let real = run(&s, &fake, false).await;
    assert_eq!(real[0].row, r.row);
    assert_eq!(real[0].credential, r.credential);
    assert_eq!(real[0].dir, r.dir);
}

#[tokio::test]
async fn a_directory_marked_for_another_repository_is_refused() {
    let s = setup();
    let dir = s.data.join("alpha");
    std::fs::write(
        dir.join(MIGRATION_MARKER),
        graft_mirror::migrate::marker_text("import-config", "beta"),
    )
    .unwrap();
    let fake = Fake::default();
    let reports = run(&s, &fake, false).await;
    assert_eq!(
        reports[0].dir,
        DirAction::Refused("its marker names repository beta".into())
    );
    assert!(reports[0].needs_attention());
    // The old files stay where they were, and serve refuses the copy too.
    assert!(dir.join("alpha.git").is_dir());
    assert!(prepare_dir(&dir, "alpha").is_err());
}

#[tokio::test]
async fn a_missing_directory_and_credential_are_reported() {
    let s = setup();
    std::fs::remove_dir_all(s.data.join("alpha")).unwrap();
    std::fs::remove_file(s.creds.join("alpha")).unwrap();
    let fake = Fake::default();
    let reports = run(&s, &fake, false).await;
    assert_eq!(reports[0].row, RowAction::Created);
    assert_eq!(reports[0].credential, CredentialAction::NoFile);
    assert_eq!(reports[0].dir, DirAction::Missing);
    assert!(reports[0].needs_attention());
}

#[test]
fn two_configs_for_one_repository_are_refused() {
    let s = setup();
    let twice = [s.configs[0].clone(), s.configs[0].clone()];
    let err = plan(&twice, &s.data, None).unwrap_err().to_string();
    assert!(err.contains("a second config for repo alpha"), "{err}");
}

#[test]
fn every_deploy_config_plans() {
    // The thirteen real configs parse and plan, one row per repository.
    let deploy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
    let mut configs: Vec<PathBuf> = std::fs::read_dir(&deploy)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("mirror") && n.ends_with(".toml"))
        })
        .collect();
    configs.sort();
    let plans = plan(&configs, Path::new("/data"), None).unwrap();
    assert_eq!(plans.len(), configs.len());
    assert!(plans.iter().all(|p| !p.row.lines.is_empty()));
}

//! `graft repo mirror` and `graft repo batch` against a real server
//! (`graft serve`, D41 §3.2, §3.5): an agent token is refused binding with
//! a typed error, a cut retried with the same request id returns the same
//! request, and a credential never appears in any output.

mod support;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};

use graft_model::ChangesetId;
use serde_json::Value;
use support::{graft, scratch};

const UP: &str = "https://github.com/o/r.git";
const SECRET: &str = "ghp_cli_never_prints_this_0123456789";
const SECRET_FILE: &str = "ghp_cli_file_secret_9876543210";

/// A server started with `graft serve` on a fresh site.
struct Site {
    dir: tempfile::TempDir,
    config: PathBuf,
    child: Child,
    _stdout: BufReader<ChildStdout>,
    endpoint: String,
}

impl Site {
    fn start() -> Self {
        let dir = scratch();
        let kek = dir.path().join("kek");
        std::fs::write(&kek, [0x5a; 32]).expect("kek");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&kek, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        let config = dir.path().join("server.toml");
        std::fs::write(
            &config,
            format!(
                "listen = \"127.0.0.1:0\"\nobjects_dir = \"{}\"\nmeta_db = \"{}\"\njson_logs = false\nshutdown_grace_s = 1\nmin_free_fraction = 0.0\n\n[mirror]\ncredential_kek = {{ id = \"k1\", file = \"{}\" }}\n",
                dir.path().join("objects").display(),
                dir.path().join("meta/meta.db").display(),
                kek.display(),
            ),
        )
        .expect("config");
        let mut child = Command::new(env!("CARGO_BIN_EXE_graft"))
            .args(["serve", "--config"])
            .arg(&config)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn graft serve");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();
        let addr = loop {
            line.clear();
            let n = stdout.read_line(&mut line).expect("read stdout");
            assert!(n > 0, "graft serve exited before listening");
            if let Some(a) = line.trim().strip_prefix("graft-server listening on ") {
                break a.to_owned();
            }
        };
        Self {
            dir,
            config,
            child,
            _stdout: stdout,
            endpoint: format!("http://{addr}"),
        }
    }

    /// Mints a token of `kind` for `subject` with `scopes`.
    fn mint(&self, subject: &str, kind: &str, scopes: &[&str]) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_graft"));
        cmd.args(["serve", "--config"]).arg(&self.config).args([
            "admin",
            "token",
            "mint",
            "--subject",
            subject,
            "--kind",
            kind,
            "--ttl",
            "3600",
        ]);
        for s in scopes {
            cmd.args(["--scope", s]);
        }
        let out = cmd.output().expect("mint");
        assert!(
            out.status.success(),
            "mint: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    /// Runs `graft --endpoint … <args>` with `token`, optionally feeding
    /// `stdin`.
    fn run(&self, token: &str, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = graft(self.dir.path());
        cmd.arg("--endpoint")
            .arg(&self.endpoint)
            .args(args)
            .env("GRAFT_TOKEN", token)
            .write_stdin(stdin.unwrap_or(""));
        cmd.output().expect("graft output")
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Everything a command printed.
fn printed(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json(out: &Output) -> Value {
    assert!(out.status.success(), "{}", printed(out));
    serde_json::from_slice(&out.stdout).expect("json")
}

#[test]
fn mirror_and_batch_commands_against_a_real_server() {
    let site = Site::start();
    let admin = site.mint("admin", "agent", &["repos:admin"]);
    let made = site.run(&admin, &["repo", "create", "r"], None);
    assert!(made.status.success(), "{}", printed(&made));
    // `credential` and `set` read the row (its version) first, so read too.
    let operator = site.mint("op", "human", &["mirror:bind", "repo:*:read"]);
    let agent = site.mint("agent", "agent", &["repo:r:admin"]);
    let mut outputs = Vec::new();

    // An agent can't bind: the server's typed refusal, with the note that
    // the grant is an operator's.
    let refused = site.run(
        &agent,
        &[
            "--json",
            "repo",
            "mirror",
            "add",
            "r",
            UP,
            "--follow",
            "main=main",
        ],
        None,
    );
    assert!(!refused.status.success());
    let text = printed(&refused);
    assert!(text.contains("permission_denied"), "{text}");
    let error: Value = serde_json::from_slice(&refused.stdout).expect("json error");
    assert_eq!(error["error"]["operator_only"], true, "{text}");
    outputs.push(refused);

    // The operator binds; the agent, as repository admin, disables it.
    let added = site.run(
        &operator,
        &[
            "--json",
            "repo",
            "mirror",
            "add",
            "r",
            UP,
            "--follow",
            "main=main",
        ],
        None,
    );
    let row = json(&added);
    assert_eq!(row["mirror"]["lines"][0]["direction"], "follow");
    assert_eq!(row["mirror"]["credential_set"], false);
    outputs.push(added);
    let set = site.run(
        &agent,
        &["--json", "repo", "mirror", "set", "r", UP, "--disable"],
        None,
    );
    assert_eq!(json(&set)["mirror"]["enabled"], false);
    outputs.push(set);

    // An agent can't seal a credential either.
    let denied = site.run(
        &agent,
        &["repo", "mirror", "credential", "r", UP, "--stdin"],
        Some(SECRET),
    );
    assert!(!denied.status.success());
    assert!(
        printed(&denied).contains("permission denied"),
        "{}",
        printed(&denied)
    );
    assert!(
        printed(&denied).contains("operator-only"),
        "{}",
        printed(&denied)
    );
    outputs.push(denied);

    // The operator seals one from standard input, then from a file.
    let sealed = site.run(
        &operator,
        &["--json", "repo", "mirror", "credential", "r", UP, "--stdin"],
        Some(&format!("{SECRET}\n")),
    );
    let row = json(&sealed);
    assert_eq!(row["mirror"]["credential_set"], true);
    assert_eq!(row["mirror"]["kek_id"], "k1");
    outputs.push(sealed);
    let file = site.path().join("cred");
    std::fs::write(&file, SECRET_FILE).expect("cred file");
    let from_file = site.run(
        &operator,
        &[
            "repo",
            "mirror",
            "credential",
            "r",
            UP,
            "--from-file",
            file.to_str().unwrap(),
        ],
        None,
    );
    assert!(from_file.status.success(), "{}", printed(&from_file));
    outputs.push(from_file);

    // Status, in both forms and for both callers, says a credential is set.
    for token in [&operator, &agent] {
        let human = site.run(token, &["repo", "mirror", "status", "r"], None);
        assert!(human.status.success(), "{}", printed(&human));
        assert!(
            printed(&human).contains("credential:set"),
            "{}",
            printed(&human)
        );
        let listed = site.run(token, &["--json", "repo", "mirror", "status", "r"], None);
        assert_eq!(json(&listed)["mirrors"][0]["credential_set"], true);
        let one = site.run(
            token,
            &["--json", "repo", "mirror", "status", "r", "--upstream", UP],
            None,
        );
        assert_eq!(json(&one)["mirror"]["upstream"], UP);
        outputs.extend([human, listed, one]);
    }

    // A cut retried with the same request id returns the same request.
    let cut = |id: &str| {
        site.run(
            &agent,
            &[
                "--json",
                "repo",
                "batch",
                "cut",
                "r",
                "--line",
                "main",
                "--request-id",
                id,
            ],
            None,
        )
    };
    let first = cut("cut-1");
    let first_json = json(&first);
    let again = cut("cut-1");
    let again_json = json(&again);
    assert_eq!(first_json["request"]["id"], again_json["request"]["id"]);
    assert_eq!(first_json["request"]["client_request_id"], "cut-1");
    assert_eq!(first_json["request"]["kind"], "cut_batch");
    outputs.extend([first, again]);
    let listed = site.run(&agent, &["--json", "repo", "batch", "list", "r"], None);
    let batches = json(&listed)["batches"].as_array().expect("batches").len();
    assert_eq!(batches, 1, "the retry recorded nothing new");
    outputs.push(listed);
    let status = site.run(&agent, &["repo", "batch", "status", "r"], None);
    assert!(status.status.success(), "{}", printed(&status));
    outputs.push(status);

    // No mirror service runs here, so the cut holds the line. Only the
    // operator cancels it, and then the line takes a new cut.
    let id = first_json["request"]["id"].to_string();
    let blocked = cut("cut-2");
    assert!(!blocked.status.success(), "{}", printed(&blocked));
    outputs.push(blocked);
    let refused = site.run(&agent, &["repo", "batch", "cancel", "r", &id], None);
    assert!(!refused.status.success());
    assert!(
        printed(&refused).contains("permission denied"),
        "{}",
        printed(&refused)
    );
    let cancelled = site.run(
        &operator,
        &["--json", "repo", "batch", "cancel", "r", &id],
        None,
    );
    let cancelled_json = json(&cancelled);
    assert_eq!(cancelled_json["request"]["state"], "failed");
    assert_eq!(cancelled_json["request"]["batch_state"], "settled");
    assert_eq!(
        cancelled_json["request"]["result"]["error_code"],
        "cancelled"
    );
    let next = cut("cut-2");
    assert_ne!(json(&next)["request"]["id"], first_json["request"]["id"]);
    outputs.extend([refused, cancelled, next]);

    // An agent can't remove or purge.
    for args in [
        &["repo", "mirror", "remove", "r", UP][..],
        &["repo", "mirror", "purge", "r", UP][..],
    ] {
        let out = site.run(&agent, args, None);
        assert!(!out.status.success());
        assert!(
            printed(&out).contains("permission denied"),
            "{}",
            printed(&out)
        );
        outputs.push(out);
    }

    // No command printed either credential.
    for out in &outputs {
        let text = printed(out);
        assert!(!text.contains(SECRET), "credential printed: {text}");
        assert!(!text.contains(SECRET_FILE), "credential printed: {text}");
    }
}

#[test]
fn credential_needs_a_file_or_stdin_and_never_an_argument() {
    let dir = scratch();
    let out = graft(dir.path())
        .args(["repo", "mirror", "credential", "r", UP])
        .output()
        .expect("graft");
    assert!(!out.status.success());
    let out = graft(dir.path())
        .args(["repo", "mirror", "credential", "r", UP, SECRET])
        .output()
        .expect("graft");
    assert!(!out.status.success(), "a credential argument is refused");
}

#[test]
fn mark_followed_requires_repository_admin() {
    let site = Site::start();
    let admin = site.mint("admin", "agent", &["repos:admin"]);
    let made = site.run(&admin, &["repo", "create", "r"], None);
    assert!(made.status.success(), "{}", printed(&made));
    let token = site.mint("reader", "agent", &["repo:r:read"]);
    let target = ChangesetId::hash(b"target").to_string();
    let refused = site.run(
        &token,
        &[
            "repo",
            "mirror",
            "mark-followed",
            "r",
            "--line",
            "main",
            "--changeset",
            &target,
        ],
        None,
    );
    assert!(!refused.status.success(), "{}", printed(&refused));
    assert!(
        printed(&refused).contains("permission denied"),
        "{}",
        printed(&refused)
    );
}

/// Binds `r` to `UP` following each of `lines` (`main=main` style).
fn follow(site: &Site, lines: &[&str]) {
    let operator = site.mint("op", "human", &["mirror:bind", "repo:*:read"]);
    let mut args = vec!["repo", "mirror", "add", "r", UP];
    for l in lines {
        args.push("--follow");
        args.push(l);
    }
    let added = site.run(&operator, &args, None);
    assert!(added.status.success(), "{}", printed(&added));
}

/// Imports a one-commit git repository into `r` (line `main`) and returns the
/// line's tip changeset.
fn land_changeset(site: &Site) -> ChangesetId {
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", printed(&out));
    };
    let src = site.path().join("src");
    std::fs::create_dir(&src).expect("src dir");
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("a.txt"), "a\n").expect("write");
    git(&src, &["add", "a.txt"]);
    git(&src, &["commit", "-q", "-m", "first"]);
    let writer = site.mint("writer", "agent", &["repos:admin", "repo:r:admin"]);
    let db = site.path().join("import.sqlite");
    let imported = site.run(
        &writer,
        &[
            "import",
            &format!("file://{}", src.display()),
            "--repo",
            "r",
            "--line",
            "main",
            "--db",
            db.to_str().expect("utf8"),
        ],
        None,
    );
    assert!(imported.status.success(), "{}", printed(&imported));
    let app = site.path().join("app");
    let cloned = site.run(&writer, &["clone", "r", app.to_str().expect("utf8")], None);
    assert!(cloned.status.success(), "{}", printed(&cloned));
    let log = site.run(
        &writer,
        &[
            "--json",
            "-C",
            app.to_str().expect("utf8"),
            "log",
            "--limit",
            "1",
        ],
        None,
    );
    assert!(log.status.success(), "{}", printed(&log));
    let text = String::from_utf8_lossy(&log.stdout).into_owned();
    let hex = text
        .split('"')
        .find(|t| ChangesetId::parse(t).is_ok())
        .expect("a changeset id in the log");
    ChangesetId::parse(hex).expect("id")
}

/// Records `id` as the mirror's `last_changeset`, as the mirror service would.
fn set_last_changeset(site: &Site, id: &ChangesetId) {
    let db = rusqlite::Connection::open(site.dir.path().join("meta/meta.db")).expect("meta db");
    let n = db
        .execute(
            "UPDATE repo_mirrors SET status_set = 1, status_changeset = ?1 WHERE upstream = ?2",
            rusqlite::params![id.0.0.to_vec(), UP],
        )
        .expect("set status");
    assert_eq!(n, 1);
}

#[test]
fn mark_followed_without_changeset_refuses_a_multi_line_mirror() {
    let site = Site::start();
    let admin = site.mint("admin", "agent", &["repos:admin"]);
    let made = site.run(&admin, &["repo", "create", "r"], None);
    assert!(made.status.success(), "{}", printed(&made));
    follow(&site, &["main=main", "dev=dev"]);
    set_last_changeset(&site, &ChangesetId::hash(b"target"));
    let agent = site.mint("agent", "agent", &["repo:r:admin"]);
    let refused = site.run(
        &agent,
        &["repo", "mirror", "mark-followed", "r", "--line", "main"],
        None,
    );
    assert!(!refused.status.success(), "{}", printed(&refused));
    assert!(
        printed(&refused)
            .contains("--changeset is required when the mirror follows more than one line"),
        "{}",
        printed(&refused)
    );
}

#[test]
fn mark_followed_without_changeset_uses_a_single_line_mirrors_last_changeset() {
    let site = Site::start();
    let admin = site.mint("admin", "agent", &["repos:admin"]);
    let made = site.run(&admin, &["repo", "create", "r"], None);
    assert!(made.status.success(), "{}", printed(&made));
    follow(&site, &["main=main"]);
    let target = land_changeset(&site);
    set_last_changeset(&site, &target);
    let agent = site.mint("agent", "agent", &["repo:r:admin"]);
    let marked = site.run(
        &agent,
        &[
            "--json",
            "repo",
            "mirror",
            "mark-followed",
            "r",
            "--line",
            "main",
        ],
        None,
    );
    assert!(marked.status.success(), "{}", printed(&marked));
    assert_eq!(json(&marked)["target"], target.to_string());
}

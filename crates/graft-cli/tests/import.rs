//! `graft import` against `graft serve --dev`: a synthetic git repo, then a
//! clone whose files match. A missing server config names `graft login`.
//! The token never appears in a child argument, stdout or stderr.

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use graft_auth::{Claims, NoDenylist, SigningKey, mint, new_jti, verify};
use graft_proto::pb::CreateRepoRequest;
use graft_proto::pb::admin_service_client::AdminServiceClient;
use graft_proto::{PROTOCOL_VERSION, Principal, PrincipalKind, VERSION_HEADER};
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::transport::Channel;

use support::{graft, scratch};

const HANG: Duration = Duration::from_secs(120);

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.test")
        .env("GIT_AUTHOR_DATE", "1000000000 +0000")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.test")
        .env("GIT_COMMITTER_DATE", "1000000000 +0000")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Dev {
    child: std::process::Child,
    token: String,
    endpoint: String,
    captured_err: Arc<Mutex<String>>,
}

fn spawn_dev(home: &Path, repo: &str) -> Dev {
    let config_path = home.join("config.toml");
    let data = home.join("data");
    let state = home.join("state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(home)
        .env("GRAFT_CONFIG", &config_path)
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .args([
            "serve",
            "--dev",
            "--data",
            data.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "--repo",
            repo,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve --dev");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let (url_tx, url_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Some(url) = line.trim().strip_prefix("graft clone ") {
                let _ = url_tx.send(url.to_owned());
            }
        }
    });
    let captured_err = Arc::new(Mutex::new(String::new()));
    let err_buf = Arc::clone(&captured_err);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            err_buf.lock().unwrap().push_str(&line);
            err_buf.lock().unwrap().push('\n');
        }
    });
    let started = Instant::now();
    let url = loop {
        match url_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(url) => break url,
            Err(_) => {
                if let Some(status) = child.try_wait().expect("poll") {
                    panic!(
                        "serve --dev exited {status}\n{}",
                        captured_err.lock().unwrap()
                    );
                }
                if started.elapsed() >= HANG {
                    let _ = child.kill();
                    panic!("serve --dev hung\n{}", captured_err.lock().unwrap());
                }
            }
        }
    };
    let config_wait = Instant::now();
    while !config_path.exists() {
        if config_wait.elapsed() >= HANG {
            let _ = child.kill();
            panic!("user config never written");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let config_text = fs::read_to_string(&config_path).unwrap();
    let parsed: toml::Value = toml::from_str(&config_text).unwrap();
    let token_path = Path::new(parsed["servers"][repo]["token_file"].as_str().unwrap());
    let token = fs::read_to_string(token_path).unwrap().trim().to_owned();
    let endpoint = parsed["servers"][repo]["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let _ = url;
    Dev {
        child,
        token,
        endpoint,
        captured_err,
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Admin token for `repo`, signed by the same key `serve --dev` stored.
/// The minted dev token is scoped to the seeded repository only.
fn admin_token(state_dir: &Path, dev_token: &str, repo: &str) -> String {
    let key = fs::read(state_dir.join("signing.key")).expect("signing key");
    let signing = SigningKey::from_seed(key.as_slice().try_into().expect("32-byte seed"));
    let claims = verify(
        &signing.verifying_key(),
        &graft_auth::Token::from_encoded(dev_token.to_owned()),
        i64::MAX / 4,
        &NoDenylist,
    )
    .expect("dev token");
    let minted = mint(
        &signing,
        &Claims {
            principal: Principal {
                subject: claims.principal.subject,
                kind: PrincipalKind::Human,
                session: None,
                work_item: None,
                scopes: vec![format!("repo:{repo}:admin")],
            },
            exp: claims.exp,
            jti: new_jti().expect("jti"),
        },
    )
    .expect("mint");
    minted.expose().to_owned()
}

/// Creates `name` through AdminService. The dev seed repo is not empty.
fn create_empty_repo(endpoint: &str, token: &str, name: &str) {
    #[derive(Clone)]
    struct Auth {
        token: String,
    }
    impl Interceptor for Auth {
        fn call(
            &mut self,
            mut req: tonic::Request<()>,
        ) -> Result<tonic::Request<()>, tonic::Status> {
            let md = req.metadata_mut();
            md.insert(
                "authorization",
                format!("Bearer {}", self.token).parse().expect("header"),
            );
            md.insert(
                VERSION_HEADER,
                PROTOCOL_VERSION
                    .to_string()
                    .parse::<MetadataValue<_>>()
                    .expect("header"),
            );
            Ok(req)
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let channel = Channel::from_shared(endpoint.to_owned())
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let mut admin = AdminServiceClient::with_interceptor(
            channel,
            Auth {
                token: token.to_owned(),
            },
        );
        admin
            .create_repo(CreateRepoRequest {
                name: name.to_owned(),
            })
            .await
            .expect("create empty repo");
    });
}

#[test]
fn missing_server_names_login() {
    let dir = scratch();
    graft(dir.path())
        .args(["import", "/tmp/no-such-git", "--repo", "demo"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("graft login"));
}

#[test]
fn failed_import_removes_token_file() {
    let home = scratch();
    let dev = spawn_dev(home.path(), "demo");
    let config = home.path().join("config.toml");
    let before = import_token_dirs();
    let import = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(home.path())
        .env("GRAFT_CONFIG", &config)
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .args([
            "import",
            "file:///tmp/no-such-git-repo",
            "--repo",
            "demo",
            "--db",
            home.path().join("mirror.sqlite").to_str().unwrap(),
        ])
        .output()
        .expect("import");
    assert!(
        !import.status.success(),
        "bad source should fail\n{}",
        String::from_utf8_lossy(&import.stderr)
    );
    let stdout = String::from_utf8_lossy(&import.stdout);
    let stderr = String::from_utf8_lossy(&import.stderr);
    assert!(!stdout.contains(&dev.token), "token on stdout");
    assert!(!stderr.contains(&dev.token), "token on stderr");
    assert!(
        !dev.captured_err.lock().unwrap().contains(&dev.token),
        "token on serve stderr"
    );
    let leaked: Vec<_> = import_token_dirs().difference(&before).cloned().collect();
    assert!(
        leaked.is_empty(),
        "token file remains after a failed import: {leaked:?}"
    );
}

#[test]
fn import_file_url_then_clone_matches() {
    let home = scratch();
    let dev = spawn_dev(home.path(), "demo");

    let src = scratch();
    git(src.path(), &["init", "-q", "-b", "main"]);
    fs::write(src.path().join("hello.txt"), b"hello graft\n").unwrap();
    git(src.path(), &["add", "hello.txt"]);
    git(src.path(), &["commit", "-q", "-m", "hello"]);
    let bare = scratch();
    let bare_path = bare.path().join("repo.git");
    assert!(
        Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(src.path())
            .arg(&bare_path)
            .status()
            .unwrap()
            .success()
    );
    let url = format!("file://{}", bare_path.display());
    let db = home.path().join("mirror.sqlite");

    // `serve --dev --repo demo` seeds `demo` with a root changeset. Import
    // into a second, empty repository so the first git commit fast-forwards.
    let import_token = admin_token(&home.path().join("state"), &dev.token, "imported");
    create_empty_repo(&dev.endpoint, &import_token, "imported");

    let import_config = home.path().join("import-config.toml");
    let import_token_path = home.path().join("import.token");
    fs::write(&import_token_path, format!("{import_token}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&import_token_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(
        &import_config,
        format!(
            "default_server = \"imported\"\n\n[servers.imported]\nendpoint = \"{}\"\ntoken_file = \"{}\"\n",
            dev.endpoint,
            import_token_path.display()
        ),
    )
    .unwrap();

    let before_import = import_token_dirs();
    let import = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(home.path())
        .env("GRAFT_CONFIG", &import_config)
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .args([
            "import",
            &url,
            "--repo",
            "imported",
            "--line",
            "main",
            "--db",
            db.to_str().unwrap(),
        ])
        .output()
        .expect("import");
    let stdout = String::from_utf8_lossy(&import.stdout).to_string();
    let stderr = String::from_utf8_lossy(&import.stderr).to_string();
    assert!(
        import.status.success(),
        "import failed {}\n{stdout}\n{stderr}\nserve:\n{}",
        import.status,
        dev.captured_err.lock().unwrap()
    );
    assert!(stdout.contains("imported "), "{stdout}");
    assert!(!stdout.contains(&import_token), "token on stdout");
    assert!(!stderr.contains(&import_token), "token on stderr");
    assert!(
        !dev.captured_err.lock().unwrap().contains(&import_token),
        "token on serve stderr"
    );
    // The parent only sees this process's argv (the URL and flags), not the
    // mirror child's. The token must not appear in that output.
    let joined = format!("{url} {stdout} {stderr}");
    assert!(!joined.contains(&import_token));
    let leaked: Vec<_> = import_token_dirs()
        .difference(&before_import)
        .cloned()
        .collect();
    assert!(
        leaked.is_empty(),
        "token file remains after a successful import: {leaked:?}"
    );

    let clone_root = home.path().join("fresh");
    fs::create_dir(&clone_root).unwrap();
    let host = dev.endpoint.trim_start_matches("http://");
    let clone_url = format!("graft://{host}/imported");
    let mut clone = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(&clone_root)
        .env("GRAFT_CONFIG", &import_config)
        .args(["clone", &clone_url, "app"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("clone");
    let mut clone_out = clone.stdout.take().unwrap();
    let mut clone_err = clone.stderr.take().unwrap();
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let o = Arc::clone(&out_buf);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = clone_out.read_to_end(&mut buf);
        *o.lock().unwrap() = buf;
    });
    let e = Arc::clone(&err_buf);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = clone_err.read_to_end(&mut buf);
        *e.lock().unwrap() = buf;
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = clone.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() >= HANG {
            let _ = clone.kill();
            panic!("clone hung");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let clone_text = format!(
        "{}{}",
        String::from_utf8_lossy(&out_buf.lock().unwrap()),
        String::from_utf8_lossy(&err_buf.lock().unwrap())
    );
    assert!(status.success(), "clone failed {status}\n{clone_text}");
    assert!(!clone_text.contains(&import_token));
    let hello = fs::read_to_string(clone_root.join("app/hello.txt")).expect("cloned file");
    assert_eq!(hello, "hello graft\n");
}

/// Names of `graft_import_*` directories in the process temp dir.
fn import_token_dirs() -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return names;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("graft_import_") {
            names.insert(name.to_owned());
        }
    }
    names
}

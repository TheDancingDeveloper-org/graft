//! `graft doctor` against the testkit server.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use graft_auth::{Claims, SigningKey, Token, mint, new_jti};
use graft_proto::context::{Principal, PrincipalKind};
use serde_json::Value;
use support::{graft, json_out, scratch, serve};

const TOKEN: &str = "doctor-token-must-stay-hidden";

fn write_token(path: &std::path::Path, raw: &str) {
    fs::write(path, raw).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms).unwrap();
}

fn minted(exp: i64) -> String {
    let key = SigningKey::generate().expect("key");
    let claims = Claims {
        principal: Principal {
            subject: "doctor".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec!["repo:demo:read".into()],
        },
        exp,
        jti: new_jti().expect("jti"),
    };
    mint(&key, &claims).expect("mint").expose().to_owned()
}

fn login(home: &std::path::Path, config: &std::path::Path, url: &str, token: &str) {
    graft(home)
        .env("GRAFT_CONFIG", config)
        .args(["login", "dev", "--endpoint", url])
        .write_stdin(format!("{token}\n"))
        .assert()
        .success();
}

fn assert_redacted(text: &str) {
    assert!(!text.contains(TOKEN), "doctor output contained the token");
}

#[test]
fn healthy_server_every_check_ok() {
    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    let raw = minted(4_102_444_800);
    login(home.path(), &config, &served.url, &raw);
    let dir = scratch();
    graft(dir.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--endpoint", &served.url, "clone", support::REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    let out = graft(&app)
        .env("GRAFT_CONFIG", &config)
        .args(["doctor"])
        .assert()
        .success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    for name in ["config", "server", "endpoint", "protocol", "token"] {
        assert!(
            text.lines()
                .any(|line| line.starts_with(&format!("ok  {name}  "))),
            "{text}"
        );
    }
    assert!(text.contains("scopes repo:demo:read"), "{text}");
    assert_redacted(&text);
    assert_redacted(&String::from_utf8_lossy(&out.get_output().stderr));
    let _ = Token::from_encoded(raw);
}

#[test]
fn stopped_server_names_the_endpoint() {
    let url = "http://127.0.0.1:1".to_owned();
    let home = scratch();
    let config = home.path().join("config.toml");
    let token_path = home.path().join("tokens").join("dev");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    write_token(&token_path, TOKEN);
    let body = format!(
        "default_server = \"dev\"\n\n[servers.dev]\nendpoint = \"{url}\"\ntoken_file = \"{}\"\n",
        token_path.display()
    );
    fs::write(&config, body).unwrap();
    let out = graft(home.path())
        .env("GRAFT_CONFIG", &config)
        .args(["doctor"])
        .assert()
        .code(1);
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        text.contains(&format!(
            "fail  endpoint  server down or unreachable at {url}"
        )),
        "{text}"
    );
    assert!(text.contains("skip  protocol"), "{text}");
    assert!(text.contains("skip  token"), "{text}");
    assert_redacted(&text);
    assert_redacted(&String::from_utf8_lossy(&out.get_output().stderr));
}

#[test]
fn expired_token_asks_to_log_in_again() {
    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    login(home.path(), &config, &served.url, TOKEN);
    let parsed: toml::Value = toml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    let token_path = std::path::PathBuf::from(
        parsed["servers"]["dev"]["token_file"]
            .as_str()
            .expect("token file"),
    );
    write_token(&token_path, &minted(1));
    let out = graft(home.path())
        .env("GRAFT_CONFIG", &config)
        .args(["doctor"])
        .assert()
        .code(1);
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        text.contains("fail  token  token expired; run `graft login` again"),
        "{text}"
    );
    assert!(text.contains("ok  endpoint"), "{text}");
    assert_redacted(&text);
    assert_redacted(&String::from_utf8_lossy(&out.get_output().stderr));
}

#[test]
fn json_lists_checks_and_redacts_the_token() {
    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    let raw = minted(4_102_444_800);
    login(home.path(), &config, &served.url, &raw);
    let dir = scratch();
    graft(dir.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--endpoint", &served.url, "clone", support::REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    let out = graft(&app)
        .env("GRAFT_CONFIG", &config)
        .args(["--json", "doctor"])
        .assert()
        .success();
    let value: Value = json_out(out.get_output());
    let checks = value["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), 10);
    assert!(
        checks
            .iter()
            .all(|check| check["ok"] == true && check["status"] == "pass")
    );
    let rendered = value.to_string();
    assert_redacted(&rendered);
    assert_redacted(&String::from_utf8_lossy(&out.get_output().stderr));
    assert!(!rendered.contains(&raw));
}

#[test]
fn working_copy_checks_pass_with_a_live_lease_and_ignore_rules() {
    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    login(home.path(), &config, &served.url, &minted(4_102_444_800));
    let dir = scratch();
    graft(dir.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--endpoint", &served.url, "clone", support::REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    fs::write(app.join(".gitignore"), "*.log\n# note\n\n").unwrap();
    fs::write(app.join(".graftignore"), "vendor/\n").unwrap();
    let out = graft(&app)
        .env("GRAFT_CONFIG", &config)
        .args(["doctor"])
        .assert()
        .success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    for needle in [
        "ok  workcopy  present, server dev",
        "ok  revisions  base resolves",
        "ok  lease  no workspace",
        "ok  node  not running; graft talks to the server directly",
        "ok  ignore  .gitignore .graftignore, 2 rules",
    ] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
    assert_redacted(&text);
}

#[test]
fn expired_workspace_lease_names_renew() {
    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    login(home.path(), &config, &served.url, &minted(4_102_444_800));
    let dir = scratch();
    graft(dir.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--endpoint", &served.url, "clone", support::REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    graft(&app)
        .env("GRAFT_CONFIG", &config)
        .args(["workspace", "open"])
        .assert()
        .success();
    served.fixture.repo().expire_workspace("w-1");
    let out = graft(&app)
        .env("GRAFT_CONFIG", &config)
        .args(["doctor"])
        .assert()
        .code(1);
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        text.contains("fail  lease  workspace lease expired; run `graft workspace renew`"),
        "{text}"
    );
    assert_redacted(&text);
}

#[test]
fn unknown_server_and_unreadable_ignore_file_fail() {
    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    login(home.path(), &config, &served.url, &minted(4_102_444_800));
    let dir = scratch();
    graft(dir.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--endpoint", &served.url, "clone", support::REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    let graft_dir = app.join(".graft");
    let config_text = fs::read_to_string(graft_dir.join("config")).unwrap();
    let rewritten = config_text.replace("server = dev\n", "server = missing\n");
    assert_ne!(rewritten, config_text, "{config_text}");
    fs::write(graft_dir.join("config"), rewritten).unwrap();
    fs::create_dir(app.join(".gitignore")).unwrap();
    let out = graft(&app)
        .env("GRAFT_CONFIG", &config)
        .args(["doctor"])
        .assert()
        .code(1);
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        text.contains("fail  workcopy  recorded server missing is not a known named server"),
        "{text}"
    );
    assert!(
        text.contains("skip  revisions  working copy not usable"),
        "{text}"
    );
    assert!(
        text.contains("skip  lease  working copy not usable"),
        "{text}"
    );
    assert!(
        text.contains("fail  ignore  cannot read .gitignore: is a directory"),
        "{text}"
    );
    assert_redacted(&text);
}

#[test]
fn outside_a_working_copy_the_extra_checks_are_skipped() {
    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    login(home.path(), &config, &served.url, &minted(4_102_444_800));
    let out = graft(home.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--json", "doctor"])
        .assert()
        .code(1);
    let value: Value = json_out(out.get_output());
    let checks = value["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), 10);
    for name in ["workcopy", "revisions", "lease", "node", "ignore"] {
        let row = checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(row["ok"], false, "{row}");
        assert_eq!(row["status"], "skip", "{row}");
        assert_eq!(row["detail"], "not in a working copy");
    }
}

#[test]
fn help_lists_exit_status_and_json_status() {
    let out = graft(scratch().path())
        .args(["doctor", "--help"])
        .assert()
        .success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(text.contains("Exit status"), "{text}");
    assert!(text.contains("0  every check passes"), "{text}");
    assert!(text.contains("1  any check fails or is skipped"), "{text}");
    assert!(
        text.contains("2  a usage error, or the working copy cannot be read"),
        "{text}"
    );

    let served = serve();
    let home = scratch();
    let config = home.path().join("config.toml");
    let raw = minted(4_102_444_800);
    login(home.path(), &config, &served.url, &raw);
    let dir = scratch();
    graft(dir.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--endpoint", &served.url, "clone", support::REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    let out = graft(&app)
        .env("GRAFT_CONFIG", &config)
        .args(["--json", "doctor"])
        .assert()
        .success();
    let value: Value = json_out(out.get_output());
    let checks = value["checks"].as_array().expect("checks");
    assert!(
        checks.iter().all(|check| check["status"] == "pass"),
        "{value}"
    );

    let url = "http://127.0.0.1:1".to_owned();
    let down = scratch();
    let down_config = down.path().join("config.toml");
    let token_path = down.path().join("tokens").join("dev");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    write_token(&token_path, TOKEN);
    let body = format!(
        "default_server = \"dev\"\n\n[servers.dev]\nendpoint = \"{url}\"\ntoken_file = \"{}\"\n",
        token_path.display()
    );
    fs::write(&down_config, body).unwrap();
    let out = graft(down.path())
        .env("GRAFT_CONFIG", &down_config)
        .args(["--json", "doctor"])
        .assert()
        .code(1);
    let value: Value = json_out(out.get_output());
    let checks = value["checks"].as_array().expect("checks");
    let row = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing in {value}"))
    };
    let endpoint = row("endpoint");
    assert_eq!(endpoint["ok"], false);
    assert_eq!(endpoint["status"], "fail");
    assert_eq!(row("protocol")["status"], "skip");
    assert_eq!(row("token")["status"], "skip");
    assert_eq!(row("config")["status"], "pass");
    for name in ["workcopy", "revisions", "lease", "node", "ignore"] {
        let check = row(name);
        assert_eq!(check["status"], "skip", "{check}");
        assert_eq!(check["detail"], "not in a working copy", "{check}");
    }
    let rendered = value.to_string();
    assert_redacted(&rendered);
    assert_redacted(&String::from_utf8_lossy(&out.get_output().stderr));
}

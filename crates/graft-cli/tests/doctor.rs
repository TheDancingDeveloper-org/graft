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
    let out = graft(home.path())
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
    let out = graft(home.path())
        .env("GRAFT_CONFIG", &config)
        .args(["--json", "doctor"])
        .assert()
        .success();
    let value: Value = json_out(out.get_output());
    let checks = value["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), 5);
    assert!(checks.iter().all(|check| check["ok"] == true));
    let rendered = value.to_string();
    assert_redacted(&rendered);
    assert_redacted(&String::from_utf8_lossy(&out.get_output().stderr));
    assert!(!rendered.contains(&raw));
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
    login(home.path(), &config, &served.url, &minted(4_102_444_800));
    let out = graft(home.path())
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
        .args(["--json", "doctor"])
        .assert()
        .code(1);
    let value: Value = json_out(out.get_output());
    let by_name: std::collections::BTreeMap<_, _> = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .map(|check| (check["name"].as_str().unwrap().to_owned(), check.clone()))
        .collect();
    assert_eq!(by_name["endpoint"]["status"], "fail");
    assert_eq!(by_name["endpoint"]["ok"], false);
    assert_eq!(by_name["protocol"]["status"], "skip");
    assert_eq!(by_name["token"]["status"], "skip");
    assert_eq!(by_name["config"]["status"], "pass");
}

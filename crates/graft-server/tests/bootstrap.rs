//! The bootstrap admin token (`bootstrap_admin_token_file`): written once on
//! first start, mode 0600, never logged, and left alone afterwards.

mod common;

use std::fs;
use std::process::Stdio;

use common::{Site, command};

fn site(tag: &str, token_file: &std::path::Path) -> Site {
    Site::new(
        tag,
        &format!(
            "bootstrap_admin_token_file = \"{}\"\nbootstrap_admin_repos = [\"boot\"]\n",
            token_file.display()
        ),
    )
}

#[tokio::test]
async fn first_start_writes_a_private_admin_token_that_is_never_logged() {
    let probe = graft_testkit::crash::TempDir::new("server-bootstrap-paths");
    let token_file = probe.path().join("secrets/admin.token");
    let site = site("server-bootstrap", &token_file);
    let log = site.dir.path().join("first.log");

    let server = site.start_with_stderr(Stdio::from(fs::File::create(&log).expect("log")));
    let first = fs::read(&token_file).expect("token file written");
    let token = String::from_utf8(first.clone())
        .expect("utf8")
        .trim()
        .to_owned();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode =
            |p: &std::path::Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&token_file), 0o600);
        assert_eq!(mode(token_file.parent().expect("dir")), 0o700);
    }
    // An admin call the server accepts with the bootstrap token.
    server
        .clients(Some(&token))
        .await
        .create_repo("boot")
        .await
        .expect("admin call with the bootstrap token");
    assert!(server.stop().success());

    let text = fs::read_to_string(&log).expect("read log");
    assert!(
        text.contains(&format!(
            "wrote bootstrap admin token to {}",
            token_file.display()
        )),
        "{text}"
    );
    assert!(!token.is_empty() && !text.contains(&token), "token in log");

    // Second start: the file is byte-identical, and no token is logged.
    let log2 = site.dir.path().join("second.log");
    let again = site.start_with_stderr(Stdio::from(fs::File::create(&log2).expect("log")));
    assert!(again.stop().success());
    assert_eq!(fs::read(&token_file).expect("read again"), first);
    let text = fs::read_to_string(&log2).expect("read log");
    assert!(!text.contains("wrote bootstrap admin token"), "{text}");
    assert!(!text.contains(&token), "token in log");
}

#[cfg(unix)]
#[test]
fn a_read_only_secrets_dir_stops_start_up_naming_the_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let probe = graft_testkit::crash::TempDir::new("server-bootstrap-ro-paths");
    let dir = probe.path().join("ro");
    fs::create_dir(&dir).expect("mkdir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod");
    let token_file = dir.join("admin.token");
    let site = site("server-bootstrap-ro", &token_file);
    let out = command()
        .arg("--config")
        .arg(&site.config)
        .output()
        .expect("run serve");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
    if fs::metadata(&token_file).is_ok() {
        // Running as root: mode bits do not stop the write.
        return;
    }
    assert!(!out.status.success(), "start-up must stop");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains(&format!("bootstrap admin token {}", token_file.display())),
        "{stderr}"
    );
    assert!(!stdout.contains("listening on"), "{stdout}");
}

#[test]
fn the_key_needs_a_repository() {
    let probe = graft_testkit::crash::TempDir::new("server-bootstrap-norepo");
    let site = Site::new(
        "server-bootstrap-norepo",
        &format!(
            "bootstrap_admin_token_file = \"{}\"\n",
            probe.path().join("t").display()
        ),
    );
    let out = command()
        .arg("--config")
        .arg(&site.config)
        .output()
        .expect("run serve");
    assert!(!out.status.success());
    assert!(!probe.path().join("t").exists());
}

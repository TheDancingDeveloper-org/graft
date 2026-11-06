//! An upstream that redirects never takes the mirror's git, or its
//! credential, to another host: git runs with `http.followRedirects=false`,
//! and the credential helper answers only for the upstream's own host. A
//! redirect to a cloud metadata address or an attacker's host would
//! otherwise bypass the upstream check and, on a 401, receive the token.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use graft_mirror::credentials::{CredentialPipe, SecretToken};
use graft_mirror::git::Mirror;

const TOKEN: &str = "ghp_redirect_must_not_leak";

/// Each request's first line and `Authorization` header.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A one-thread HTTP server answering every request with `respond`, and
/// recording each request's first line and `Authorization` header.
fn serve(respond: impl Fn(&str) -> String + Send + 'static) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            let mut auth = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line
                    .strip_prefix("Authorization: ")
                    .or_else(|| line.strip_prefix("authorization: "))
                {
                    auth = Some(v.trim().to_owned());
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
            log.lock().unwrap().push((first.trim().to_owned(), auth));
            let _ = stream.write_all(respond(&path).as_bytes());
        }
    });
    (port, seen)
}

#[tokio::test]
async fn a_redirecting_upstream_is_not_followed_and_gets_no_credential() {
    // The redirect target asks for a credential, as an attacker's would.
    let (evil_port, evil) = serve(|_| {
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned()
    });
    let (up_port, upstream) = serve(move |path| {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:{evil_port}{path}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let mut mirror = Mirror::new(
        dir.path().join("m.git"),
        format!("http://127.0.0.1:{up_port}/o/r.git"),
    );
    mirror.init_fresh().await.expect("init");
    let pipe = CredentialPipe::open(SecretToken::new(TOKEN.to_owned()).unwrap())
        .await
        .unwrap();
    mirror.set_credential_pipe(Some(Arc::new(pipe)));
    let err = mirror
        .ls_remote(&["main"])
        .await
        .expect_err("a redirect fails");
    assert!(!format!("{err:?}").contains(TOKEN));
    assert!(
        !upstream.lock().unwrap().is_empty(),
        "the upstream was asked"
    );
    assert!(
        evil.lock().unwrap().is_empty(),
        "git followed the redirect: {:?}",
        evil.lock().unwrap()
    );
}

#[tokio::test]
async fn the_credential_goes_only_to_the_upstream_host() {
    // The upstream itself asks for a credential: it gets it.
    let (port, seen) = serve(|_| {
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned()
    });
    let dir = tempfile::tempdir().unwrap();
    let mut mirror = Mirror::new(
        dir.path().join("m.git"),
        format!("http://127.0.0.1:{port}/o/r.git"),
    );
    mirror.init_fresh().await.expect("init");
    let pipe = CredentialPipe::open(SecretToken::new(TOKEN.to_owned()).unwrap())
        .await
        .unwrap();
    mirror.set_credential_pipe(Some(Arc::new(pipe)));
    let _ = mirror.ls_remote(&["main"]).await;
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(_, a)| a.as_deref().is_some_and(|a| a.starts_with("Basic "))),
        "the upstream's own host is answered: {seen:?}"
    );
}

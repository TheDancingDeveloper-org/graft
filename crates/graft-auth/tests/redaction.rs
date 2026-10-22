//! Token and key values never appear in `Debug` or `Display` output, or in
//! logs that format them (design §12.2, AGENTS.md rule 8).

use std::io;
use std::sync::{Arc, Mutex};

use graft_auth::{AuthError, Claims, NoDenylist, SigningKey, Token, mint, verify};
use graft_proto::{Principal, PrincipalKind};
use tracing_subscriber::layer::SubscriberExt as _;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn fixture() -> (SigningKey, Token) {
    let key = SigningKey::from_seed(&[0x42; 32]);
    let claims = Claims {
        principal: Principal {
            subject: "svc".into(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec!["objects:read".into()],
        },
        exp: 10,
        jti: "j".into(),
    };
    let token = mint(&key, &claims).unwrap();
    (key, token)
}

/// Every string a leak could take: the encoded token, its signed body, and
/// the key bytes in hex and as a Rust byte array.
fn secrets(key: &SigningKey, token: &Token) -> Vec<String> {
    let body = token.expose().strip_prefix("v4.public.").unwrap();
    vec![
        token.expose().to_owned(),
        body[..24].to_owned(),
        body[body.len() - 24..].to_owned(),
        hex(&key.expose_seed()),
        format!("{:?}", key.expose_seed()),
        hex(&key.verifying_key().to_bytes()),
        format!("{:?}", key.verifying_key().to_bytes()),
    ]
}

fn assert_clean(out: &str, secrets: &[String]) {
    for s in secrets {
        assert!(!out.contains(s.as_str()), "leaked {s:?} in {out:?}");
    }
}

#[test]
fn debug_and_display_redact() {
    let (key, token) = fixture();
    let secrets = secrets(&key, &token);
    let vk = key.verifying_key();
    let errors = [
        verify(&vk, &token, 100, &NoDenylist).unwrap_err(),
        verify(
            &SigningKey::from_seed(&[1; 32]).verifying_key(),
            &token,
            0,
            &NoDenylist,
        )
        .unwrap_err(),
        AuthError::Malformed,
    ];
    let mut out = vec![
        format!("{token:?}"),
        format!("{token:#?}"),
        format!("{key:?}"),
        format!("{key:#?}"),
        format!("{vk:?}"),
        format!("{:?}", Some(&token)),
    ];
    for e in &errors {
        out.push(format!("{e}"));
        out.push(format!("{e:?}"));
        out.push(format!("{:?}", graft_proto::ApiError::from(e.clone())));
    }
    for o in &out {
        assert_clean(o, &secrets);
    }
    assert_eq!(format!("{token:?}"), "Token(<redacted>)");
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn logs_redact() {
    let (key, token) = fixture();
    let secrets = secrets(&key, &token);
    let capture = Capture::default();
    let writer = capture.clone();
    // The layer `graft_telemetry::init` installs in production.
    let subscriber = tracing_subscriber::registry()
        .with(graft_telemetry::log_layer(true, move || writer.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("rpc", token = ?token);
        let _g = span.enter();
        tracing::info!(?token, ?key, vk = ?key.verifying_key(), "authenticating");
        let err = verify(&key.verifying_key(), &token, 100, &NoDenylist).unwrap_err();
        tracing::warn!(error = %err, ?err, "refused");
    });
    let out = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("Token(<redacted>)"), "{out}");
    assert!(out.contains("token expired"), "{out}");
    assert_clean(&out, &secrets);
}

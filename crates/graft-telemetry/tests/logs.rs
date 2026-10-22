//! The production log layer writes JSON carrying span fields.

use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::SubscriberExt as _;

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
fn json_lines_carry_span_fields() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::registry()
        .with(graft_telemetry::log_layer(true, move || writer.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("rpc", repo = "graft", op = "Land", request_id = "r1");
        let _g = span.enter();
        tracing::info!(attempt = 1, "landing");
    });
    let out = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let line = out.lines().next().expect("one log line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["fields"]["message"], "landing");
    assert_eq!(v["fields"]["attempt"], 1);
    assert_eq!(v["span"]["repo"], "graft");
    assert_eq!(v["span"]["op"], "Land");
    assert_eq!(v["span"]["request_id"], "r1");
}

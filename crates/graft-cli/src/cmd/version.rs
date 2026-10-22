//! `graft version`: the build's version and the protocol version it speaks.

use graft_proto::PROTOCOL_VERSION;
use serde_json::json;

use crate::cli::Global;
use crate::error::Failure;
use crate::render;

/// Prints the version.
pub(crate) fn run(g: &Global) -> Result<(), Failure> {
    let version = env!("CARGO_PKG_VERSION");
    if g.json {
        return render::print_json(&json!({
            "version": version,
            "protocol": {
                "major": PROTOCOL_VERSION.major,
                "minor": PROTOCOL_VERSION.minor,
            },
        }));
    }
    println!(
        "graft {version} (protocol {}.{})",
        PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor
    );
    Ok(())
}

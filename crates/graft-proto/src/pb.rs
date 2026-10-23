//! Generated protobuf messages and gRPC services (`proto/graft/v1`).
//!
//! Convert to and from the Rust types in [`crate::types`] with `From` (Rust
//! to wire) and `TryFrom` (wire to Rust, validating at the boundary).

#![allow(missing_docs, unreachable_pub, clippy::all)]

tonic::include_proto!("graft.v1");

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for ExchangeCiTokenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangeCiTokenRequest")
            .field("ci_token", &"<redacted>")
            .field("changeset", &self.changeset)
            .finish()
    }
}

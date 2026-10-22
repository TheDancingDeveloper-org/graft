//! The graft API: traits, request and response types, typed errors, the
//! object frame format, and the protocol version (`docs/design.md` §7).
//!
//! The engine implements the traits in-process, `graft-client` implements
//! them over the wire, and the CLI, MCP and LSP servers are written against
//! them, so every surface works in-process or remote unchanged. The gRPC
//! definitions and generated code arrive with work item G1.3.

pub mod api;
pub mod context;
pub mod error;
pub mod frame;
pub mod types;
pub mod version;

pub use api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
pub use context::{CallContext, Principal, PrincipalKind};
pub use error::ApiError;
pub use frame::{Codec, Frame};
pub use version::{PROTOCOL_VERSION, ProtocolVersion};

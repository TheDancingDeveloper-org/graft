//! The graft API: traits, request and response types, typed errors, the
//! object frame format, and the protocol version (`docs/design.md` §7).
//!
//! The engine implements the traits in-process, `graft-client` implements
//! them over the wire, and the CLI, MCP and LSP servers are written against
//! them, so every surface works in-process or remote unchanged. Over the wire
//! they travel as gRPC: [`pb`] holds the generated messages and services, and
//! `convert` maps between them and the Rust types.

pub mod api;
pub mod context;
pub mod convert;
pub mod error;
pub mod frame;
pub mod pb;
pub mod repo_url;
pub mod types;
pub mod version;

pub use api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
pub use context::{CallContext, Principal, PrincipalKind};
pub use convert::WireError;
pub use error::ApiError;
pub use frame::{Codec, Frame, FrameError};
pub use repo_url::RepoUrl;
pub use version::{PROTOCOL_VERSION, ProtocolVersion, VERSION_HEADER};

//! Protocol version binding (`docs/design.md` §7.1).

/// A `(major, minor)` protocol version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolVersion {
    /// Breaking changes bump this.
    pub major: u16,
    /// Compatible additions bump this.
    pub minor: u16,
}

/// The version this build speaks.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 1 };

impl ProtocolVersion {
    /// Whether a client at `self` can bind to a server at `server`: majors
    /// match and the server's minor is at least the client's.
    pub fn binds_to(self, server: ProtocolVersion) -> bool {
        self.major == server.major && server.minor >= self.minor
    }
}

//! Protocol version binding (`docs/design.md` §7.1).
//!
//! The client sends its version in the [`VERSION_HEADER`] request header as
//! `major.minor`. The server binds it with [`ProtocolVersion::accept`] and
//! answers `IncompatibleProtocol` naming its own version when it can't.

use std::fmt;
use std::str::FromStr;

use crate::error::ApiError;

/// The gRPC metadata key that carries the client's protocol version.
pub const VERSION_HEADER: &str = "graft-protocol";

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

    /// Server side: binds the version a client sent in [`VERSION_HEADER`].
    /// A missing or malformed header does not bind.
    pub fn accept(self, client_header: Option<&str>) -> Result<ProtocolVersion, ApiError> {
        match client_header.and_then(|h| h.parse::<ProtocolVersion>().ok()) {
            Some(client) if client.binds_to(self) => Ok(client),
            _ => Err(ApiError::IncompatibleProtocol {
                server: self.to_string(),
            }),
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// A version string that is not `major.minor` in plain decimal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseVersionError;

impl FromStr for ProtocolVersion {
    type Err = ParseVersionError;

    /// Parses `major.minor`: ASCII digits only, no sign, no leading zeros.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        fn part(p: &str) -> Result<u16, ParseVersionError> {
            let canonical = !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p == "0" || !p.starts_with('0'));
            if !canonical {
                return Err(ParseVersionError);
            }
            p.parse().map_err(|_| ParseVersionError)
        }
        let (major, minor) = s.split_once('.').ok_or(ParseVersionError)?;
        Ok(ProtocolVersion {
            major: part(major)?,
            minor: part(minor)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn v(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    #[test]
    fn binding_rules() {
        // Same version binds.
        assert!(v(1, 3).binds_to(v(1, 3)));
        // An older client binds to a newer minor.
        assert!(v(1, 2).binds_to(v(1, 5)));
        // A newer client does not bind to an older minor.
        assert!(!v(1, 5).binds_to(v(1, 2)));
        // Majors must match, in both directions.
        assert!(!v(1, 0).binds_to(v(2, 0)));
        assert!(!v(2, 0).binds_to(v(1, 9)));
    }

    #[test]
    fn server_accepts_or_names_its_version() {
        let server = v(0, 4);
        assert_eq!(server.accept(Some("0.1")), Ok(v(0, 1)));
        assert_eq!(server.accept(Some("0.4")), Ok(v(0, 4)));
        let refused = Err(ApiError::IncompatibleProtocol {
            server: "0.4".into(),
        });
        for h in [
            None,
            Some("0.5"),
            Some("1.0"),
            Some(""),
            Some("0"),
            Some("0.1.2"),
            Some("00.1"),
            Some("+0.1"),
            Some("0.70000"),
            Some(" 0.1"),
        ] {
            assert_eq!(server.accept(h), refused, "{h:?}");
        }
    }

    #[test]
    fn this_build_binds_to_itself() {
        assert_eq!(
            PROTOCOL_VERSION.accept(Some(&PROTOCOL_VERSION.to_string())),
            Ok(PROTOCOL_VERSION)
        );
    }

    proptest! {
        #[test]
        fn text_form_round_trips(major in any::<u16>(), minor in any::<u16>()) {
            let x = v(major, minor);
            prop_assert_eq!(x.to_string().parse::<ProtocolVersion>(), Ok(x));
        }

        #[test]
        fn accept_matches_binds_to(c in (0u16..3, 0u16..3), s in (0u16..3, 0u16..3)) {
            let (client, server) = (v(c.0, c.1), v(s.0, s.1));
            prop_assert_eq!(
                server.accept(Some(&client.to_string())).is_ok(),
                client.binds_to(server)
            );
        }
    }
}

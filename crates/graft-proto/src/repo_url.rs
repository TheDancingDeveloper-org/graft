//! A `graft://` or `grafts://` repository URL.
//!
//! `graft://host[:port]/repo` is plain HTTP; `grafts://` is TLS. An optional
//! `?line=` selects the line. Userinfo is rejected so a token cannot hide in
//! the URL. `unix:` sockets are not this form. The CLI and MCP share this
//! parser so a URL is accepted or refused the same way on every surface.

/// A parsed repository URL: endpoint, repository, and an optional line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoUrl {
    /// `http://host[:port]` or `https://host[:port]`.
    pub endpoint: String,
    /// True when the scheme was `grafts://`.
    pub tls: bool,
    /// Repository name, one path segment.
    pub repo: String,
    /// Line from `?line=`, when the URL named one.
    pub line: Option<String>,
}

impl RepoUrl {
    /// Parses a clone URL. `Err` is one line of usage text and never echoes
    /// the input.
    pub fn parse(input: &str) -> Result<Self, String> {
        let s = input.trim();
        let (tls, rest) = if let Some(rest) = s.strip_prefix("grafts://") {
            (true, rest)
        } else if let Some(rest) = s.strip_prefix("graft://") {
            (false, rest)
        } else {
            return Err("expected graft://host[:port]/repo or grafts://host[:port]/repo".into());
        };
        if rest.contains('@') {
            return Err(
                "a graft URL must not carry userinfo; pass the token with --token-file or graft login"
                    .into(),
            );
        }
        let (authority, after) = match rest.split_once('/') {
            Some((authority, after)) if !authority.is_empty() && !after.is_empty() => {
                (authority, after)
            }
            _ => {
                return Err("expected graft://host[:port]/repo".into());
            }
        };
        let (path, query) = match after.split_once(['?', '#']) {
            Some((path, query)) if after.contains('?') => (path, Some(query)),
            Some(_) => {
                return Err("a graft URL does not take a fragment".into());
            }
            None => (after, None),
        };
        if path.is_empty()
            || path.contains('/')
            || path.chars().any(char::is_control)
            || path.chars().any(char::is_whitespace)
        {
            return Err("repository name in a graft URL must be a single path segment".into());
        }
        validate_authority(authority)?;
        let line = parse_line(query)?;
        let scheme = if tls { "https" } else { "http" };
        Ok(RepoUrl {
            endpoint: format!("{scheme}://{authority}"),
            tls,
            repo: path.to_owned(),
            line,
        })
    }

    /// True when `endpoint` names the same server as this URL.
    ///
    /// A missing port matches a missing port. `http` and `https` do not
    /// match. The comparison is textual after trimming, so `host` and
    /// `host:80` are different.
    pub fn matches_endpoint(&self, endpoint: &str) -> bool {
        let want = endpoint.trim().trim_end_matches('/');
        self.endpoint == want
    }
}

fn parse_line(query: Option<&str>) -> Result<Option<String>, String> {
    let Some(query) = query else {
        return Ok(None);
    };
    // A fragment after `?line=` is not a query.
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    if query.is_empty() {
        return Err("a graft URL query must be line=<name>".into());
    }
    let mut line = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err("a graft URL query must be line=<name>".into());
        };
        if key != "line" || value.is_empty() || line.is_some() {
            return Err("a graft URL query must be line=<name>".into());
        }
        if value.chars().any(char::is_control) || value.contains('/') {
            return Err("line name in a graft URL is not valid".into());
        }
        line = Some(value.to_owned());
    }
    Ok(line)
}

/// Host, optional port, including `[IPv6]`. An unbracketed host must not
/// contain `:`, so `a:1:2` and `::1` are refused.
fn validate_authority(authority: &str) -> Result<(), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err("IPv6 host in a graft URL must be [addr][:port]".into());
        };
        if host.is_empty() || host.contains('[') || host.contains(']') {
            return Err("IPv6 host in a graft URL must be [addr][:port]".into());
        }
        let port = if let Some(port) = after.strip_prefix(':') {
            if port.is_empty() {
                return Err("graft URL is missing a port after ':'".into());
            }
            Some(port)
        } else if after.is_empty() {
            None
        } else {
            return Err("expected graft://[ipv6][:port]/repo".into());
        };
        (host, port)
    } else if authority.contains(':') {
        let Some((host, port)) = authority.rsplit_once(':') else {
            return Err("graft URL host is not valid".into());
        };
        if host.is_empty() {
            return Err("graft URL is missing a host".into());
        }
        if host.contains(':') {
            return Err("IPv6 host in a graft URL must be [addr][:port]".into());
        }
        if port.is_empty() {
            return Err("graft URL is missing a port after ':'".into());
        }
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host.is_empty()
        || host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '/')
    {
        return Err("graft URL host is not valid".into());
    }
    if let Some(port) = port {
        let value: u16 = port
            .parse()
            .map_err(|_| "graft URL port must be 1 to 65535".to_owned())?;
        if value == 0 {
            return Err("graft URL port must be 1 to 65535".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_endpoint_repo_line_and_tls() {
        let url = RepoUrl::parse("graft://graft-dev:17888/rusttorrent?line=trial").unwrap();
        assert_eq!(url.endpoint, "http://graft-dev:17888");
        assert!(!url.tls);
        assert_eq!(url.repo, "rusttorrent");
        assert_eq!(url.line.as_deref(), Some("trial"));

        let tls = RepoUrl::parse("grafts://example.test/app").unwrap();
        assert_eq!(tls.endpoint, "https://example.test");
        assert!(tls.tls);
        assert!(tls.line.is_none());

        let v6 = RepoUrl::parse("graft://[2001:db8::1]/repo").unwrap();
        assert_eq!(v6.endpoint, "http://[2001:db8::1]");
        let v6_port = RepoUrl::parse("graft://[2001:db8::1]:17888/repo").unwrap();
        assert_eq!(v6_port.endpoint, "http://[2001:db8::1]:17888");
        assert!(RepoUrl::parse("graft://[::1]/repo").is_ok());
    }

    #[test]
    fn rejects_userinfo_bad_ports_and_unbracketed_colons() {
        let userinfo = RepoUrl::parse("graft://token@host/repo").unwrap_err();
        assert!(userinfo.contains("userinfo"), "{userinfo}");
        assert!(!userinfo.contains("token@"));
        let port = RepoUrl::parse("graft://host:/repo").unwrap_err();
        assert!(port.contains("port"), "{port}");
        assert!(RepoUrl::parse("graft://host:abc/repo").is_err());
        assert!(RepoUrl::parse("graft://host:0/repo").is_err());
        assert!(RepoUrl::parse("http://host/repo").is_err());
        assert!(RepoUrl::parse("graft://host").is_err());
        assert!(RepoUrl::parse("unix:/run/graft.sock").is_err());
        assert!(RepoUrl::parse("graft://[::1").is_err());
        assert!(RepoUrl::parse("graft://a:1:2/repo").is_err());
        assert!(RepoUrl::parse("graft://::1/repo").is_err());
        assert!(RepoUrl::parse("graft://host/repo#frag").is_err());
    }

    #[test]
    fn endpoint_match_is_textual() {
        let url = RepoUrl::parse("graft://graft-dev:17888/repo").unwrap();
        assert!(url.matches_endpoint("http://graft-dev:17888"));
        assert!(url.matches_endpoint("http://graft-dev:17888/"));
        assert!(!url.matches_endpoint("http://other:17888"));
        assert!(!url.matches_endpoint("https://graft-dev:17888"));
        assert!(!url.matches_endpoint("http://graft-dev"));
    }
}

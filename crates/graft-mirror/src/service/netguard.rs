//! The upstream check the service makes before every poll (D41 §3.2): the
//! server's URL rules again, then the host resolved and every address it
//! resolves to refused if it is in a blocked range. This closes the
//! DNS-rebinding route round the server's string check.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;

use graft_proto::pb::MirrorErrorCode;

use super::status::PollFailure;

/// Where an upstream points: its host and port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The host name or address, without brackets.
    pub host: String,
    /// The port.
    pub port: u16,
}

fn blocked(why: &str) -> PollFailure {
    PollFailure::new(
        MirrorErrorCode::UrlBlocked,
        format!("the upstream is refused: {why}"),
    )
}

/// Splits `host[:port]` (an IPv6 address in brackets).
fn host_port(authority: &str, default: u16) -> Option<(String, u16)> {
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default,
            None => return None,
        };
        return Some((host.to_owned(), port));
    }
    match hostport.split_once(':') {
        Some((h, p)) => Some((h.to_owned(), p.parse().ok()?)),
        None => Some((hostport.to_owned(), default)),
    }
}

/// The host an upstream URL names, by the server's rules: `https://`,
/// `ssh://` or the scp form `user@host:path`. Anything else is refused.
pub fn target(upstream: &str) -> Result<Target, PollFailure> {
    let (rest, default) = if let Some(r) = upstream.strip_prefix("https://") {
        (r, 443)
    } else if let Some(r) = upstream.strip_prefix("ssh://") {
        (r, 22)
    } else if !upstream.contains("://") && !upstream.contains("::") && upstream.contains('@') {
        // scp form: user@host:path.
        let (authority, _) = upstream
            .split_once(':')
            .ok_or_else(|| blocked("not a URL the mirror may fetch"))?;
        let (host, _) = host_port(authority, 22).ok_or_else(|| blocked("a malformed host"))?;
        return check_name(Target { host, port: 22 });
    } else {
        return Err(blocked("only https and ssh upstreams are fetched"));
    };
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = host_port(authority, default).ok_or_else(|| blocked("a malformed host"))?;
    check_name(Target { host, port })
}

fn check_name(t: Target) -> Result<Target, PollFailure> {
    let h = t.host.trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return Err(blocked("no host"));
    }
    if h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".svc")
        || h.ends_with(".cluster.local")
        || h.ends_with(".internal")
    {
        return Err(blocked("a local or cluster name"));
    }
    if let Ok(ip) = h.parse::<IpAddr>()
        && is_blocked(ip)
    {
        return Err(blocked("a private or local address"));
    }
    Ok(t)
}

fn v4_blocked(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        // CGNAT, 100.64.0.0/10 (the tailnet lives here).
        || (a == 100 && (64..128).contains(&b))
        // Benchmarking, 198.18.0.0/15.
        || (a == 198 && (b == 18 || b == 19))
        // Reserved, 240.0.0.0/4.
        || a >= 240
}

fn v6_blocked(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_blocked(v4);
    }
    let first = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local, fc00::/7.
        || (first & 0xfe00) == 0xfc00
        // Link-local, fe80::/10.
        || (first & 0xffc0) == 0xfe80
        // NAT64 well-known prefix 64:ff9b::/96 can reach any v4 address.
        || (first == 0x0064 && ip.segments()[1] == 0xff9b)
}

/// Whether `ip` is in a range the mirror never connects to.
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => v6_blocked(v6),
    }
}

/// Resolves a host and port to its addresses.
pub type Resolve = std::sync::Arc<
    dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>
        + Send
        + Sync,
>;

/// The system resolver.
pub fn system_resolver() -> Resolve {
    std::sync::Arc::new(|host, port| {
        Box::pin(async move { Ok(tokio::net::lookup_host((host, port)).await?.collect()) })
    })
}

/// Refuses `addrs` if any is blocked; none is unreachable.
pub fn judge(addrs: &[SocketAddr]) -> Result<(), PollFailure> {
    if addrs.is_empty() {
        return Err(PollFailure::new(
            MirrorErrorCode::UpstreamUnreachable,
            "the upstream host did not resolve",
        ));
    }
    if addrs.iter().any(|a| is_blocked(a.ip())) {
        return Err(blocked("it resolves to a private or local address"));
    }
    Ok(())
}

/// Checks `upstream` by the rules and resolves it with `resolve`; refuses
/// it if any address it resolves to is blocked.
pub async fn check(upstream: &str, resolve: &Resolve) -> Result<Target, PollFailure> {
    let t = target(upstream)?;
    let addrs = resolve(t.host.clone(), t.port).await.map_err(|_| {
        PollFailure::new(
            MirrorErrorCode::UpstreamUnreachable,
            "the upstream host did not resolve",
        )
    })?;
    judge(&addrs)?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_read_from_each_url_form() {
        assert_eq!(
            target("https://github.com/o/r.git").unwrap(),
            Target {
                host: "github.com".to_owned(),
                port: 443
            }
        );
        assert_eq!(target("ssh://git@github.com:2222/o/r").unwrap().port, 2222);
        assert_eq!(target("git@github.com:o/r.git").unwrap().host, "github.com");
        assert_eq!(
            target("https://[2606:4700::1]/o/r").unwrap().host,
            "2606:4700::1"
        );
    }

    #[test]
    fn schemes_names_and_literal_addresses_are_refused() {
        for url in [
            "file:///etc",
            "http://github.com/o/r",
            "git://github.com/o/r",
            "ext::sh -c id",
            "/srv/git/r.git",
            "https://localhost/o/r",
            "https://api.default.svc/o/r",
            "https://x.cluster.local/o/r",
            "https://127.0.0.1/o/r",
            "https://10.1.2.3/o/r",
            "https://100.92.54.45/o/r",
            "https://169.254.169.254/latest",
            "https://[::1]/o/r",
            "https://[fd00::1]/o/r",
            "https://[::ffff:192.168.0.1]/o/r",
        ] {
            let e = target(url).expect_err(url);
            assert_eq!(e.code, MirrorErrorCode::UrlBlocked, "{url}");
        }
    }

    #[tokio::test]
    async fn a_name_resolving_to_a_blocked_address_is_refused() {
        let rebinding: Resolve = std::sync::Arc::new(|_, port| {
            Box::pin(async move {
                Ok(vec![
                    SocketAddr::from(([140, 82, 112, 3], port)),
                    SocketAddr::from(([10, 0, 0, 5], port)),
                ])
            })
        });
        let e = check("https://github.com/o/r", &rebinding)
            .await
            .expect_err("blocked");
        assert_eq!(e.code, MirrorErrorCode::UrlBlocked);
        let public: Resolve = std::sync::Arc::new(|_, port| {
            Box::pin(async move { Ok(vec![SocketAddr::from(([140, 82, 112, 3], port))]) })
        });
        assert!(check("https://github.com/o/r", &public).await.is_ok());
        assert_eq!(
            judge(&[]).unwrap_err().code,
            MirrorErrorCode::UpstreamUnreachable
        );
    }
}

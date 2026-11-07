//! The upstream check the service makes before every poll (D41 §3.2): the
//! server's URL rules again, then the host resolved and every address it
//! resolves to refused if it is in a blocked range. This closes the
//! DNS-rebinding route round the server's string check.
//!
//! An operator may name hosts that are allowed a private address
//! (`private_hosts_allowed` in the serve config): a self-hosted forge on
//! the LAN or the tailnet. The name must match exactly, and only RFC 1918,
//! CGNAT (100.64.0.0/10) and unique-local (fc00::/7) addresses are excused
//! for it; loopback, link-local (the metadata address), and the other
//! reserved ranges stay refused, and only an `https://` upstream is
//! excused (ssh is not pinned). git is pinned to the vetted address and
//! follows no redirect, so the exemption reaches that host's address only.

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
    let seg = ip.segments();
    let first = seg[0];
    // The v4 address in bits `at..at + 32`.
    let v4_at = |at: usize| Ipv4Addr::from(((seg[at] as u32) << 16) | seg[at + 1] as u32);
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // IPv4-compatible ::a.b.c.d (deprecated), judged as its v4 address.
        || (seg[..6] == [0; 6] && v4_blocked(v4_at(6)))
        // Unique local, fc00::/7.
        || (first & 0xfe00) == 0xfc00
        // Link-local, fe80::/10, and site-local, fec0::/10 (deprecated).
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        // NAT64 64:ff9b::/96 and the local-use 64:ff9b:1::/48 can reach
        // any v4 address.
        || (first == 0x0064 && seg[1] == 0xff9b)
        // Documentation, 2001:db8::/32.
        || (first == 0x2001 && seg[1] == 0x0db8)
        // Teredo, 2001::/32: it tunnels to any v4 address.
        || (first == 0x2001 && seg[1] == 0)
        // 6to4, 2002::/16, judged as the v4 address it embeds.
        || (first == 0x2002 && v4_blocked(v4_at(1)))
}

/// Whether `ip` is in a range the mirror never connects to.
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => v6_blocked(v6),
    }
}

/// Whether `ip` is in a private range a named host may be allowed to use:
/// RFC 1918, CGNAT 100.64.0.0/10 and unique-local fc00::/7 (an IPv4-mapped
/// address judged as its v4 address). Nothing else is ever excused.
fn excusable(ip: IpAddr) -> bool {
    let v4 = |v4: Ipv4Addr| {
        let [a, b, ..] = v4.octets();
        v4.is_private() || (a == 100 && (64..128).contains(&b))
    };
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(m) => v4(m),
            None => (a.segments()[0] & 0xfe00) == 0xfc00,
        },
    }
}

/// Checks a `private_hosts_allowed` list: each entry a DNS name (no
/// wildcard, address, port or path), returned lowercase without a trailing
/// dot. An empty list, the default, allows no host a private address.
pub fn private_hosts(names: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for n in names {
        let h = n.trim_end_matches('.').to_ascii_lowercase();
        let label_ok = |l: &str| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        let tld = h.rsplit('.').next().unwrap_or("");
        if h.is_empty()
            || h.len() > 253
            || !h.split('.').all(label_ok)
            || !(tld.starts_with("xn--") || tld.bytes().all(|b| b.is_ascii_alphabetic()))
        {
            return Err(format!(
                "private_hosts_allowed: {n:?} is not a host name (no wildcards, addresses or ports)"
            ));
        }
        if check_name(Target {
            host: h.clone(),
            port: 443,
        })
        .is_err()
        {
            return Err(format!(
                "private_hosts_allowed: {n:?} is a local or cluster name"
            ));
        }
        if !out.contains(&h) {
            out.push(h);
        }
    }
    Ok(out)
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

/// Refuses `addrs` if any is blocked; none is unreachable. With `private`
/// set (the host is in `private_hosts_allowed`), an address in a private
/// range ([`excusable`]) is let through; every other blocked range is not.
pub fn judge(addrs: &[SocketAddr], private: bool) -> Result<(), PollFailure> {
    if addrs.is_empty() {
        return Err(PollFailure::new(
            MirrorErrorCode::UpstreamUnreachable,
            "the upstream host did not resolve",
        ));
    }
    if addrs
        .iter()
        .any(|a| is_blocked(a.ip()) && !(private && excusable(a.ip())))
    {
        return Err(blocked("it resolves to a private or local address"));
    }
    Ok(())
}

/// Checks `upstream` by the rules and resolves it with `resolve`; refuses
/// it if any address it resolves to is blocked. Returns the target and the
/// vetted address git must connect to: the caller pins git to it
/// ([`crate::git::Mirror::set_resolve_pin`]) so git does not resolve the
/// name again and reach another address (DNS rebinding).
///
/// `private_hosts` ([`private_hosts`], checked) names the hosts that may
/// resolve to a private address; the upstream's host must equal one
/// exactly (case and a trailing dot aside), and the upstream must be
/// `https://`: only HTTP(S) git is pinned to the vetted address, while ssh
/// resolves the name again, which would reopen DNS rebinding.
pub async fn check(
    upstream: &str,
    resolve: &Resolve,
    private_hosts: &[String],
) -> Result<(Target, IpAddr), PollFailure> {
    let t = target(upstream)?;
    let name = t.host.trim_end_matches('.').to_ascii_lowercase();
    let private = upstream.starts_with("https://")
        && name.parse::<IpAddr>().is_err()
        && private_hosts.contains(&name);
    let addrs = resolve(t.host.clone(), t.port).await.map_err(|_| {
        PollFailure::new(
            MirrorErrorCode::UpstreamUnreachable,
            "the upstream host did not resolve",
        )
    })?;
    judge(&addrs, private)?;
    let addr = addrs[0].ip();
    Ok((t, addr))
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
        let e = check("https://github.com/o/r", &rebinding, &[])
            .await
            .expect_err("blocked");
        assert_eq!(e.code, MirrorErrorCode::UrlBlocked);
        let public: Resolve = std::sync::Arc::new(|_, port| {
            Box::pin(async move { Ok(vec![SocketAddr::from(([140, 82, 112, 3], port))]) })
        });
        let (t, addr) = check("https://github.com/o/r", &public, &[])
            .await
            .expect("public");
        assert_eq!(t.host, "github.com");
        // The vetted address is returned, for git to be pinned to.
        assert_eq!(addr, IpAddr::from([140, 82, 112, 3]));
        assert_eq!(
            judge(&[], false).unwrap_err().code,
            MirrorErrorCode::UpstreamUnreachable
        );
    }

    fn resolving_to(ips: &'static [[u8; 4]]) -> Resolve {
        std::sync::Arc::new(move |_, port| {
            Box::pin(
                async move { Ok(ips.iter().map(|ip| SocketAddr::from((*ip, port))).collect()) },
            )
        })
    }

    #[tokio::test]
    async fn only_a_named_host_may_resolve_to_a_private_address() {
        let allow = private_hosts(&["Repo.Example.NET.".to_owned()]).unwrap();
        assert_eq!(allow, vec!["repo.example.net"]);
        let lan = resolving_to(&[[192, 168, 1, 75]]);
        // The named host is let through, pinned to the address it resolved to.
        let (t, addr) = check("https://repo.example.net/o/r.git", &lan, &allow)
            .await
            .expect("allowed");
        assert_eq!(t.host, "repo.example.net");
        assert_eq!(addr, IpAddr::from([192, 168, 1, 75]));
        let tailnet = resolving_to(&[[100, 92, 54, 45]]);
        check("https://REPO.example.net./o/r", &tailnet, &allow)
            .await
            .expect("case and a trailing dot aside");
        // ssh is not pinned to the vetted address, so it gets no exemption.
        for url in ["ssh://git@repo.example.net/o/r", "git@repo.example.net:o/r"] {
            let e = check(url, &tailnet, &allow).await.expect_err(url);
            assert_eq!(e.code, MirrorErrorCode::UrlBlocked, "{url}");
        }
        // Any other host on a private address is refused, a redirect target
        // included (and git follows no redirect: see git.rs).
        for url in [
            "https://other.example.net/o/r",
            "https://sub.repo.example.net/o/r",
            "https://repo.example.net.evil.org/o/r",
        ] {
            let e = check(url, &lan, &allow).await.expect_err(url);
            assert_eq!(e.code, MirrorErrorCode::UrlBlocked, "{url}");
        }
        // The default, no list, is today's behaviour.
        let e = check("https://repo.example.net/o/r", &lan, &[])
            .await
            .expect_err("default");
        assert_eq!(e.code, MirrorErrorCode::UrlBlocked);
    }

    #[tokio::test]
    async fn a_named_host_is_still_refused_loopback_link_local_and_reserved() {
        let allow = private_hosts(&["repo.example.net".to_owned()]).unwrap();
        static BAD: [[[u8; 4]; 2]; 5] = [
            [[192, 168, 1, 75], [127, 0, 0, 1]],
            [[192, 168, 1, 75], [169, 254, 169, 254]],
            [[10, 0, 0, 1], [0, 0, 0, 0]],
            [[10, 0, 0, 1], [240, 0, 0, 1]],
            [[172, 16, 0, 1], [198, 18, 0, 1]],
        ];
        for ips in &BAD {
            let e = check("https://repo.example.net/o/r", &resolving_to(ips), &allow)
                .await
                .expect_err("never excused");
            assert_eq!(e.code, MirrorErrorCode::UrlBlocked, "{ips:?}");
        }
        // Unique-local v6 and a mapped private v4 are excused; loopback and
        // link-local v6 are not.
        for (ip, ok) in [
            ("fd00::1", true),
            ("::ffff:10.1.2.3", true),
            ("::1", false),
            ("fe80::1", false),
            ("::ffff:127.0.0.1", false),
        ] {
            let addr = SocketAddr::new(ip.parse().unwrap(), 443);
            assert_eq!(judge(&[addr], true).is_ok(), ok, "{ip}");
            assert!(judge(&[addr], false).is_err(), "{ip} blocked by default");
        }
        // A literal address in the URL is never excused, whatever the list.
        let e = check(
            "https://192.168.1.75/o/r",
            &resolving_to(&[[192, 168, 1, 75]]),
            &allow,
        )
        .await
        .expect_err("literal");
        assert_eq!(e.code, MirrorErrorCode::UrlBlocked);
    }

    #[test]
    fn the_list_takes_exact_host_names_only() {
        for bad in [
            "*.example.net",
            "*",
            "192.168.1.75",
            "repo.example.net:443",
            "https://repo.example.net",
            "repo.example.net/o",
            "",
            "localhost",
            "git.internal",
            "api.default.svc",
        ] {
            assert!(private_hosts(&[bad.to_owned()]).is_err(), "{bad}");
        }
        assert_eq!(private_hosts(&[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn every_special_purpose_v6_range_is_blocked() {
        for ip in [
            // IPv4-mapped, of loopback, private and the metadata address.
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            // IPv4-compatible (deprecated) of the same.
            "::127.0.0.1",
            "::10.0.0.1",
            // NAT64 to a private address, well-known and local-use.
            "64:ff9b::10.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::10.0.0.1",
            // Unique local fc00::/7.
            "fc00::1",
            "fdff:ffff::1",
            // Link-local fe80::/10.
            "fe80::1",
            "febf::1",
            // Site-local fec0::/10 (deprecated).
            "fec0::1",
            "feff::1",
            // Multicast, unspecified, loopback.
            "ff02::1",
            "ff0e::1",
            "::",
            "::1",
            // Documentation 2001:db8::/32.
            "2001:db8::1",
            "2001:db8:ffff::1",
            // 6to4 2002::/16 embedding a private or local v4.
            "2002:0a00:0001::1",
            "2002:7f00:0001::1",
            "2002:a9fe:a9fe::1",
            "2002:c0a8:0001::1",
            // Teredo 2001::/32: tunnels to any v4 address.
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        ] {
            let addr: Ipv6Addr = ip.parse().expect(ip);
            assert!(is_blocked(IpAddr::V6(addr)), "{ip} must be blocked");
            let url = format!("https://[{ip}]/o/r");
            assert_eq!(
                target(&url).expect_err(&url).code,
                MirrorErrorCode::UrlBlocked,
                "{url}"
            );
        }
        // Public v6, and 6to4 of a public v4, still pass.
        for ip in ["2606:4700::1", "2a00:1450:4009::200e", "2002:8c52:7003::1"] {
            let addr: Ipv6Addr = ip.parse().expect(ip);
            assert!(!is_blocked(IpAddr::V6(addr)), "{ip} must pass");
        }
    }
}

//! Mirror settings (D41, `docs/notes/mirror-service.md` §3.2): the
//! `repo_mirrors` rows behind `MirrorService.SetMirror`, `GetMirror`,
//! `ListMirrors`, `RemoveMirror`, `PurgeMirror` and `SetMirrorStatus`.
//!
//! The server stores an upstream URL and branch names as strings and never
//! fetches them (rule 6). It validates the URL here, at the boundary
//! (rule 9): `https` or `ssh` only, no address the server's own network can
//! reach privately, and only a host the operator allowed. The mirror
//! service checks again after DNS resolution.
//!
//! Grants (§3.1): creating a row needs `mirror:bind`; turning on export or
//! changing the export author map needs `mirror:export`; a repository admin
//! (or a `mirror:bind` holder) may change the rest of an existing row.
//! Removal and purge need `mirror:bind`; status needs `repo:*:mirror`. A
//! purge is recorded as a mirror request (`mirror_requests`).
//!
//! `RemoveMirror --clear-followed` deletes each line's followed marker in
//! the removal's transaction, with one `RefDeleted` journal entry per
//! marker: `{"name":"graft.followed/main","old":"c…","version":3,"actor":"…"}`.
//!
//! The authority lease (§3.6): one `mirror_leases` row per repository.
//! `AcquireLease` takes it by CAS when it is free or lapsed and bumps the
//! epoch; `RenewLease` and `ReleaseLease` name the epoch they hold. Every
//! mirror write carries that epoch and commits under a `LeaseHeld`
//! precondition, so a fenced-off process gets a typed `LeaseLost` and its
//! write is not applied.

use std::net::{IpAddr, Ipv4Addr};

use crate::credentials::{CredentialError, KeyRing, MAX_CREDENTIAL, Secret};
use graft_auth::Scope;
use graft_model::{ChangesetId, ObjectId};
use graft_proto::types::followed_ref;
use graft_proto::{ApiError, Principal, PrincipalKind, pb};
use graft_store::{
    LeaseRecord, MetaStore, MetaTxn, MirrorDirection, MirrorExportRecord, MirrorLineRecord,
    MirrorQuery, MirrorRecord, MirrorStatusRecord, Precondition, RepoId, StoreError, TxnOutcome,
    Write,
};

/// The poll interval a row gets when it names none (D9).
pub(crate) const DEFAULT_INTERVAL_S: u32 = 15;

/// The longest upstream URL accepted.
const MAX_URL: usize = 2048;

/// The longest branch, line, principal or identity accepted.
const MAX_NAME: usize = 256;

/// The most lines, and the most export authors, on one row.
const MAX_LINES: usize = 64;
const MAX_AUTHORS: usize = 256;

/// The longest status message, in characters.
pub(crate) const MAX_STATUS_MESSAGE: usize = 200;

/// The most rows one `ListMirrors` page returns.
pub(crate) const MAX_PAGE: u32 = 200;

/// How long a lease lives unless renewed (§3.6). The service renews it every
/// 20 seconds.
pub(crate) const LEASE_TTL_MS: i64 = 60_000;

/// The longest lease instance id accepted.
const MAX_INSTANCE: usize = 64;

/// The journal kind of a deleted followed marker.
pub(crate) const REF_DELETED: &str = "RefDeleted";

/// The operator's mirror settings (`[mirror]` in the server config).
#[derive(Clone, Debug)]
pub(crate) struct MirrorPolicy {
    /// Hosts an upstream may name, lowercase, matched exactly.
    pub(crate) allowed_hosts: Vec<String>,
    /// The shortest poll interval a row may ask for.
    pub(crate) min_interval_s: u32,
    /// The server's own names and addresses, never an upstream.
    pub(crate) own_names: Vec<String>,
}

impl Default for MirrorPolicy {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["github.com".into()],
            min_interval_s: DEFAULT_INTERVAL_S,
            own_names: vec!["localhost".into()],
        }
    }
}

pub(crate) fn refused(policy: &str, detail: impl Into<String>) -> ApiError {
    ApiError::Rejected {
        policy: policy.to_owned(),
        detail: detail.into(),
    }
}

/// Why an upstream URL is refused. The URL itself is never echoed: it may
/// carry a credential.
fn url_refused(reason: &str) -> ApiError {
    refused("mirror-url", reason)
}

/// Checks `url` against the allowlist (§3.2). Text only: nothing is
/// resolved or fetched.
pub(crate) fn check_upstream(url: &str, policy: &MirrorPolicy) -> Result<(), ApiError> {
    if url.is_empty() || url.len() > MAX_URL {
        return Err(url_refused("the upstream URL is empty or too long"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(url_refused(
            "the upstream URL holds whitespace or a control character",
        ));
    }
    let scheme_end = url.find("://");
    // A transport helper, `<x>::<address>`: git would run `git-remote-<x>`.
    if let Some(i) = url.find("::")
        && scheme_end.is_none_or(|s| i < s)
    {
        return Err(url_refused("transport helpers (<x>::) are not allowed"));
    }
    let host = match scheme_end {
        Some(s) => {
            let scheme = url[..s].to_ascii_lowercase();
            if scheme != "https" && scheme != "ssh" {
                return Err(url_refused("the scheme must be https or ssh"));
            }
            let rest = &url[s + 3..];
            let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
            let host_port = match authority.rsplit_once('@') {
                Some(_) if scheme == "https" => {
                    return Err(url_refused("an https upstream may not carry user info"));
                }
                Some((_, hp)) => hp,
                None => authority,
            };
            host_of(host_port, true)?
        }
        None => {
            // The scp-like form `[user@]host:path`.
            let Some((before, path)) = url.split_once(':') else {
                return Err(url_refused(
                    "the upstream must be https://, ssh:// or user@host:path",
                ));
            };
            if before.contains('/') || before.starts_with('.') || path.is_empty() {
                return Err(url_refused("local paths are not allowed"));
            }
            let host = before.rsplit_once('@').map_or(before, |(_, h)| h);
            host_of(host, false)?
        }
    };
    check_host(&host, policy)
}

/// The host of `host[:port]` (or `[v6][:port]`), lowercased, without a
/// trailing dot.
fn host_of(hp: &str, port_allowed: bool) -> Result<String, ApiError> {
    let (host, port) = if let Some(rest) = hp.strip_prefix('[') {
        let (h, after) = rest
            .split_once(']')
            .ok_or_else(|| url_refused("an unclosed IPv6 address"))?;
        (h, after.strip_prefix(':'))
    } else {
        match hp.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hp, None),
        }
    };
    if let Some(p) = port
        && (!port_allowed || p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(url_refused("a bad port"));
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(url_refused("the upstream names no host"));
    }
    Ok(host)
}

fn check_host(host: &str, policy: &MirrorPolicy) -> Result<(), ApiError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public(ip) {
            return Err(url_refused(
                "loopback, link-local, private, CGNAT and unique-local addresses are not allowed",
            ));
        }
    } else {
        let dns = host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        // The last label must be alphabetic (or IDNA), so a numeric form such
        // as `2130706433` or `0x7f.1` is never read as an address.
        let tld = host.rsplit('.').next().unwrap_or("");
        let tld_ok = tld.starts_with("xn--") || tld.bytes().all(|b| b.is_ascii_alphabetic());
        if !dns || !tld_ok || host.len() > 253 {
            return Err(url_refused("the host is not a DNS name or an address"));
        }
        let blocked_suffix = ["localhost", "svc", "cluster.local"]
            .iter()
            .any(|s| host == *s || host.ends_with(&format!(".{s}")));
        if blocked_suffix {
            return Err(url_refused(
                "localhost and cluster service names are not allowed",
            ));
        }
    }
    if policy
        .own_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case(host))
    {
        return Err(url_refused("the server's own name is not an upstream"));
    }
    if !policy
        .allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host))
    {
        return Err(url_refused("the host is not in mirror.allowed_hosts"));
    }
    Ok(())
}

/// Whether `ip` is a public unicast address.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (first & 0xffc0) == 0xfe80) // link-local fe80::/10
        }
    }
}

fn public_v4(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    !(v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))) // CGNAT 100.64.0.0/10
}

/// A branch, line, principal or identity: non-empty, bounded, no control
/// characters.
pub(crate) fn name(field: &str, s: &str) -> Result<String, ApiError> {
    if s.is_empty() || s.len() > MAX_NAME || s.chars().any(char::is_control) {
        return Err(refused(
            "mirror-field",
            format!("{field} must be 1..={MAX_NAME} bytes with no control characters"),
        ));
    }
    Ok(s.to_owned())
}

fn lines(wire: Vec<pb::MirrorLine>) -> Result<Vec<MirrorLineRecord>, ApiError> {
    if wire.is_empty() || wire.len() > MAX_LINES {
        return Err(refused(
            "mirror-field",
            format!("a mirror has 1..={MAX_LINES} lines"),
        ));
    }
    let mut out: Vec<MirrorLineRecord> = Vec::with_capacity(wire.len());
    for l in wire {
        let line = MirrorLineRecord {
            branch: name("lines.branch", &l.branch)?,
            line: name("lines.line", &l.line)?,
            direction: MirrorDirection::parse(&l.direction).ok_or_else(|| {
                refused("mirror-field", "lines.direction is follow, export or both")
            })?,
        };
        if out.iter().any(|o| o.line == line.line) {
            return Err(refused("mirror-field", "a line is mirrored once per row"));
        }
        out.push(line);
    }
    Ok(out)
}

fn export(wire: Option<pb::MirrorExport>) -> Result<MirrorExportRecord, ApiError> {
    let wire = wire.unwrap_or_default();
    if wire.authors.len() > MAX_AUTHORS {
        return Err(refused(
            "mirror-field",
            format!("at most {MAX_AUTHORS} export authors"),
        ));
    }
    let mut authors = wire
        .authors
        .into_iter()
        .map(|a| {
            Ok((
                name("export.authors.principal", &a.principal)?,
                name("export.authors.identity", &a.identity)?,
            ))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    authors.sort();
    authors.dedup_by(|a, b| a.0 == b.0);
    Ok(MirrorExportRecord {
        default_author: wire
            .default_author
            .map(|a| name("export.default_author", &a))
            .transpose()?,
        authors,
    })
}

/// Holds any of `scopes`; the refusal names the first.
pub(crate) fn any_of(p: &Principal, scopes: &[Scope]) -> Result<(), ApiError> {
    if scopes
        .iter()
        .any(|s| graft_auth::authorize(p, s, None).is_ok())
    {
        return Ok(());
    }
    Err(ApiError::PermissionDenied {
        scope: scopes[0].to_string(),
        path: None,
    })
}

/// Whether `new` turns on export somewhere `old` did not, or changes the
/// author map: either needs `mirror:export`.
fn changes_export(
    old: Option<&MirrorRecord>,
    lines: &[MirrorLineRecord],
    export: &MirrorExportRecord,
) -> bool {
    let exported = |l: &MirrorLineRecord| l.direction.exports();
    let new_export = lines
        .iter()
        .filter(|l| exported(l))
        .any(|l| !old.is_some_and(|o| o.lines.iter().any(|ol| ol == l)));
    let map_changed = match old {
        Some(o) => o.export != *export,
        None => *export != MirrorExportRecord::default(),
    };
    new_export || map_changed
}

pub(crate) fn store(e: StoreError) -> ApiError {
    tracing::error!(error = ?e, "mirror store call failed");
    ApiError::Unavailable {
        retry_after_ms: 1_000,
    }
}

pub(crate) async fn repo_id<M: MetaStore>(meta: &M, repo: &str) -> Result<RepoId, ApiError> {
    meta.repo(repo)
        .await
        .map_err(store)?
        .ok_or_else(|| ApiError::NotFound {
            what: format!("repository {repo}"),
        })
}

fn version_conflict(expected: Option<u64>, actual: Option<u64>) -> ApiError {
    let show = |v: Option<u64>| v.map_or_else(|| "absent".to_owned(), |v| v.to_string());
    refused(
        "mirror-version",
        format!(
            "expected version {}, the row is {}",
            show(expected),
            show(actual)
        ),
    )
}

fn not_found(repo: &str) -> ApiError {
    ApiError::NotFound {
        what: format!("mirror of repository {repo}"),
    }
}

/// The wire form of a row. Never the credential: only whether one is set.
pub(crate) fn view(repo: &str, r: MirrorRecord) -> pb::Mirror {
    pb::Mirror {
        repo: repo.to_owned(),
        upstream: r.upstream,
        lines: r
            .lines
            .into_iter()
            .map(|l| pb::MirrorLine {
                branch: l.branch,
                line: l.line,
                direction: l.direction.as_str().to_owned(),
            })
            .collect(),
        interval_s: r.interval_s,
        enabled: r.enabled,
        credential_set: r.credential.is_some(),
        kek_id: r.kek_id,
        export: Some(pb::MirrorExport {
            default_author: r.export.default_author,
            authors: r
                .export
                .authors
                .into_iter()
                .map(|(principal, identity)| pb::MirrorAuthor {
                    principal,
                    identity,
                })
                .collect(),
        }),
        version: r.version,
        status: r.status.map(status_view),
        created_by: r.created_by,
        updated_by: r.updated_by,
        updated_at_ms: r.updated_at_ms,
    }
}

fn status_view(s: MirrorStatusRecord) -> pb::MirrorStatus {
    pb::MirrorStatus {
        last_poll_at_ms: s.last_poll_at_ms,
        last_success_at_ms: s.last_success_at_ms,
        last_imported_sha: s.last_imported_sha,
        last_changeset: s.last_changeset.map(|c| c.0.0.to_vec()),
        lag_changesets: s.lag_changesets,
        lag_seconds: s.lag_seconds,
        error_code: s
            .error_code
            .as_deref()
            .and_then(|c| {
                pb::MirrorErrorCode::from_str_name(&format!(
                    "{CODE_PREFIX}{}",
                    c.to_ascii_uppercase()
                ))
            })
            .unwrap_or(pb::MirrorErrorCode::Unspecified) as i32,
        error_message: s.error_message,
        backoff_until_ms: s.backoff_until_ms,
        lease_epoch: s.lease_epoch,
    }
}

/// `SetMirror`: create or replace a row's settings by CAS on `version`.
pub(crate) async fn set<M: MetaStore>(
    meta: &M,
    p: &Principal,
    policy: &MirrorPolicy,
    now_ms: i64,
    req: pb::SetMirrorRequest,
) -> Result<pb::Mirror, ApiError> {
    check_upstream(&req.upstream, policy)?;
    let interval_s = match req.interval_s {
        0 => DEFAULT_INTERVAL_S.max(policy.min_interval_s),
        n if n < policy.min_interval_s => {
            return Err(refused(
                "mirror-interval",
                format!("interval_s must be at least {}", policy.min_interval_s),
            ));
        }
        n => n,
    };
    let lines = lines(req.lines)?;
    let export = export(req.export)?;
    let id = repo_id(meta, &req.repo).await?;
    let old = meta.get_mirror(id, &req.upstream).await.map_err(store)?;
    // Grants first, so a caller without them learns nothing of the row.
    match &old {
        None => any_of(p, &[Scope::MirrorBind])?,
        Some(_) => any_of(
            p,
            &[
                Scope::Admin {
                    repo: req.repo.clone(),
                },
                Scope::MirrorBind,
            ],
        )?,
    }
    if changes_export(old.as_ref(), &lines, &export) {
        any_of(p, &[Scope::MirrorExport])?;
    }
    let current = old.as_ref().map(|o| o.version);
    if req.expected_version != current {
        return Err(version_conflict(req.expected_version, current));
    }
    let record = MirrorRecord {
        upstream: req.upstream.clone(),
        lines,
        interval_s,
        enabled: req.enabled,
        credential: old.as_ref().and_then(|o| o.credential.clone()),
        kek_id: old.as_ref().and_then(|o| o.kek_id.clone()),
        export,
        version: current.unwrap_or(0) + 1,
        status: old.as_ref().and_then(|o| o.status.clone()),
        created_by: old
            .as_ref()
            .map_or_else(|| p.subject.clone(), |o| o.created_by.clone()),
        updated_by: p.subject.clone(),
        updated_at_ms: now_ms,
    };
    let txn = MetaTxn {
        preconditions: vec![Precondition::MirrorVersionIs {
            upstream: req.upstream.clone(),
            version: current,
        }],
        writes: vec![Write::PutMirror(record.clone())],
    };
    match meta.commit(id, txn).await.map_err(store)? {
        TxnOutcome::Committed(_) => Ok(view(&req.repo, record)),
        TxnOutcome::PreconditionFailed(_) => {
            let now = meta.get_mirror(id, &req.upstream).await.map_err(store)?;
            Err(version_conflict(current, now.map(|r| r.version)))
        }
    }
}

/// The read grant on one repository's rows.
pub(crate) fn may_read(p: &Principal, repo: &str) -> Result<(), ApiError> {
    any_of(
        p,
        &[
            Scope::Read {
                repo: repo.to_owned(),
            },
            Scope::Mirror {
                repo: repo.to_owned(),
            },
        ],
    )
}

/// `GetMirror`.
pub(crate) async fn get<M: MetaStore>(
    meta: &M,
    p: &Principal,
    req: pb::MirrorKey,
) -> Result<pb::Mirror, ApiError> {
    may_read(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    let row = meta.get_mirror(id, &req.upstream).await.map_err(store)?;
    row.map(|r| view(&req.repo, r))
        .ok_or_else(|| not_found(&req.repo))
}

fn page_token(repo: RepoId, upstream: &str) -> String {
    format!("{}:{upstream}", repo.0)
}

fn parse_token(t: &str) -> Result<Option<(RepoId, String)>, ApiError> {
    if t.is_empty() {
        return Ok(None);
    }
    let bad = || refused("mirror-field", "page_token is not one this server issued");
    let (id, upstream) = t.split_once(':').ok_or_else(bad)?;
    let id = id.parse::<u32>().map_err(|_| bad())?;
    Ok(Some((RepoId(id), upstream.to_owned())))
}

/// `ListMirrors`: one repository's rows, or every repository's.
pub(crate) async fn list<M: MetaStore>(
    meta: &M,
    p: &Principal,
    req: pb::ListMirrorsRequest,
) -> Result<pb::MirrorPage, ApiError> {
    let repo = if req.repo.is_empty() {
        let any = graft_auth::ANY_REPO.to_owned();
        any_of(
            p,
            &[
                Scope::Read { repo: any.clone() },
                Scope::Mirror { repo: any },
                Scope::ReposAdmin,
            ],
        )?;
        None
    } else {
        may_read(p, &req.repo)?;
        Some(repo_id(meta, &req.repo).await?)
    };
    let limit = match req.limit {
        0 => MAX_PAGE,
        n => n.min(MAX_PAGE),
    };
    let rows = meta
        .list_mirrors(MirrorQuery {
            repo,
            enabled: req.enabled,
            after: parse_token(&req.page_token)?,
            limit,
        })
        .await
        .map_err(store)?;
    let next_page_token = match rows.last() {
        Some(last) if rows.len() == limit as usize => page_token(last.repo, &last.record.upstream),
        _ => String::new(),
    };
    Ok(pb::MirrorPage {
        mirrors: rows
            .into_iter()
            .map(|l| view(&l.repo_name, l.record))
            .collect(),
        next_page_token,
    })
}

/// The `RefDeleted` journal body of a cleared marker.
fn ref_deleted(name: &str, old: ChangesetId, version: u64, actor: &str) -> Vec<u8> {
    serde_json::json!({
        "name": name,
        "old": old.to_string(),
        "version": version,
        "actor": actor,
    })
    .to_string()
    .into_bytes()
}

/// `RemoveMirror`: delete the row and, with `clear_followed`, its lines'
/// followed markers, in one transaction.
pub(crate) async fn remove<M: MetaStore>(
    meta: &M,
    p: &Principal,
    req: pb::RemoveMirrorRequest,
) -> Result<pb::RemoveMirrorResult, ApiError> {
    any_of(p, &[Scope::MirrorBind])?;
    let id = repo_id(meta, &req.repo).await?;
    let row = meta
        .get_mirror(id, &req.upstream)
        .await
        .map_err(store)?
        .ok_or_else(|| not_found(&req.repo))?;
    if req.expected_version.is_some_and(|v| v != row.version) {
        return Err(version_conflict(req.expected_version, Some(row.version)));
    }
    let mut txn = MetaTxn {
        preconditions: vec![Precondition::MirrorVersionIs {
            upstream: req.upstream.clone(),
            version: Some(row.version),
        }],
        writes: Vec::new(),
    };
    let mut cleared = Vec::new();
    if req.clear_followed {
        for line in &row.lines {
            let name = followed_ref(&line.line);
            let Some(marker) = meta.get_ref(id, &name).await.map_err(store)? else {
                continue;
            };
            txn.preconditions.push(Precondition::RefIs {
                name: name.clone(),
                expected: Some((marker.target, marker.version)),
            });
            txn.writes.push(Write::DeleteRef(name.clone()));
            txn.writes.push(Write::Journal {
                kind: REF_DELETED.to_owned(),
                body: ref_deleted(&name, marker.target, marker.version, &p.subject),
            });
            cleared.push(name);
        }
    }
    txn.writes.push(Write::DeleteMirror(req.upstream.clone()));
    match meta.commit(id, txn).await.map_err(store)? {
        TxnOutcome::Committed(_) => Ok(pb::RemoveMirrorResult { cleared }),
        // The row or a marker moved since the read: nothing was removed.
        TxnOutcome::PreconditionFailed(_) => Err(refused(
            "mirror-version",
            "the row or a followed marker changed; read it again and retry",
        )),
    }
}

/// `PurgeMirror`: only for a removed or disabled row. Records a purge
/// request (§3.5); the mirror service deletes the data and releases the
/// lease.
pub(crate) async fn purge<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::PurgeMirrorRequest,
) -> Result<pb::MirrorRequest, ApiError> {
    any_of(p, &[Scope::MirrorBind])?;
    let upstream = name("upstream", &req.upstream)?;
    let id = repo_id(meta, &req.repo).await?;
    if let Some(row) = meta.get_mirror(id, &req.upstream).await.map_err(store)?
        && row.enabled
    {
        return Err(refused(
            "mirror-enabled",
            "disable or remove the mirror before purging its data",
        ));
    }
    crate::mirror_requests::record_purge(
        meta,
        p,
        now_ms,
        &req.repo,
        upstream,
        &req.client_request_id,
    )
    .await
}

/// The wire prefix of every `MirrorErrorCode` name.
const CODE_PREFIX: &str = "MIRROR_ERROR_CODE_";

/// The text form of a status error code, as stored: `timeout`,
/// `auth_failed`, ….
fn error_code(code: i32) -> Result<Option<String>, ApiError> {
    let code = pb::MirrorErrorCode::try_from(code)
        .map_err(|_| refused("mirror-field", "status.error_code is not a known code"))?;
    Ok(match code {
        pb::MirrorErrorCode::Unspecified => None,
        c => Some(
            c.as_str_name()
                .trim_start_matches(CODE_PREFIX)
                .to_ascii_lowercase(),
        ),
    })
}

/// `SetMirrorStatus`: operational state from the mirror service. Not
/// journalled; it never creates a row. The lease-epoch check arrives with
/// the lease (WI-3309).
pub(crate) async fn set_status<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::SetMirrorStatusRequest,
) -> Result<(), ApiError> {
    lease_caller(p, &req.repo)?;
    let s = req.status.unwrap_or_default();
    let message = s
        .error_message
        .map(|m| {
            if m.chars().count() > MAX_STATUS_MESSAGE || m.chars().any(char::is_control) {
                Err(refused(
                    "mirror-field",
                    format!(
                        "status.error_message is at most {MAX_STATUS_MESSAGE} characters with no control characters"
                    ),
                ))
            } else {
                Ok(m)
            }
        })
        .transpose()?;
    let sha = s
        .last_imported_sha
        .map(|v| name("status.last_imported_sha", &v))
        .transpose()?;
    let changeset = s
        .last_changeset
        .map(|b| {
            <[u8; 32]>::try_from(b.as_slice())
                .map(|a| ChangesetId(ObjectId(a)))
                .map_err(|_| refused("mirror-field", "status.last_changeset is not a 32-byte id"))
        })
        .transpose()?;
    let status = MirrorStatusRecord {
        last_poll_at_ms: s.last_poll_at_ms,
        last_success_at_ms: s.last_success_at_ms,
        last_imported_sha: sha,
        last_changeset: changeset,
        lag_changesets: s.lag_changesets,
        lag_seconds: s.lag_seconds,
        error_code: error_code(s.error_code)?,
        error_message: message,
        backoff_until_ms: s.backoff_until_ms,
        lease_epoch: s.lease_epoch,
    };
    let id = repo_id(meta, &req.repo).await?;
    if meta
        .get_mirror(id, &req.upstream)
        .await
        .map_err(store)?
        .is_none()
    {
        return Err(not_found(&req.repo));
    }
    // The holder is checked here and the epoch again in the transaction;
    // an epoch names one holder, so the pair cannot change in between.
    caller_lease(meta, p, id, &req.repo, status.lease_epoch, now_ms).await?;
    let txn = MetaTxn {
        preconditions: vec![Precondition::LeaseHeld {
            epoch: status.lease_epoch,
            now_ms,
        }],
        writes: vec![Write::SetMirrorStatus {
            upstream: req.upstream,
            status,
        }],
    };
    match meta.commit(id, txn).await.map_err(store)? {
        TxnOutcome::Committed(_) => Ok(()),
        TxnOutcome::PreconditionFailed(_) => Err(lease_lost(&req.repo)),
    }
}

pub(crate) fn lease_lost(repo: &str) -> ApiError {
    ApiError::LeaseLost {
        repo: repo.to_owned(),
    }
}

/// Lease calls are the mirror permission set on a mirror-kind token only.
pub(crate) fn lease_caller(p: &Principal, repo: &str) -> Result<(), ApiError> {
    any_of(
        p,
        &[Scope::Mirror {
            repo: repo.to_owned(),
        }],
    )?;
    if p.kind != PrincipalKind::Mirror {
        return Err(ApiError::PermissionDenied {
            scope: format!("repo:{repo}:mirror"),
            path: None,
        });
    }
    Ok(())
}

fn lease_view(repo: &str, l: &LeaseRecord) -> pb::Lease {
    pb::Lease {
        repo: repo.to_owned(),
        epoch: l.epoch,
        expires_at_ms: l.expires_at_ms,
    }
}

/// Whether the lease `holder` (`<subject>/<instance>`) belongs to exactly
/// `subject`. An instance never holds a `/` (acquire refuses one), so a
/// subject that is a prefix of another (`m` and `m/x`) does not match.
fn held_by(holder: Option<&str>, subject: &str) -> bool {
    holder
        .and_then(|h| h.strip_prefix(subject))
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|instance| !instance.is_empty() && !instance.contains('/'))
}

/// The lease when the caller `p` holds it under `epoch` now; else
/// `LeaseLost`. Holder and epoch are both checked: the epoch alone is a
/// small integer another mirror subject could guess.
async fn caller_lease<M: MetaStore>(
    meta: &M,
    p: &Principal,
    id: RepoId,
    repo: &str,
    epoch: u64,
    now_ms: i64,
) -> Result<LeaseRecord, ApiError> {
    meta.get_lease(id)
        .await
        .map_err(store)?
        .filter(|l| l.held(epoch, now_ms) && held_by(l.holder.as_deref(), &p.subject))
        .ok_or_else(|| lease_lost(repo))
}

/// Writes `next` under a CAS on the stored epoch `expected`.
async fn put_lease<M: MetaStore>(
    meta: &M,
    id: RepoId,
    repo: &str,
    expected: u64,
    next: LeaseRecord,
) -> Result<LeaseRecord, ApiError> {
    let txn = MetaTxn {
        preconditions: vec![Precondition::LeaseEpochIs { epoch: expected }],
        writes: vec![Write::PutLease(next.clone())],
    };
    match meta.commit(id, txn).await.map_err(store)? {
        TxnOutcome::Committed(_) => Ok(next),
        TxnOutcome::PreconditionFailed(_) => Err(lease_lost(repo)),
    }
}

/// `AcquireLease` (§3.6): takes the lease when it is free or lapsed, under
/// the next epoch. A lease another holder still holds is `LeaseLost`, as is
/// losing the race to a concurrent acquirer.
pub(crate) async fn acquire_lease<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::AcquireLeaseRequest,
) -> Result<pb::Lease, ApiError> {
    lease_caller(p, &req.repo)?;
    if req.instance.is_empty()
        || req.instance.len() > MAX_INSTANCE
        || req
            .instance
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '/')
    {
        return Err(refused(
            "mirror-field",
            format!("instance is 1 to {MAX_INSTANCE} bytes with no whitespace or '/'"),
        ));
    }
    let id = repo_id(meta, &req.repo).await?;
    let current = meta.get_lease(id).await.map_err(store)?;
    let epoch = current.as_ref().map_or(0, |l| l.epoch);
    if current
        .as_ref()
        .is_some_and(|l| l.holder.is_some() && now_ms < l.expires_at_ms)
    {
        return Err(lease_lost(&req.repo));
    }
    let next = LeaseRecord {
        holder: Some(format!("{}/{}", p.subject, req.instance)),
        epoch: epoch + 1,
        expires_at_ms: now_ms.saturating_add(LEASE_TTL_MS),
    };
    let l = put_lease(meta, id, &req.repo, epoch, next).await?;
    Ok(lease_view(&req.repo, &l))
}

/// `RenewLease`: extends a lease still held under `epoch` by the TTL.
pub(crate) async fn renew_lease<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::LeaseRequest,
) -> Result<pb::Lease, ApiError> {
    lease_caller(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    let l = caller_lease(meta, p, id, &req.repo, req.epoch, now_ms).await?;
    let next = LeaseRecord {
        expires_at_ms: now_ms.saturating_add(LEASE_TTL_MS),
        ..l
    };
    let l = put_lease(meta, id, &req.repo, req.epoch, next).await?;
    Ok(lease_view(&req.repo, &l))
}

/// `ReleaseLease`: frees a lease held under `epoch`, keeping the epoch so
/// the next acquire moves past it. Releasing a lease already lost is
/// `LeaseLost`.
pub(crate) async fn release_lease<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::LeaseRequest,
) -> Result<(), ApiError> {
    lease_caller(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    caller_lease(meta, p, id, &req.repo, req.epoch, now_ms).await?;
    let next = LeaseRecord {
        holder: None,
        epoch: req.epoch,
        expires_at_ms: now_ms,
    };
    put_lease(meta, id, &req.repo, req.epoch, next).await?;
    Ok(())
}

fn credential_refused(e: CredentialError) -> ApiError {
    refused("mirror-credential", e.to_string())
}

/// `SetMirrorCredential` (§3.4): write-only, `mirror:bind`. Seals the
/// credential under the active KEK (an empty one clears it) and bumps the
/// row's version, by CAS on `expected_version`.
pub(crate) async fn set_credential<M: MetaStore>(
    meta: &M,
    p: &Principal,
    ring: &KeyRing,
    now_ms: i64,
    req: pb::SetMirrorCredentialRequest,
) -> Result<pb::Mirror, ApiError> {
    any_of(p, &[Scope::MirrorBind])?;
    if req.credential.len() > MAX_CREDENTIAL {
        return Err(refused(
            "mirror-field",
            format!("credential is at most {MAX_CREDENTIAL} bytes"),
        ));
    }
    // A line break or NUL inside would add lines to git's credential
    // protocol; trailing whitespace (a file's last newline) is trimmed by
    // the reader.
    if req
        .credential
        .trim_ascii()
        .iter()
        .any(|b| matches!(b, b'\n' | b'\r' | 0))
    {
        return Err(refused(
            "mirror-field",
            "credential must not hold a line break or NUL",
        ));
    }
    let id = repo_id(meta, &req.repo).await?;
    let Some(old) = meta.get_mirror(id, &req.upstream).await.map_err(store)? else {
        return Err(not_found(&req.repo));
    };
    if req.expected_version != old.version {
        return Err(version_conflict(
            Some(req.expected_version),
            Some(old.version),
        ));
    }
    let sealed = if req.credential.is_empty() {
        None
    } else {
        let plain = Secret::new(req.credential);
        Some(
            ring.seal(id, &req.upstream, plain.expose())
                .map_err(credential_refused)?,
        )
    };
    let record = MirrorRecord {
        kek_id: sealed.as_ref().map(|s| s.kek_id.clone()),
        credential: sealed.map(|s| s.envelope),
        version: old.version + 1,
        updated_by: p.subject.clone(),
        updated_at_ms: now_ms,
        ..old
    };
    let txn = MetaTxn {
        preconditions: vec![Precondition::MirrorVersionIs {
            upstream: req.upstream.clone(),
            version: Some(req.expected_version),
        }],
        writes: vec![Write::PutMirror(record.clone())],
    };
    match meta.commit(id, txn).await.map_err(store)? {
        TxnOutcome::Committed(_) => Ok(view(&req.repo, record)),
        TxnOutcome::PreconditionFailed(_) => {
            let now = meta.get_mirror(id, &req.upstream).await.map_err(store)?;
            Err(version_conflict(
                Some(req.expected_version),
                now.map(|r| r.version),
            ))
        }
    }
}

/// `GetMirrorCredential` (§3.4): the plaintext, only to a `mirror`-kind
/// token whose subject holds the current lease `epoch` on the repository.
/// A token of another kind or without the grant is `PermissionDenied`; a
/// stale or foreign epoch is `LeaseLost`.
pub(crate) async fn get_credential<M: MetaStore>(
    meta: &M,
    p: &Principal,
    ring: &KeyRing,
    now_ms: i64,
    req: pb::GetMirrorCredentialRequest,
) -> Result<pb::MirrorCredential, ApiError> {
    lease_caller(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    caller_lease(meta, p, id, &req.repo, req.epoch, now_ms).await?;
    let row = meta
        .get_mirror(id, &req.upstream)
        .await
        .map_err(store)?
        .ok_or_else(|| not_found(&req.repo))?;
    let (Some(envelope), Some(kek_id)) = (row.credential, row.kek_id) else {
        return Err(ApiError::NotFound {
            what: format!("credential of mirror of repository {}", req.repo),
        });
    };
    let plain = ring
        .open(id, &req.upstream, &kek_id, &envelope)
        .map_err(|e| {
            tracing::error!(repo = %req.repo, kek_id = %kek_id, error = %e, "mirror credential did not open");
            credential_refused(e)
        })?;
    Ok(pb::MirrorCredential {
        credential: plain.expose().to_vec(),
        kek_id,
    })
}

/// What `admin mirror rewrap` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RewrapReport {
    /// Rows re-wrapped under the active key.
    pub(crate) rewrapped: u64,
    /// Rows already under it, or with no credential.
    pub(crate) unchanged: u64,
    /// Rows that did not open (unknown key or tampered), left as they are.
    pub(crate) failed: Vec<(String, String)>,
}

/// How many times one row's rewrap retries a lost CAS before reporting it.
const REWRAP_TRIES: usize = 8;

/// `admin mirror rewrap` (§3.4): re-wraps every row's data key under the
/// active KEK, one transaction per row, by CAS on its version. Pages
/// through the rows; a row changed concurrently is re-read and retried.
pub(crate) async fn rewrap_all<M: MetaStore>(
    meta: &M,
    ring: &KeyRing,
    now_ms: i64,
) -> Result<RewrapReport, ApiError> {
    if ring.active_id().is_none() {
        return Err(credential_refused(CredentialError::NoActiveKey));
    }
    let mut report = RewrapReport::default();
    let mut after = None;
    loop {
        let rows = meta
            .list_mirrors(MirrorQuery {
                repo: None,
                enabled: None,
                after: after.clone(),
                limit: MAX_PAGE,
            })
            .await
            .map_err(store)?;
        let Some(last) = rows.last() else { break };
        after = Some((last.repo, last.record.upstream.clone()));
        let full = rows.len() == MAX_PAGE as usize;
        for l in rows {
            rewrap_row(
                meta,
                ring,
                now_ms,
                l.repo,
                &l.repo_name,
                l.record,
                &mut report,
            )
            .await?;
        }
        if !full {
            break;
        }
    }
    Ok(report)
}

async fn rewrap_row<M: MetaStore>(
    meta: &M,
    ring: &KeyRing,
    now_ms: i64,
    id: RepoId,
    repo: &str,
    mut row: MirrorRecord,
    report: &mut RewrapReport,
) -> Result<(), ApiError> {
    for _ in 0..REWRAP_TRIES {
        let (Some(envelope), Some(kek_id)) = (&row.credential, &row.kek_id) else {
            report.unchanged += 1;
            return Ok(());
        };
        let sealed = match ring.rewrap(id, &row.upstream, kek_id, envelope) {
            Ok(Some(s)) => s,
            Ok(None) => {
                report.unchanged += 1;
                return Ok(());
            }
            Err(e) => {
                tracing::error!(repo, kek_id = %kek_id, error = %e, "mirror credential did not rewrap");
                report.failed.push((repo.to_owned(), row.upstream.clone()));
                return Ok(());
            }
        };
        let upstream = row.upstream.clone();
        let version = row.version;
        let record = MirrorRecord {
            credential: Some(sealed.envelope),
            kek_id: Some(sealed.kek_id),
            version: version + 1,
            updated_by: "operator".to_owned(),
            updated_at_ms: now_ms,
            ..row
        };
        let txn = MetaTxn {
            preconditions: vec![Precondition::MirrorVersionIs {
                upstream: upstream.clone(),
                version: Some(version),
            }],
            writes: vec![Write::PutMirror(record)],
        };
        match meta.commit(id, txn).await.map_err(store)? {
            TxnOutcome::Committed(_) => {
                report.rewrapped += 1;
                return Ok(());
            }
            TxnOutcome::PreconditionFailed(_) => {
                match meta.get_mirror(id, &upstream).await.map_err(store)? {
                    Some(r) => row = r,
                    None => return Ok(()),
                }
            }
        }
    }
    report.failed.push((repo.to_owned(), row.upstream));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_proto::PrincipalKind;
    use graft_store::memory::MemoryStore;
    use graft_store::{RefKind, RefRecord};

    const UP: &str = "https://github.com/o/r.git";

    fn who(scopes: &[&str]) -> Principal {
        Principal {
            subject: "p".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn policy() -> MirrorPolicy {
        MirrorPolicy {
            allowed_hosts: vec!["github.com".into()],
            min_interval_s: 15,
            own_names: vec!["localhost".into(), "graft.example.com".into()],
        }
    }

    fn line(branch: &str, direction: &str) -> pb::MirrorLine {
        pb::MirrorLine {
            branch: branch.into(),
            line: branch.into(),
            direction: direction.into(),
        }
    }

    fn req(
        upstream: &str,
        expected: Option<u64>,
        lines: Vec<pb::MirrorLine>,
    ) -> pb::SetMirrorRequest {
        pb::SetMirrorRequest {
            repo: "r".into(),
            upstream: upstream.into(),
            lines,
            interval_s: 0,
            enabled: true,
            export: None,
            expected_version: expected,
        }
    }

    /// A mirror-kind token holding the mirror permission set.
    fn mirror_token(subject: &str) -> Principal {
        Principal {
            subject: subject.into(),
            kind: PrincipalKind::Mirror,
            ..who(&["repo:*:mirror"])
        }
    }

    /// Takes the lease on `r` at `now_ms`, returning its epoch.
    async fn take(s: &impl MetaStore, p: &Principal, now_ms: i64) -> Result<u64, ApiError> {
        acquire_lease(
            s,
            p,
            now_ms,
            pb::AcquireLeaseRequest {
                repo: "r".into(),
                instance: "i1".into(),
            },
        )
        .await
        .map(|l| l.epoch)
    }

    fn lost<T: std::fmt::Debug>(r: Result<T, ApiError>) {
        assert!(
            matches!(r, Err(ApiError::LeaseLost { ref repo }) if repo == "r"),
            "{r:?}"
        );
    }

    fn held(epoch: u64) -> pb::LeaseRequest {
        pb::LeaseRequest {
            repo: "r".into(),
            epoch,
        }
    }

    async fn store() -> MemoryStore {
        let s = MemoryStore::new();
        s.create_repo("r").await.expect("repo");
        s
    }

    fn denied(r: Result<impl std::fmt::Debug, ApiError>, scope: &str) {
        match r {
            Err(ApiError::PermissionDenied { scope: s, .. }) => assert_eq!(s, scope),
            other => panic!("expected a denial of {scope}, got {other:?}"),
        }
    }

    fn rejected(r: Result<impl std::fmt::Debug, ApiError>, policy: &str) {
        match r {
            Err(ApiError::Rejected { policy: p, .. }) => assert_eq!(p, policy),
            other => panic!("expected {policy}, got {other:?}"),
        }
    }

    #[test]
    fn https_and_ssh_upstreams_on_an_allowed_host_pass() {
        for url in [
            UP,
            "https://GitHub.com./o/r",
            "https://github.com:443/o/r",
            "ssh://git@github.com/o/r.git",
            "ssh://git@github.com:22/o/r.git",
            "git@github.com:o/r.git",
            "github.com:o/r.git",
        ] {
            assert_eq!(check_upstream(url, &policy()), Ok(()), "{url}");
        }
    }

    /// Each blocked class is refused even when its host is allowlisted, so
    /// the class check, not the allowlist, is what refuses it.
    #[test]
    fn every_blocked_url_class_is_refused() {
        let cases: &[(&str, &str)] = &[
            // Schemes and transports.
            ("file:///srv/repo.git", ""),
            ("git://github.com/o/r", ""),
            ("http://github.com/o/r", ""),
            ("ext::sh -c touch% /tmp/x", ""),
            ("ext::sh", ""),
            ("fd::17", ""),
            ("foo::https://github.com/o/r", ""),
            ("/srv/repo.git", ""),
            ("./repo:x", ""),
            ("../x/y:z", ""),
            ("", ""),
            ("https://github.com/o r", ""),
            ("https://tok@github.com/o/r", "github.com"),
            ("https://github.com:x/o/r", "github.com"),
            // Loopback.
            ("https://127.0.0.1/o/r", "127.0.0.1"),
            ("https://127.1.2.3/o/r", "127.1.2.3"),
            ("https://[::1]/o/r", "::1"),
            ("ssh://git@localhost/o/r", "localhost"),
            ("git@app.localhost:o/r", "app.localhost"),
            ("https://0.0.0.0/o/r", "0.0.0.0"),
            ("https://[::]/o/r", "::"),
            // Numeric forms some resolvers read as addresses.
            ("https://2130706433/o/r", "2130706433"),
            ("https://0x7f.1/o/r", "0x7f.1"),
            // Link-local, including the cloud metadata address.
            ("https://169.254.169.254/latest", "169.254.169.254"),
            ("https://[fe80::1]/o/r", "fe80::1"),
            // RFC 1918.
            ("https://10.0.0.1/o/r", "10.0.0.1"),
            ("https://172.16.5.4/o/r", "172.16.5.4"),
            ("git@192.168.1.10:o/r", "192.168.1.10"),
            // CGNAT (the tailnet range).
            ("https://100.64.0.1/o/r", "100.64.0.1"),
            ("https://100.127.255.1/o/r", "100.127.255.1"),
            // Unique-local IPv6, and v4-mapped loopback.
            ("https://[fd00::1]/o/r", "fd00::1"),
            ("https://[::ffff:127.0.0.1]/o/r", "::ffff:127.0.0.1"),
            ("https://[::ffff:10.1.1.1]/o/r", "::ffff:10.1.1.1"),
            // Cluster service names.
            (
                "https://kubernetes.default.svc/o/r",
                "kubernetes.default.svc",
            ),
            (
                "https://git.ns.svc.cluster.local/o/r",
                "git.ns.svc.cluster.local",
            ),
            ("ssh://git@cluster.local/o/r", "cluster.local"),
            // The server's own name.
            ("https://graft.example.com/o/r", "graft.example.com"),
            // Not allowlisted at all.
            ("https://gitlab.com/o/r", ""),
            ("https://github.com.evil.example/o/r", ""),
        ];
        for (url, host) in cases {
            let mut p = policy();
            if !host.is_empty() {
                p.allowed_hosts.push((*host).to_owned());
            }
            rejected(check_upstream(url, &p), "mirror-url");
        }
        // A public address passes the class check and is held to the
        // allowlist like any host.
        let mut p = policy();
        p.allowed_hosts.push("140.82.112.3".into());
        assert_eq!(check_upstream("https://140.82.112.3/o/r", &p), Ok(()));
        rejected(check_upstream("https://140.82.112.4/o/r", &p), "mirror-url");
    }

    #[test]
    fn a_refusal_never_echoes_the_url() {
        let Err(ApiError::Rejected { detail, .. }) =
            check_upstream("https://secret-token@github.com/o/r", &policy())
        else {
            panic!("refused");
        };
        assert!(!detail.contains("secret-token"), "{detail}");
    }

    #[tokio::test]
    async fn the_interval_floor_is_enforced() {
        let s = store().await;
        let bind = who(&["mirror:bind"]);
        let mut r = req(UP, None, vec![line("main", "follow")]);
        r.interval_s = 14;
        rejected(
            set(&s, &bind, &policy(), 0, r.clone()).await,
            "mirror-interval",
        );
        let mut p = policy();
        p.min_interval_s = 60;
        r.interval_s = 0;
        assert_eq!(
            set(&s, &bind, &p, 0, r).await.expect("default").interval_s,
            60
        );
        let mut r = req(UP, Some(1), vec![line("main", "follow")]);
        r.interval_s = 15;
        assert_eq!(
            set(&s, &bind, &policy(), 0, r)
                .await
                .expect("floor")
                .interval_s,
            15
        );
    }

    #[tokio::test]
    async fn settings_writes_are_compare_and_swap_on_version() {
        let s = store().await;
        let bind = who(&["mirror:bind"]);
        let created = set(
            &s,
            &bind,
            &policy(),
            1,
            req(UP, None, vec![line("main", "follow")]),
        )
        .await
        .expect("create");
        assert_eq!(created.version, 1);
        // Creating again (no expected version) and a stale etag both lose.
        rejected(
            set(
                &s,
                &bind,
                &policy(),
                2,
                req(UP, None, vec![line("main", "follow")]),
            )
            .await,
            "mirror-version",
        );
        rejected(
            set(
                &s,
                &bind,
                &policy(),
                2,
                req(UP, Some(5), vec![line("main", "follow")]),
            )
            .await,
            "mirror-version",
        );
        let mut r = req(UP, Some(1), vec![line("main", "follow")]);
        r.enabled = false;
        let updated = set(&s, &bind, &policy(), 3, r.clone())
            .await
            .expect("update");
        assert_eq!((updated.version, updated.enabled), (2, false));
        // The same etag again is now stale.
        rejected(set(&s, &bind, &policy(), 4, r).await, "mirror-version");
    }

    #[tokio::test]
    async fn the_grant_matrix_holds() {
        let s = store().await;
        let admin = who(&["repo:r:admin"]);
        let reader = who(&["repo:r:read"]);
        let bind = who(&["mirror:bind"]);
        let operator = who(&["mirror:bind", "mirror:export", "repo:r:admin"]);
        let mirror = mirror_token("m");
        let follow = vec![line("main", "follow")];

        // Creating a row (binding an upstream) needs mirror:bind: a repo
        // admin, reader or mirror token cannot.
        for p in [&admin, &reader, &mirror] {
            denied(
                set(&s, p, &policy(), 0, req(UP, None, follow.clone())).await,
                "mirror:bind",
            );
        }
        // Export on create needs mirror:export too.
        denied(
            set(
                &s,
                &bind,
                &policy(),
                0,
                req(UP, None, vec![line("main", "both")]),
            )
            .await,
            "mirror:export",
        );
        set(&s, &bind, &policy(), 0, req(UP, None, follow.clone()))
            .await
            .expect("bind creates");

        // A repo admin may enable/disable, change the interval and edit
        // follow lines.
        let mut r = req(
            UP,
            Some(1),
            vec![line("main", "follow"), line("dev", "follow")],
        );
        r.interval_s = 120;
        r.enabled = false;
        let v = set(&s, &admin, &policy(), 0, r)
            .await
            .expect("admin edits")
            .version;
        // ... but not turn on export, or change the author map.
        denied(
            set(
                &s,
                &admin,
                &policy(),
                0,
                req(UP, Some(v), vec![line("main", "export")]),
            )
            .await,
            "mirror:export",
        );
        let mut r = req(UP, Some(v), follow.clone());
        r.export = Some(pb::MirrorExport {
            default_author: Some("Bot <b@example.com>".into()),
            authors: vec![],
        });
        denied(
            set(&s, &admin, &policy(), 0, r.clone()).await,
            "mirror:export",
        );
        // A reader edits nothing.
        denied(
            set(&s, &reader, &policy(), 0, req(UP, Some(v), follow.clone())).await,
            "repo:r:admin",
        );
        // The operator may do all of it.
        let v = set(&s, &operator, &policy(), 0, r)
            .await
            .expect("export map")
            .version;
        let v = set(&s, &operator, &policy(), 0, {
            let mut r = req(UP, Some(v), vec![line("main", "both")]);
            r.export = Some(pb::MirrorExport {
                default_author: Some("Bot <b@example.com>".into()),
                authors: vec![],
            });
            r
        })
        .await
        .expect("export line")
        .version;
        // Keeping an export line that is already on needs no export grant.
        let mut keep = req(UP, Some(v), vec![line("main", "both")]);
        keep.interval_s = 30;
        keep.export = Some(pb::MirrorExport {
            default_author: Some("Bot <b@example.com>".into()),
            authors: vec![],
        });
        set(&s, &admin, &policy(), 0, keep)
            .await
            .expect("admin keeps export");

        // Status: only the mirror permission set; admin does not grant it.
        let status = pb::SetMirrorStatusRequest {
            repo: "r".into(),
            upstream: UP.into(),
            status: Some(pb::MirrorStatus {
                lease_epoch: 1,
                ..pb::MirrorStatus::default()
            }),
        };
        denied(
            set_status(&s, &admin, 0, status.clone()).await,
            "repo:r:mirror",
        );
        denied(
            set_status(&s, &operator, 0, status.clone()).await,
            "repo:r:mirror",
        );
        // A lease is not the operator's to take; the mirror's lease fences
        // its status write.
        denied(take(&s, &operator, 0).await, "repo:r:mirror");
        assert_eq!(take(&s, &mirror, 0).await, Ok(1));
        set_status(&s, &mirror, 0, status)
            .await
            .expect("mirror writes status");

        // Remove and purge need mirror:bind.
        let key = pb::RemoveMirrorRequest {
            repo: "r".into(),
            upstream: UP.into(),
            expected_version: None,
            clear_followed: false,
        };
        for p in [&admin, &mirror] {
            denied(remove(&s, p, key.clone()).await, "mirror:bind");
            denied(
                purge(
                    &s,
                    p,
                    0,
                    pb::PurgeMirrorRequest {
                        repo: "r".into(),
                        upstream: UP.into(),
                        client_request_id: "k".into(),
                    },
                )
                .await,
                "mirror:bind",
            );
        }
        // Reads: the repository's readers and the mirror token.
        for p in [&reader, &admin, &mirror] {
            get(
                &s,
                p,
                pb::MirrorKey {
                    repo: "r".into(),
                    upstream: UP.into(),
                },
            )
            .await
            .expect("read");
        }
        denied(
            get(
                &s,
                &who(&["repo:x:read"]),
                pb::MirrorKey {
                    repo: "r".into(),
                    upstream: UP.into(),
                },
            )
            .await,
            "repo:r:read",
        );
        remove(&s, &bind, key).await.expect("bind removes");
    }

    #[tokio::test]
    async fn purge_needs_a_disabled_or_removed_row() {
        let s = store().await;
        let bind = who(&["mirror:bind"]);
        set(
            &s,
            &bind,
            &policy(),
            0,
            req(UP, None, vec![line("main", "follow")]),
        )
        .await
        .expect("create");
        let key = pb::PurgeMirrorRequest {
            repo: "r".into(),
            upstream: UP.into(),
            client_request_id: "purge-1".into(),
        };
        rejected(purge(&s, &bind, 0, key.clone()).await, "mirror-enabled");
        let mut off = req(UP, Some(1), vec![line("main", "follow")]);
        off.enabled = false;
        set(&s, &bind, &policy(), 0, off).await.expect("disable");
        // A disabled row's purge is recorded once; a retry returns it.
        let first = purge(&s, &bind, 0, key.clone()).await.expect("purge");
        assert_eq!(first.kind, pb::MirrorRequestKind::Purge as i32);
        assert_eq!(first.state, pb::MirrorRequestState::Pending as i32);
        assert_eq!(first.upstream.as_deref(), Some(UP));
        let again = purge(&s, &bind, 5, key).await.expect("retry");
        assert_eq!(again.id, first.id);
    }

    async fn marker(s: &MemoryStore, line: &str) {
        let id = s.repo("r").await.expect("repo").expect("exists");
        let name = followed_ref(line);
        let txn = MetaTxn {
            preconditions: vec![Precondition::RefIs {
                name: name.clone(),
                expected: None,
            }],
            writes: vec![Write::SetRef(RefRecord {
                name,
                target: ChangesetId(ObjectId([1; 32])),
                version: 1,
                kind: RefKind::Tool,
            })],
        };
        assert!(matches!(
            s.commit(id, txn).await,
            Ok(TxnOutcome::Committed(_))
        ));
    }

    #[tokio::test]
    async fn removal_keeps_markers_unless_asked_to_clear_them() {
        let s = store().await;
        let id = s.repo("r").await.expect("repo").expect("exists");
        let bind = who(&["mirror:bind"]);
        let lines = vec![line("main", "follow"), line("dev", "follow")];
        marker(&s, "main").await;
        marker(&s, "dev").await;

        set(&s, &bind, &policy(), 0, req(UP, None, lines.clone()))
            .await
            .expect("create");
        let tip = s.journal_tip(id).await.expect("tip");
        let kept = remove(
            &s,
            &bind,
            pb::RemoveMirrorRequest {
                repo: "r".into(),
                upstream: UP.into(),
                expected_version: Some(1),
                clear_followed: false,
            },
        )
        .await
        .expect("remove");
        assert!(kept.cleared.is_empty());
        assert_eq!(s.get_mirror(id, UP).await.expect("get"), None);
        assert!(
            s.get_ref(id, "graft.followed/main")
                .await
                .expect("ref")
                .is_some()
        );
        assert_eq!(s.journal_tip(id).await.expect("tip"), tip);

        set(&s, &bind, &policy(), 0, req(UP, None, lines))
            .await
            .expect("re-add");
        rejected(
            remove(
                &s,
                &bind,
                pb::RemoveMirrorRequest {
                    repo: "r".into(),
                    upstream: UP.into(),
                    expected_version: Some(9),
                    clear_followed: true,
                },
            )
            .await,
            "mirror-version",
        );
        let cleared = remove(
            &s,
            &bind,
            pb::RemoveMirrorRequest {
                repo: "r".into(),
                upstream: UP.into(),
                expected_version: None,
                clear_followed: true,
            },
        )
        .await
        .expect("remove and clear");
        assert_eq!(
            cleared.cleared,
            vec!["graft.followed/main", "graft.followed/dev"]
        );
        assert_eq!(
            s.get_ref(id, "graft.followed/main").await.expect("ref"),
            None
        );
        assert_eq!(
            s.get_ref(id, "graft.followed/dev").await.expect("ref"),
            None
        );
        // One journal entry per cleared marker.
        let entries = s.journal(id, tip.unwrap_or(0), 10).await.expect("journal");
        let deleted: Vec<String> = entries
            .iter()
            .filter(|(_, kind, _)| kind == REF_DELETED)
            .map(|(_, _, body)| {
                let v: serde_json::Value = serde_json::from_slice(body).expect("json");
                v["name"].as_str().expect("name").to_owned()
            })
            .collect();
        assert_eq!(deleted, vec!["graft.followed/main", "graft.followed/dev"]);
    }

    #[tokio::test]
    async fn pages_are_bounded_and_cross_repo_listing_needs_a_global_grant() {
        let s = store().await;
        s.create_repo("q").await.expect("repo");
        let bind = who(&["mirror:bind"]);
        for i in 0..(MAX_PAGE + 5) {
            set(
                &s,
                &bind,
                &policy(),
                0,
                req(
                    &format!("https://github.com/o/r{i:04}"),
                    None,
                    vec![line("main", "follow")],
                ),
            )
            .await
            .expect("create");
        }
        let mut other = req(UP, None, vec![line("main", "follow")]);
        other.repo = "q".into();
        set(&s, &bind, &policy(), 0, other)
            .await
            .expect("create in q");

        let reader = who(&["repo:r:read"]);
        let page = list(
            &s,
            &reader,
            pb::ListMirrorsRequest {
                repo: "r".into(),
                enabled: None,
                limit: 0,
                page_token: String::new(),
            },
        )
        .await
        .expect("list");
        assert_eq!(page.mirrors.len(), MAX_PAGE as usize);
        let rest = list(
            &s,
            &reader,
            pb::ListMirrorsRequest {
                repo: "r".into(),
                enabled: None,
                limit: 1000,
                page_token: page.next_page_token,
            },
        )
        .await
        .expect("rest");
        assert_eq!(rest.mirrors.len(), 5);
        assert!(rest.next_page_token.is_empty());

        // Every repository: only with repo:*:read, repo:*:mirror or
        // repos:admin.
        let all = pb::ListMirrorsRequest {
            repo: String::new(),
            enabled: Some(true),
            limit: 3,
            page_token: String::new(),
        };
        denied(list(&s, &reader, all.clone()).await, "repo:*:read");
        for scopes in [["repo:*:read"], ["repo:*:mirror"], ["repos:admin"]] {
            let page = list(&s, &who(&scopes), all.clone()).await.expect("all");
            assert_eq!(page.mirrors.len(), 3);
            assert!(!page.next_page_token.is_empty());
        }
    }

    #[tokio::test]
    async fn reads_carry_whether_a_credential_is_set_never_its_bytes() {
        let s = store().await;
        let id = s.repo("r").await.expect("repo").expect("exists");
        let bind = who(&["mirror:bind"]);
        set(
            &s,
            &bind,
            &policy(),
            0,
            req(UP, None, vec![line("main", "follow")]),
        )
        .await
        .expect("create");
        // As SetMirrorCredential (WI-3209) will leave it.
        let mut row = s.get_mirror(id, UP).await.expect("get").expect("row");
        row.credential = Some(b"sealed-secret".to_vec());
        row.kek_id = Some("kek-1".into());
        row.version = 2;
        let txn = MetaTxn {
            preconditions: vec![Precondition::MirrorVersionIs {
                upstream: UP.into(),
                version: Some(1),
            }],
            writes: vec![Write::PutMirror(row)],
        };
        s.commit(id, txn).await.expect("seal");

        let got = get(
            &s,
            &bind,
            pb::MirrorKey {
                repo: "r".into(),
                upstream: UP.into(),
            },
        )
        .await;
        let got = got.expect_err("bind alone does not read");
        assert!(matches!(got, ApiError::PermissionDenied { .. }));
        let got = get(
            &s,
            &who(&["repo:r:read"]),
            pb::MirrorKey {
                repo: "r".into(),
                upstream: UP.into(),
            },
        )
        .await
        .expect("get");
        assert!(got.credential_set);
        assert_eq!(got.kek_id.as_deref(), Some("kek-1"));
        assert!(!format!("{got:?}").contains("sealed-secret"));
        // An admin edit keeps the credential.
        let v = set(
            &s,
            &who(&["repo:r:admin"]),
            &policy(),
            0,
            req(UP, Some(2), vec![line("main", "follow")]),
        )
        .await
        .expect("edit");
        assert!(v.credential_set);
    }

    #[tokio::test]
    async fn status_is_bounded_typed_and_never_creates_a_row() {
        let s = store().await;
        let mirror = mirror_token("m");
        let status = |message: String| pb::SetMirrorStatusRequest {
            repo: "r".into(),
            upstream: UP.into(),
            status: Some(pb::MirrorStatus {
                error_code: pb::MirrorErrorCode::Timeout as i32,
                error_message: Some(message),
                lease_epoch: 1,
                ..pb::MirrorStatus::default()
            }),
        };
        assert_eq!(take(&s, &mirror, 0).await, Ok(1));
        assert!(matches!(
            set_status(&s, &mirror, 0, status("x".into())).await,
            Err(ApiError::NotFound { .. })
        ));
        set(
            &s,
            &who(&["mirror:bind"]),
            &policy(),
            0,
            req(UP, None, vec![line("main", "follow")]),
        )
        .await
        .expect("create");
        rejected(
            set_status(&s, &mirror, 0, status("x".repeat(201))).await,
            "mirror-field",
        );
        set_status(&s, &mirror, 0, status("é".repeat(200)))
            .await
            .expect("200 characters");
        let id = s.repo("r").await.expect("repo").expect("exists");
        let row = s.get_mirror(id, UP).await.expect("get").expect("row");
        assert_eq!(row.version, 1, "status is not part of the version");
        assert_eq!(
            row.status.expect("status").error_code.as_deref(),
            Some("timeout")
        );
        let got = get(
            &s,
            &mirror,
            pb::MirrorKey {
                repo: "r".into(),
                upstream: UP.into(),
            },
        )
        .await
        .expect("get");
        assert_eq!(
            got.status.expect("status").error_code,
            pb::MirrorErrorCode::Timeout as i32
        );
    }

    /// §3.6: acquire bumps the epoch, renew extends by the TTL, a lapsed
    /// lease is taken again under a higher epoch, and release keeps the
    /// epoch. The clock is the `now_ms` argument: no sleeps.
    #[tokio::test]
    async fn the_lease_is_acquired_renewed_and_expires() {
        let s = store().await;
        let (a, b) = (mirror_token("a"), mirror_token("b"));

        assert_eq!(take(&s, &a, 0).await, Ok(1));
        lost(take(&s, &b, LEASE_TTL_MS - 1).await);
        let renewed = renew_lease(&s, &a, 50_000, held(1)).await.expect("renew");
        assert_eq!(
            (renewed.epoch, renewed.expires_at_ms),
            (1, 50_000 + LEASE_TTL_MS)
        );
        // Renewed, so still held past the first expiry.
        lost(take(&s, &b, LEASE_TTL_MS).await);
        // Lapsed: renewal is refused and a new holder gets a higher epoch.
        lost(renew_lease(&s, &a, 50_000 + LEASE_TTL_MS, held(1)).await);
        assert_eq!(take(&s, &b, 50_000 + LEASE_TTL_MS).await, Ok(2));
        let id = s.repo("r").await.expect("repo").expect("exists");
        let row = s.get_lease(id).await.expect("lease").expect("row");
        assert_eq!(row.holder.as_deref(), Some("b/i1"));

        // A release frees it at once; the epoch moves on, never back.
        lost(release_lease(&s, &a, 120_000, held(1)).await);
        release_lease(&s, &b, 120_000, held(2))
            .await
            .expect("release");
        lost(renew_lease(&s, &b, 120_000, held(2)).await);
        assert_eq!(take(&s, &a, 120_000).await, Ok(3));

        // Only a mirror-kind token with the mirror set may hold a lease, and
        // the instance id is bounded.
        let human = who(&["repo:*:mirror"]);
        denied(take(&s, &human, 0).await, "repo:r:mirror");
        rejected(
            acquire_lease(
                &s,
                &b,
                0,
                pb::AcquireLeaseRequest {
                    repo: "r".into(),
                    instance: " ".into(),
                },
            )
            .await,
            "mirror-field",
        );
    }

    /// §3.6: once a lease is reacquired, every fenced call made under the
    /// old epoch is `LeaseLost` and writes nothing.
    #[tokio::test]
    async fn a_stale_epoch_writer_is_fenced_off() {
        let s = store().await;
        set(
            &s,
            &who(&["mirror:bind"]),
            &policy(),
            0,
            req(UP, None, vec![line("main", "follow")]),
        )
        .await
        .expect("create");
        let (old, new) = (mirror_token("old"), mirror_token("new"));
        let status = |epoch: u64, sha: &str| pb::SetMirrorStatusRequest {
            repo: "r".into(),
            upstream: UP.into(),
            status: Some(pb::MirrorStatus {
                last_imported_sha: Some(sha.into()),
                lease_epoch: epoch,
                ..pb::MirrorStatus::default()
            }),
        };
        let id = s.repo("r").await.expect("repo").expect("exists");
        let sha = |s: MemoryStore| async move {
            s.get_mirror(id, UP)
                .await
                .expect("get")
                .and_then(|m| m.status)
                .and_then(|st| st.last_imported_sha)
        };

        // No lease at all: even epoch 0 is fenced.
        assert!(matches!(
            set_status(&s, &old, 0, status(0, "zero")).await,
            Err(ApiError::LeaseLost { .. })
        ));
        assert_eq!(take(&s, &old, 0).await, Ok(1));
        set_status(&s, &old, 1_000, status(1, "first"))
            .await
            .expect("holder writes");
        // Past the TTL the old holder is fenced even before anyone else
        // takes over.
        assert!(matches!(
            set_status(&s, &old, LEASE_TTL_MS, status(1, "late")).await,
            Err(ApiError::LeaseLost { .. })
        ));
        assert_eq!(take(&s, &new, LEASE_TTL_MS).await, Ok(2));
        let now = LEASE_TTL_MS + 1;
        assert!(matches!(
            set_status(&s, &old, now, status(1, "stale")).await,
            Err(ApiError::LeaseLost { .. })
        ));
        assert!(matches!(
            renew_lease(&s, &old, now, held(1)).await,
            Err(ApiError::LeaseLost { .. })
        ));
        assert!(matches!(
            release_lease(&s, &old, now, held(1)).await,
            Err(ApiError::LeaseLost { .. })
        ));
        assert_eq!(sha(s.clone()).await.as_deref(), Some("first"));
        // The old holder's release did not free the new holder's lease.
        set_status(&s, &new, now, status(2, "second"))
            .await
            .expect("new holder writes");
        assert_eq!(sha(s.clone()).await.as_deref(), Some("second"));
    }

    /// §3.6: contenders racing `AcquireLease` on one SQLite store, across
    /// threads. Whatever the interleaving, exactly one wins, at epoch 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_acquirers_have_exactly_one_winner() {
        use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("dir");
        let s = Arc::new(
            SqliteMetaStore::open(SqliteConfig::new(dir.path().join("meta.db"))).expect("open"),
        );
        s.create_repo("r").await.expect("repo");
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                tokio::spawn(async move { take(&*s, &mirror_token(&format!("m{i}")), 0).await })
            })
            .collect();
        let mut won = Vec::new();
        for t in tasks {
            match t.await.expect("task") {
                Ok(epoch) => won.push(epoch),
                Err(ApiError::LeaseLost { .. }) => {}
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(won, vec![1]);
        let id = s.repo("r").await.expect("repo").expect("exists");
        assert_eq!(
            s.get_lease(id).await.expect("lease").map(|l| l.epoch),
            Some(1)
        );
    }

    const PAT: &[u8] = b"ghp_interim_row_pat";

    fn ring(active: Option<(&str, u8)>, retired: &[(&str, u8)]) -> KeyRing {
        use crate::credentials::Kek;
        let k = |(id, b): (&str, u8)| Kek::new(id, [b; 32]).expect("kek");
        KeyRing::new(active.map(k), retired.iter().copied().map(k).collect()).expect("ring")
    }

    fn cred(upstream: &str, value: &[u8], version: u64) -> pb::SetMirrorCredentialRequest {
        pb::SetMirrorCredentialRequest {
            repo: "r".into(),
            upstream: upstream.into(),
            credential: value.to_vec(),
            expected_version: version,
        }
    }

    fn fetch(upstream: &str, epoch: u64) -> pb::GetMirrorCredentialRequest {
        pb::GetMirrorCredentialRequest {
            repo: "r".into(),
            upstream: upstream.into(),
            epoch,
        }
    }

    /// A store with one row at version 1 on `UP`.
    async fn with_row() -> MemoryStore {
        let s = store().await;
        set(
            &s,
            &who(&["mirror:bind"]),
            &policy(),
            1,
            req(UP, None, vec![line("main", "follow")]),
        )
        .await
        .expect("row");
        s
    }

    #[tokio::test]
    async fn setting_a_credential_is_write_only_bound_and_bumps_the_version() {
        let s = with_row().await;
        let keys = ring(Some(("k1", 1)), &[]);
        for p in [
            who(&["repo:r:admin"]),
            who(&["repos:admin"]),
            mirror_token("m"),
        ] {
            denied(
                set_credential(&s, &p, &keys, 2, cred(UP, PAT, 1)).await,
                "mirror:bind",
            );
        }
        let bind = who(&["mirror:bind"]);
        rejected(
            set_credential(&s, &bind, &keys, 2, cred(UP, PAT, 7)).await,
            "mirror-version",
        );
        rejected(
            set_credential(
                &s,
                &bind,
                &keys,
                2,
                cred(UP, &[b'x'; MAX_CREDENTIAL + 1], 1),
            )
            .await,
            "mirror-field",
        );
        rejected(
            set_credential(&s, &bind, &ring(None, &[("k1", 1)]), 2, cred(UP, PAT, 1)).await,
            "mirror-credential",
        );
        let m = set_credential(&s, &bind, &keys, 2, cred(UP, PAT, 1))
            .await
            .expect("set");
        assert_eq!((m.version, m.credential_set), (2, true));
        assert_eq!(m.kek_id.as_deref(), Some("k1"));
        // What is stored is the envelope, never the plaintext.
        let id = s.repo("r").await.expect("repo").expect("r");
        let row = s.get_mirror(id, UP).await.expect("get").expect("row");
        let stored = row.credential.expect("sealed");
        assert!(!stored.windows(PAT.len()).any(|w| w == PAT));
        // Every read answer says only that one is set.
        let read = get(
            &s,
            &who(&["repo:r:read"]),
            pb::MirrorKey {
                repo: "r".into(),
                upstream: UP.into(),
            },
        )
        .await
        .expect("read");
        assert!(read.credential_set);
        assert!(!format!("{read:?}").contains("ghp_"));
        // An empty credential clears it, and bumps the version again.
        let cleared = set_credential(&s, &bind, &keys, 3, cred(UP, b"", 2))
            .await
            .expect("clear");
        assert_eq!((cleared.version, cleared.credential_set), (3, false));
        assert_eq!(cleared.kek_id, None);
        let absent = set_credential(
            &s,
            &bind,
            &keys,
            3,
            cred("https://github.com/o/x.git", PAT, 1),
        )
        .await;
        assert!(
            matches!(absent, Err(ApiError::NotFound { .. })),
            "{absent:?}"
        );
    }

    #[tokio::test]
    async fn only_the_lease_holding_mirror_token_reads_the_plaintext() {
        let s = with_row().await;
        let keys = ring(Some(("k1", 1)), &[]);
        set_credential(&s, &who(&["mirror:bind"]), &keys, 2, cred(UP, PAT, 1))
            .await
            .expect("set");
        let m = mirror_token("m");
        let epoch = take(&s, &m, 1_000).await.expect("lease");
        let got = get_credential(&s, &m, &keys, 2_000, fetch(UP, epoch))
            .await
            .expect("holder reads");
        assert_eq!(got.credential, PAT);
        assert_eq!(got.kek_id, "k1");
        // The response's Debug carries no plaintext.
        for shown in [format!("{got:?}"), format!("{got:#?}")] {
            assert!(
                shown.contains("<redacted>") && !shown.contains("ghp_"),
                "{shown}"
            );
        }

        // Not the mirror kind, whatever the scopes: refused by permission.
        let human_with_set = Principal {
            kind: PrincipalKind::Human,
            ..mirror_token("m")
        };
        let service = Principal {
            kind: PrincipalKind::Service,
            ..mirror_token("m")
        };
        for p in [
            human_with_set,
            service,
            who(&[
                "mirror:bind",
                "mirror:export",
                "repos:admin",
                "repo:r:admin",
            ]),
        ] {
            denied(
                get_credential(&s, &p, &keys, 2_000, fetch(UP, epoch)).await,
                "repo:r:mirror",
            );
        }
        // A mirror token not holding the current epoch: fenced.
        lost(get_credential(&s, &m, &keys, 2_000, fetch(UP, epoch - 1)).await);
        lost(get_credential(&s, &m, &keys, 2_000, fetch(UP, epoch + 1)).await);
        lost(get_credential(&s, &mirror_token("other"), &keys, 2_000, fetch(UP, epoch)).await);
        // The lease lapsed: fenced too.
        lost(get_credential(&s, &m, &keys, 1_000 + LEASE_TTL_MS, fetch(UP, epoch)).await);
        // A newer holder's epoch fences the old one out.
        let next = take(&s, &m, 1_000 + LEASE_TTL_MS).await.expect("re-take");
        lost(get_credential(&s, &m, &keys, 2_000 + LEASE_TTL_MS, fetch(UP, epoch)).await);
        assert!(
            get_credential(&s, &m, &keys, 2_000 + LEASE_TTL_MS, fetch(UP, next))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_ciphertext_copied_into_another_row_does_not_open() {
        let s = with_row().await;
        let other = "https://github.com/o/other.git";
        let bind = who(&["mirror:bind"]);
        set(
            &s,
            &bind,
            &policy(),
            1,
            req(other, None, vec![line("main", "follow")]),
        )
        .await
        .expect("second row");
        let keys = ring(Some(("k1", 1)), &[]);
        set_credential(&s, &bind, &keys, 2, cred(UP, PAT, 1))
            .await
            .expect("set");
        // A database writer copies the sealed bytes and key id across.
        let id = s.repo("r").await.expect("repo").expect("r");
        let source = s.get_mirror(id, UP).await.expect("get").expect("row");
        let target = s.get_mirror(id, other).await.expect("get").expect("row");
        let moved = MirrorRecord {
            credential: source.credential.clone(),
            kek_id: source.kek_id.clone(),
            version: target.version + 1,
            ..target.clone()
        };
        let txn = MetaTxn {
            preconditions: vec![Precondition::MirrorVersionIs {
                upstream: other.into(),
                version: Some(target.version),
            }],
            writes: vec![Write::PutMirror(moved)],
        };
        assert!(matches!(
            s.commit(id, txn).await.expect("commit"),
            TxnOutcome::Committed(_)
        ));
        let m = mirror_token("m");
        let epoch = take(&s, &m, 1_000).await.expect("lease");
        let r = get_credential(&s, &m, &keys, 2_000, fetch(other, epoch)).await;
        rejected(r, "mirror-credential");
        assert_eq!(
            get_credential(&s, &m, &keys, 2_000, fetch(UP, epoch))
                .await
                .expect("source still opens")
                .credential,
            PAT
        );
    }

    #[tokio::test]
    async fn rewrap_moves_every_row_to_the_active_key_so_the_old_one_can_retire() {
        let s = with_row().await;
        let bind = who(&["mirror:bind"]);
        let bare = "https://github.com/o/bare.git";
        set(
            &s,
            &bind,
            &policy(),
            1,
            req(bare, None, vec![line("main", "follow")]),
        )
        .await
        .expect("row without a credential");
        let v1 = ring(Some(("k1", 1)), &[]);
        set_credential(&s, &bind, &v1, 2, cred(UP, PAT, 1))
            .await
            .expect("set");
        // Rewrap needs an active key.
        assert!(rewrap_all(&s, &ring(None, &[("k1", 1)]), 3).await.is_err());
        let v2 = ring(Some(("k2", 2)), &[("k1", 1)]);
        let report = rewrap_all(&s, &v2, 3).await.expect("rewrap");
        assert_eq!((report.rewrapped, report.unchanged), (1, 1));
        assert!(report.failed.is_empty());
        let id = s.repo("r").await.expect("repo").expect("r");
        let row = s.get_mirror(id, UP).await.expect("get").expect("row");
        assert_eq!((row.kek_id.as_deref(), row.version), (Some("k2"), 3));
        // Again: nothing left to do.
        let again = rewrap_all(&s, &v2, 4).await.expect("rewrap");
        assert_eq!((again.rewrapped, again.unchanged), (0, 2));
        // Retire k1 entirely: the row still opens under k2 alone.
        let v3 = ring(Some(("k2", 2)), &[]);
        let m = mirror_token("m");
        let epoch = take(&s, &m, 1_000).await.expect("lease");
        assert_eq!(
            get_credential(&s, &m, &v3, 2_000, fetch(UP, epoch))
                .await
                .expect("opens")
                .credential,
            PAT
        );
        // A row whose key is gone is reported, not dropped.
        let v4 = ring(Some(("k3", 3)), &[]);
        let lost_key = rewrap_all(&s, &v4, 5).await.expect("rewrap");
        assert_eq!(lost_key.failed, vec![("r".to_owned(), UP.to_owned())]);
        let row = s.get_mirror(id, UP).await.expect("get").expect("row");
        assert_eq!(row.kek_id.as_deref(), Some("k2"));
    }
}

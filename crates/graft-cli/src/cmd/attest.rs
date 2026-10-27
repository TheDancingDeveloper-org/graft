//! `graft attest` and `graft attestations` (`docs/design.md` §8.3).

use graft_proto::api::RepoApi;
use graft_proto::types::{
    AttestKind, AttestStatus, AttestV2, AttestationSelector, GetAttestations, Page,
};
use serde_json::json;

use crate::cli::{
    AttestArgs, AttestKindArg, AttestStatusArg, AttestationsArgs, Global, PAGE_DEFAULT,
};
use crate::conn::{call_context, connect, runtime, working_copy_info};
use crate::error::Failure;
use crate::render::{print_json, short};

/// Posts one attestation.
pub(crate) fn run(g: &Global, args: &AttestArgs) -> Result<(), Failure> {
    let (kind, status, name) = attestation(args)?;
    let legacy = args.check.is_some();
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let changeset = conn.resolve(&cx, &args.rev).await?;
        conn.remote
            .attest_v2(
                &cx,
                AttestV2 {
                    repo: conn.repo.clone(),
                    changeset,
                    kind,
                    check: name.clone(),
                    status,
                    by: None,
                    url: args.url.clone(),
                },
            )
            .await?;
        if g.json {
            if legacy {
                return print_json(&json!({
                    "changeset": changeset.to_string(),
                    "check": name,
                    "passed": status == AttestStatus::Pass,
                    "url": args.url,
                }));
            }
            return print_json(&json!({
                "changeset": changeset.to_string(),
                "kind": kind_name(kind),
                "status": status_name(status),
                "name": name,
                "url": args.url,
            }));
        }
        if legacy {
            outln!(
                "attested {} {} on {}",
                name,
                if status == AttestStatus::Pass {
                    "passed"
                } else {
                    "failed"
                },
                short(changeset)
            );
            return Ok(());
        }
        outln!(
            "attested {} {} {} on {}",
            kind_name(kind),
            name,
            status_name(status),
            short(changeset)
        );
        Ok(())
    })
}

pub(crate) fn list(g: &Global, args: &AttestationsArgs) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let selector = match (&args.rev, &args.line, args.from, args.to) {
            (Some(rev), None, None, None) => {
                AttestationSelector::ByChangeset(conn.resolve(&cx, rev).await?)
            }
            (None, Some(line), Some(from_pos), Some(to_pos)) if from_pos <= to_pos => {
                AttestationSelector::ByLineRange {
                    line: line.clone(),
                    from_pos,
                    to_pos,
                }
            }
            (None, Some(_), Some(_), Some(_)) => {
                return Err(Failure::usage("--from must not exceed --to"));
            }
            _ => {
                return Err(Failure::usage(
                    "pass a changeset, or --line LINE --from POS --to POS",
                ));
            }
        };
        let page = conn
            .remote
            .get_attestations(
                &cx,
                GetAttestations {
                    repo: conn.repo.clone(),
                    selector,
                    kinds: Vec::new(),
                    page: Page {
                        limit: PAGE_DEFAULT,
                        token: None,
                    },
                },
            )
            .await?;
        if g.json {
            return print_json(&json!({
                "attestations": page.attestations.iter().map(|a| json!({
                    "changeset": a.changeset.to_string(),
                    "kind": kind_name(a.kind),
                    "status": status_name(a.status),
                    "name": a.check,
                    "by": a.by,
                    "url": a.url,
                })).collect::<Vec<_>>(),
                "next": page.next,
            }));
        }
        for a in page.attestations {
            outln!(
                "{} {} {} {}{}",
                short(a.changeset),
                kind_name(a.kind),
                status_name(a.status),
                a.check,
                a.by.as_deref()
                    .map(|by| format!(" by {by}"))
                    .unwrap_or_default()
            );
        }
        if let Some(next) = page.next {
            outln!("next {next}");
        }
        Ok(())
    })
}

fn attestation(args: &AttestArgs) -> Result<(AttestKind, AttestStatus, String), Failure> {
    match (&args.kind, &args.status, &args.name, &args.check) {
        (Some(kind), Some(status), Some(name), None) if !args.pass && !args.fail => Ok((
            match kind {
                AttestKindArg::Check => AttestKind::Check,
                AttestKindArg::Review => AttestKind::Review,
                AttestKindArg::Approval => AttestKind::Approval,
            },
            match status {
                AttestStatusArg::Pass => AttestStatus::Pass,
                AttestStatusArg::Fail => AttestStatus::Fail,
                AttestStatusArg::Pending => AttestStatus::Pending,
                AttestStatusArg::Error => AttestStatus::Error,
            },
            name.clone(),
        )),
        (None, None, None, Some(check)) if args.pass ^ args.fail => Ok((
            AttestKind::Check,
            AttestStatus::from_passed(args.pass),
            check.clone(),
        )),
        _ => Err(Failure::usage(
            "use --kind KIND --status STATUS --name NAME, or legacy --check NAME with exactly one of --pass or --fail",
        )),
    }
}

pub(crate) fn kind_name(kind: AttestKind) -> &'static str {
    match kind {
        AttestKind::Check => "check",
        AttestKind::Review => "review",
        AttestKind::Approval => "approval",
        _ => "unknown",
    }
}

pub(crate) fn status_name(status: AttestStatus) -> &'static str {
    match status {
        AttestStatus::Pass => "pass",
        AttestStatus::Fail => "fail",
        AttestStatus::Pending => "pending",
        AttestStatus::Error => "error",
        _ => "unknown",
    }
}

//! `graft attest <rev> --check NAME --pass|--fail [--url URL]`: records a
//! check result for a changeset (`docs/design.md` §8.3). CI posts these for
//! the checks an `attested` line requires (decision D6: `ci/build` and
//! `ci/test` on `main`), on the stack's head before it lands; a failing
//! check on a landed changeset of a post-land-tested line asks for a revert.
//! The server takes the attester from the token, and refuses an agent's.
//! No working copy is needed: `--repo` names the repository.

use graft_proto::api::RepoApi;
use graft_proto::types::Attest;
use serde_json::json;

use crate::cli::{AttestArgs, Global};
use crate::conn::{call_context, connect, runtime, working_copy_info};
use crate::error::Failure;
use crate::render::{print_json, short};

/// Posts one attestation.
pub(crate) fn run(g: &Global, args: &AttestArgs) -> Result<(), Failure> {
    let passed = args.pass;
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let changeset = conn.resolve(&cx, &args.rev).await?;
        conn.remote
            .attest(
                &cx,
                Attest {
                    repo: conn.repo.clone(),
                    changeset,
                    check: args.check.clone(),
                    passed,
                    url: args.url.clone(),
                },
            )
            .await?;
        if g.json {
            return print_json(&json!({
                "changeset": changeset.to_string(),
                "check": args.check,
                "passed": passed,
                "url": args.url,
            }));
        }
        let result = if passed { "passed" } else { "failed" };
        outln!("attested {} {result} on {}", args.check, short(changeset));
        Ok(())
    })
}

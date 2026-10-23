# ci/: workflow templates for graft-managed repositories

Templates for repositories whose `main` is managed by graft (design §8.3,
§11.3, [D6](../docs/decisions.md#d6-landing-gate)). They are not run by this
repository's own CI, and `.github/workflows/` does not use them.

| File | Purpose |
| --- | --- |
| `github/graft-pre-land.yml` | Reusable workflow template, one copy per repository. Runs the required checks on a `graft/*` branch, then a separate job posts an `Attest` for each. |
| `github/graft-post-land.yml` | Reusable workflow template, one copy per repository. Runs checks on the mirror's pushes to a line's branch and posts `post-land/*` attestations, which the auto-revert bot reads. |
| `github/caller-pre-land.yml`, `github/caller-post-land.yml` | Callers to copy into a repository's `.github/workflows/`. |

## How a draft gets its attestation

1. `graft land` on a git-primary or `attested` line makes the mirror push the
   draft's stack to `graft/<workspace>/<short-id>`. Every exported commit
   carries a `Graft-Changeset:` trailer.
2. The caller workflow runs on that push and calls the shared
   `graft-pre-land` workflow. One job per required check runs the command.
   These jobs run the branch's own code and have **no** `id-token`.
3. A separate `attest` job (no checkout, no branch code) reads each check's
   result from `needs`, asks GitHub for an OIDC token (audience = the graft
   deployment), and runs `graft ci exchange --changeset <id>`. The server
   checks that the job's commit (the token's `sha` claim) is the commit the
   mirror exported for that changeset, and only then mints a token **pinned to
   that changeset**. The job then calls `Attest { changeset, check, passed, url }`
   for each check. A failing check is attested as failed, so the land is
   `Rejected` with the run's URL rather than waiting.
4. An `attested` land of that draft succeeds once every check the line's
   policy names has a passing attestation.

Post-land, the mirror's push of the landed changeset runs the shared
`graft-post-land` workflow the same way, attesting `post-land/<name>` on the
landed changeset. A failed one is the auto-revert bot's signal.

## The exchange

`graft_auth::oidc` (crate `graft-auth`) is the server side. It verifies the
CI job's JWT and mints a graft token:

- The JWT is checked against the issuer's JWKS: `RS256` or `EdDSA` only (never
  `none` or HMAC), a named `kid`, the signature, then `iss` (exact),
  `aud` (must include the deployment's audience), `exp` (no leeway),
  `nbf`/`iat` (60 s leeway), and a lifetime of at most an hour.
- The CI repository must have a `Binding` to a graft repository. A good token
  for any other repository is refused (`PermissionDenied`). A binding may also
  require the numeric repository id (so a re-created name does not inherit it),
  restrict refs, and pin the workflow. A repository may have several bindings
  (for example one for pre-land and one for post-land); the first whose ref and
  workflow rules the token passes is used.
- A binding with a `write` scope can attest, so its tokens come only from
  `Exchange::exchange_pinned`. The caller names the changeset, and supplies a
  check that the token's `sha` is the commit exported for it. A token with no
  `sha`, or a `sha` that does not match, is refused. A write binding must also
  name its refs and, for GitHub, pin the reusable workflow. `Exchange::exchange`
  (no pin) refuses write bindings with `PinRequired`.
- The graft token is `kind = ci`, subject
  `ci:<provider>:<owner/repo>#<changeset>`, carries exactly the binding's
  scopes (`read` and `write`, never `land` or `admin`), and lives at most 15
  minutes and never past the CI token's own expiry.

### The server must enforce the pin

The token's subject carries the pinned changeset (`graft_auth::oidc::pinned_changeset`).
The pin only has an effect where `Attest` is checked: **the server (or engine)
must refuse an `Attest` from a `ci` principal whose subject is not pinned to
`req.changeset`**. `graft-engine`'s `do_attest` does not do this yet; it is the
one change in another crate that this design needs before CI bindings go live
(tracked with the exchange RPC, WI-2271). Until it does, a CI write token can
attest any changeset in its repository for its 15 minutes.

### Server configuration (what an operator sets)

For each GitHub repository, two bindings:

```text
issuer     https://token.actions.githubusercontent.com
audience   <a value unique to this graft deployment>

pre-land   owner/repo -> graft repo `myrepo`
           repository id  <numeric id, from the GitHub API>
           refs           refs/heads/graft/        (prefix: ends with `/`)
           workflows      <shared-org>/graft-ci/.github/workflows/graft-pre-land-myrepo.yml@refs/heads/main
           scopes         repo:myrepo:read   repo:myrepo:write:<prefix>

post-land  owner/repo -> graft repo `myrepo`
           repository id  <same>
           refs           refs/heads/main          (exact)
           workflows      <shared-org>/graft-ci/.github/workflows/graft-post-land-myrepo.yml@refs/heads/main
           scopes         as above
```

### What a CI write token can do (operator decision)

`Attest` needs a `write` scope on the repository, any prefix: the engine's
check is `Need::Workspace`, and the path prefix is not looked at for an
attestation. The same check admits `OpenWorkspace`, `RenewLease`, `SetHead`
(which can adopt abandoned drafts), `CloseWorkspace` and `Sync`. So a CI token
cannot change repository *content* (a prefixed write scope limits changes to
that prefix, which should be a path nothing real lives under), **but it can
open, move, adopt and close draft stacks and workspaces in that repository**
for its 15 minutes. The changeset pin limits `Attest` only.

That is the exposure of using `write` for CI today. The tidier fix is a
dedicated `attest` scope, which changes the scope grammar and the engine
(`graft-auth` and `graft-engine`); until then, enabling a write binding is an
operator decision.

### Trust model

- **The shared workflows take no inputs.** Anyone who can push a branch can
  write the caller. If the caller chose the checks, the endpoint or the
  runner, a branch author could run `true` on an exported commit and attest it.
  So each repository gets its own copy of the template in a shared workflow
  repository the branch authors cannot push to, with its checks fixed there,
  and the binding pins that file by `job_workflow_ref` (including the ref,
  `@refs/heads/main`, and protect that branch).
- **Pin to a commit, not a trailer.** The trailer only names what the job wants
  to attest. The server accepts it only if the OIDC `sha` is the commit the
  mirror exported for that changeset. A branch pushed with a victim's trailer
  has a different `sha`, so no token is minted.
- **Checks and attest are different jobs.** The check jobs run the branch's
  code with `contents: read` only. The attest job holds `id-token: write`,
  never checks out the branch, and takes results from `needs`. On a persistent
  self-hosted runner, the attest job must run on a runner the check jobs cannot
  modify (an ephemeral runner, or a separate runner group), or branch code
  could replace `graft`, `curl` or `jq`.
- The graft token and the OIDC token stay inside the attest step's shell
  variables and are masked. The graft token reaches `graft` through the
  environment, never through an argument.
- Re-running a job reuses the same commit, so an old failing run can be
  re-run to pass: that attests a real pass of the real checks. Nothing in the
  token lets it be attested for a different commit.

## What the templates expect from the `graft` CLI

Neither exists yet. They belong to `graft-cli` and `graft-server` (WI-2271):

- `graft ci exchange --provider github|woodpecker --changeset <id>`: reads the
  CI JWT on stdin, calls a server RPC that runs
  `graft_auth::oidc::Exchange::exchange_pinned` (its commit check is the
  mirror map's "git sha is the export of this changeset"), prints the graft
  token on stdout.
- `graft attest --changeset <id> --check <name> --status passed|failed --url <url>`:
  calls `Attest` with `GRAFT_TOKEN` (or `--token-file`) as the bearer token.
  (`attest` is `Surface::Excluded` in `graft-cli/src/api.rs` today.)

## Woodpecker

`Provider::Woodpecker` verifies a JWT whose `repo` (`owner/name`), `repo_id`,
`ref` and `commit` claims name the pipeline's repository, with the same issuer,
audience, lifetime and binding rules. The claim names are an assumption about
the job token an estate Woodpecker will issue; check them against a real token
before enabling it. A Woodpecker pipeline is the branch's own file, so it
cannot be pinned to a trusted workflow: a Woodpecker binding can hold `read`
only, and `exchange_pinned` refuses a Woodpecker `write` binding. There is no
Woodpecker template.

## Rehearsal

The plan asks for one live rehearsal of a draft gaining an attestation from a
real or `act`-run workflow. It is not done: `act` is not installed on the
build host, and the `graft ci exchange` and `graft attest` commands above do
not exist yet. Follow-up, once they do: run `graft-pre-land.yml` under `act`
against a scratch repository whose binding trusts a locally served JWKS, land
the draft with an `attested` policy before and after, and check `Rejected`
then `Landed`. `crates/graft-auth/tests/oidc.rs` covers the exchange itself
against recorded tokens.

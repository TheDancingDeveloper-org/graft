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
- A binding holds `repo:<r>:read` and, to attest, `repo:<r>:attest`, and
  nothing else: `Binding::new` refuses `write`, `land` and `admin`. A binding
  with `attest` gives tokens only through `Exchange::exchange_pinned`. The
  caller names the changeset, and supplies a check that the token's `sha` is
  the commit exported for it. A token with no `sha`, or a `sha` that does not
  match, is refused. An attest binding must also name its refs and, for
  GitHub, pin the reusable workflow. `Exchange::exchange` (no pin) refuses
  attest bindings with `PinRequired`.
- The graft token is `kind = ci`, subject
  `ci:<provider>:<owner/repo>#<changeset>`, carries exactly the binding's
  scopes (`read` and `attest`), and lives at most 15 minutes and never past
  the CI token's own expiry.

### The engine enforces the pin

The token's subject carries the pinned changeset (`graft_auth::oidc::pinned_changeset`).
`graft-engine`'s `do_attest` refuses, with `PermissionDenied`, an `Attest` from
a `ci` principal whose subject is not pinned to exactly `req.changeset`
(pinned to another changeset, or not pinned at all), before it looks the
changeset up. So a CI token attests the one changeset it was minted for.

### Server configuration (what an operator sets)

For each GitHub repository, two bindings:

```text
issuer     https://token.actions.githubusercontent.com
audience   <a value unique to this graft deployment>

pre-land   owner/repo -> graft repo `myrepo`
           repository id  <numeric id, from the GitHub API>
           refs           refs/heads/graft/        (prefix: ends with `/`)
           workflows      <shared-org>/graft-ci/.github/workflows/graft-pre-land-myrepo.yml@refs/heads/main
           scopes         repo:myrepo:read   repo:myrepo:attest

post-land  owner/repo -> graft repo `myrepo`
           repository id  <same>
           refs           refs/heads/main          (exact)
           workflows      <shared-org>/graft-ci/.github/workflows/graft-post-land-myrepo.yml@refs/heads/main
           scopes         as above
```

### What a CI token can do

`Attest` needs `repo:<r>:attest` (or `admin`, which CI never holds). `attest`
grants `Attest` and nothing else: no read, no write, no land, no workspaces.
`write` does not imply it, so a token with only `write` cannot attest. A CI
token therefore reads the repository and attests the one changeset it is
pinned to, and cannot open, move, adopt or close drafts or workspaces
(operator decision, WI-2272; before it, CI attested with a `write` scope on an
unused prefix, which also admitted those workspace calls).

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
  never checks out the branch, and takes results from `needs`. It runs on a
  GitHub-hosted runner by default, which is fresh for every job; on a
  persistent self-hosted runner, branch code could replace `graft`, `curl` or
  `jq` before it runs, so a self-hosted attest runner must be ephemeral or in a
  runner group the check jobs cannot use. It installs the `graft` CLI from a
  URL and SHA-256 you set in the template, and the graft endpoint must be
  reachable from GitHub's runners.
- The OIDC token stays in the attest step's shell variables and is masked. The
  graft token is written by `graft ci exchange --out` to a 0600 file and read
  from there with `--token-file`: never stdout, an argument or the environment.
- Re-running a job reuses the same commit, so an old failing run can be
  re-run to pass: that attests a real pass of the real checks. Nothing in the
  token lets it be attested for a different commit.

## What the templates expect from the `graft` CLI

`graft attest <rev> --check NAME --pass|--fail [--url URL]` exists (G3.5).
The templates call it with `--token-file`, as the global option takes the
bearer token from a file.

`graft ci exchange` does not exist yet; it belongs to `graft-cli` and
`graft-server` (WI-2271), and the templates expect this form:

- `graft ci exchange --provider github|woodpecker --changeset <id> --out PATH`:
  reads the CI JWT on stdin, calls a server RPC that runs
  `graft_auth::oidc::Exchange::exchange_pinned` (its commit check is the
  mirror map's "git sha is the export of this changeset"), and writes the
  graft token to `PATH`, created 0600. It never prints the token.

## Woodpecker

`Provider::Woodpecker` verifies a JWT whose `repo` (`owner/name`), `repo_id`,
`ref` and `commit` claims name the pipeline's repository, with the same issuer,
audience, lifetime and binding rules. The claim names are an assumption about
the job token an estate Woodpecker will issue; check them against a real token
before enabling it. A Woodpecker pipeline is the branch's own file, so it
cannot be pinned to a trusted workflow: a Woodpecker binding can hold `read`
only, and `exchange_pinned` refuses a Woodpecker `attest` binding. There is no
Woodpecker template.

## Rehearsal

The plan asks for one live rehearsal of a draft gaining an attestation from a
real or `act`-run workflow. It is not done: `act` is not installed on the
build host, and `graft ci exchange` and the server's exchange RPC (WI-2271)
do not exist yet. Follow-up, once they do: run `graft-pre-land.yml` under `act`
against a scratch repository whose binding trusts a locally served JWKS, land
the draft with an `attested` policy before and after, and check `Rejected`
then `Landed`. `crates/graft-auth/tests/oidc.rs` covers the exchange itself
against recorded tokens.

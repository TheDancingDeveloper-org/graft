# ci/: workflow templates for graft-managed repositories

Templates for repositories whose `main` is managed by graft (design §8.3,
§11.3, [D6](../docs/decisions.md#d6-landing-gate)). They are not run by this
repository's own CI, and `.github/workflows/` does not use them.

| File | Purpose |
| --- | --- |
| `github/graft-pre-land.yml` | Reusable workflow. Runs the required checks on a `graft/*` branch and posts an `Attest` for each. |
| `github/graft-post-land.yml` | Reusable workflow. Runs checks on the mirror's pushes to a line's branch and posts `post-land/*` attestations, which the auto-revert bot reads. |
| `github/caller-pre-land.yml`, `github/caller-post-land.yml` | Callers to copy into a repository's `.github/workflows/`. |

## How a draft gets its attestation

1. `graft land` on a git-primary or `attested` line makes the mirror push the
   draft's stack to `graft/<workspace>/<short-id>`. Every exported commit
   carries a `Graft-Changeset:` trailer.
2. The caller workflow runs on that push and calls `graft-pre-land.yml`. One
   job per required check runs the command.
3. After the command, the job asks GitHub for an OIDC token (audience = the
   graft deployment), exchanges it for a short graft token, and calls
   `Attest { changeset, check, passed, url }` with the changeset from the head
   commit's trailer. A failing check is attested as failed, so the land is
   `Rejected` with the run's URL rather than waiting.
4. An `attested` land of that draft succeeds once every check the line's
   policy names has a passing attestation.

Post-land, the mirror's push of the landed changeset runs `graft-post-land.yml`,
which attests `post-land/<name>`. A failed one is the auto-revert bot's signal.

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
  restrict refs, and pin the workflow.
- The graft token is `kind = ci`, subject `ci:<provider>:<owner/repo>`,
  carries exactly the binding's scopes (`read` and `write`, never `land` or
  `admin`), and lives at most 15 minutes and never past the CI token's own
  expiry.

### Server configuration (what an operator sets)

For each GitHub repository:

```text
issuer     https://token.actions.githubusercontent.com
audience   <a value unique to this graft deployment>
binding    owner/repo -> graft repo `myrepo`
           repository id  <numeric id, from the GitHub API>
           refs           refs/heads/graft/   (prefix)   refs/heads/main (exact)
           workflows      <shared-org>/graft-ci/.github/workflows/graft-pre-land.yml@refs/heads/main
                          <shared-org>/graft-ci/.github/workflows/graft-post-land.yml@refs/heads/main
           scopes         repo:myrepo:read   repo:myrepo:write:<prefix>
```

`Attest` needs a `write` scope on the repository (any prefix; the paths are
not checked for an attestation). Give CI a prefix nothing real lives under, so
the token cannot change content. A dedicated `attest` scope would be tidier;
it is a change to the scope grammar and the engine, noted in the hand-off.

### Trust model, and what the templates cannot enforce

- Anyone who can push a branch named `graft/*` to GitHub can run workflows
  there. Pin `job_workflow_ref` in the binding to the shared reusable workflow
  so a workflow added on the branch gets no token.
- A caller chooses the shared workflow's `checks` input, so a branch author can
  name a check that always passes. For repositories where the branch author is
  not trusted, put the check commands in a per-repository copy of the shared
  workflow (not an input), and pin that copy.
- The graft token and the OIDC token stay inside one step's shell variables and
  are masked. The graft token reaches `graft` through the environment, never
  through an argument.

## What the templates expect from the `graft` CLI

Neither exists yet. They belong to `graft-cli` and `graft-server`, which this
item does not own, and are the follow-up that makes the workflows runnable:

- `graft ci exchange --provider github|woodpecker`: reads the CI JWT on stdin,
  calls a server RPC that runs `graft_auth::oidc::Exchange::exchange`, prints
  the graft token on stdout.
- `graft attest --changeset <id> --check <name> --status passed|failed --url <url>`:
  calls `Attest` with `GRAFT_TOKEN` (or `--token-file`) as the bearer token.
  (`attest` is `Surface::Excluded` in `graft-cli/src/api.rs` today.)

## Woodpecker

`Provider::Woodpecker` verifies a JWT whose `repo` (`owner/name`), `repo_id`
and `ref` claims name the pipeline's repository, with the same issuer,
audience, lifetime and binding rules. The claim names are an assumption about
the job token an estate Woodpecker will issue; check them against a real token
before enabling it. There is no Woodpecker template yet.

## Rehearsal

The plan asks for one live rehearsal of a draft gaining an attestation from a
real or `act`-run workflow. It is not done: `act` is not installed on the
build host, and the `graft ci exchange` and `graft attest` commands above do
not exist yet. Follow-up, once they do: run `graft-pre-land.yml` under `act`
against a scratch repository whose binding trusts a locally served JWKS, land
the draft with an `attested` policy before and after, and check `Rejected`
then `Landed`. `crates/graft-auth/tests/oidc.rs` covers the exchange itself
against recorded tokens.

# Attest a changeset from GitHub Actions

A trusted GitHub Actions job can exchange its GitHub OpenID Connect (OIDC)
identity token for a short-lived graft token and use that token to post an
`Attest` with `kind = check` for one changeset. The exchange does not require a
stored GitHub secret.

The server administrator must first configure a GitHub issuer and a repository
binding. The binding pins the GitHub repository, numeric repository ID, ref,
and workflow. Its only scope should be `repo:<repository>:attest`. See the
[server configuration reference](server.md#ci-ciissuers-and-cibindings).

The server verifies the OIDC signature and claims, then verifies that the job's
Git commit is the recorded export of the requested changeset. A successful
exchange returns a token that:

- expires within 15 minutes and never outlives the GitHub identity token;
- has attest-only access to the bound graft repository;
- is pinned to the requested changeset; and
- cannot land changes or act on another changeset.

## Workflow shape

`graft attest` and `graft ci` are experimental client commands in this
release. Set `GRAFT_EXPERIMENTAL=1` in the job's environment to enable them.

Grant `id-token: write` only to the attestation job. The job requests an OIDC
token with the audience configured by the server, pipes it to
`graft ci exchange`, and writes the graft token to a mode-0600 temporary file:

```sh
umask 077
token_file=$(mktemp "$RUNNER_TEMP/graft-token.XXXXXX")
trap 'rm -f "$token_file"' EXIT

printf '%s' "$oidc_token" | graft --endpoint "$GRAFT_ENDPOINT" \
  ci exchange --provider github --changeset "$changeset" --out "$token_file"
unset oidc_token

graft --endpoint "$GRAFT_ENDPOINT" --repo "$GRAFT_REPO" \
  --token-file "$token_file" attest "$changeset" \
  --check "ci/github-oidc-rehearsal" --pass --url "$run_url"
```

The identity token stays in a masked shell variable only long enough to feed
standard input. The exchanged graft token stays in the private file and is
read with `--token-file`. Never put either token in command arguments, logs,
step output, artifacts, or an environment variable.

Keep the attestation job separate from jobs that execute untrusted repository
code. The steps that request and exchange the identity token must come from a
protected ref. A batch branch's files come from landed changesets, so a copy of
those steps on the branch cannot be trusted to hold a token.

Put the token-holding steps in a reusable workflow and pin every caller to the
protected ref. The reusable workflow is
[`.github/workflows/graft-ci-attest.yml`](../../.github/workflows/graft-ci-attest.yml)
(`on: workflow_call`, with a `changeset` input). An identical sample is
[`ci/github/graft-ci-attest.yml`](../../ci/github/graft-ci-attest.yml). Pin the
server binding's workflow list to that file at `refs/heads/main`. GitHub then
sets the OIDC `job_workflow_ref` to the called workflow on main, while `ref`
and `sha` stay the branch the job was dispatched on.

Dispatch it from
[`.github/workflows/graft-ci-attest-dispatch.yml`](../../.github/workflows/graft-ci-attest-dispatch.yml).
That workflow is `on: workflow_dispatch` with a `changeset` input and one job:

```yaml
jobs:
  attest:
    permissions:
      contents: read
      id-token: write
    uses: TheDancingDeveloper-org/graft/.github/workflows/graft-ci-attest.yml@main
    with:
      changeset: ${{ inputs.changeset }}
```

The `@main` pin is required. Calling the reusable workflow by a relative path
would make `job_workflow_ref` the branch's own file. To rehearse a batch
branch, run this dispatch workflow on `refs/heads/graft/batch-N` and pass the
changeset that branch's commit exports. Allow that ref with a prefix rule such
as `refs/heads/graft/` (a trailing `/` matches every ref under the prefix) and
keep `refs/heads/main` in the same list. Leave the job off the required checks.

## Post-land checks

On a line whose checks run after the land, the checker dispatches
[`.github/workflows/graft-post-land.yml`](../../.github/workflows/graft-post-land.yml)
on the land's check ref (under `refs/heads/graft/`) with three inputs:
`changeset`, `attempt` and `request_id`. The run name carries the
`request_id`, so the checker can find its run. The workflow refuses refs
outside `refs/heads/graft/`, and it runs one run per ref at a time; a queued
run is never cancelled by a run on another ref.

Its one job calls
[`.github/workflows/graft-post-land-check.yml`](../../.github/workflows/graft-post-land-check.yml)
pinned at `@main`, and only that call gets `id-token: write`. The called
workflow has two kinds of job:

- `fmt`, `clippy` and `test` run the same commands as the repository's
  required checks, on the self-hosted pool. They check out the landed commit
  with `contents: read`, no `id-token` and no secrets.
- `attest` runs on the attestation runner after the checks, even when one
  failed. It checks out no repository code. It exchanges its identity once
  and posts a pass (`ci/fmt`, `ci/clippy`, `ci/test`) for each check job that
  succeeded. A failed, skipped or cancelled check posts nothing; the checker
  reads the missing pass as not passed.

Add `graft-post-land-check.yml` at `refs/heads/main` to the binding's
workflow list. GitHub dispatches only workflows that exist on the default
branch, so both files must be on main before the first dispatch. Leave the
workflow off the required checks.

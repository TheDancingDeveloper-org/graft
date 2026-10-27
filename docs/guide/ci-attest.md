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
code. Pin the allowed workflow in server configuration, and protect the ref
that contains it. The manual workflow is installed at
`.github/workflows/graft-ci-attest.yml`, and an identical reusable sample is
available at [`ci/github/graft-ci-attest.yml`](../../ci/github/graft-ci-attest.yml).

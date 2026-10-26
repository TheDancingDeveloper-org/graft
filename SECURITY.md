# Security policy

## Reporting a vulnerability

Report a vulnerability privately through GitHub's private vulnerability
reporting for this repository: open the **Security** tab and choose
**Report a vulnerability**. Don't open a public issue, pull request or
discussion for a security problem.

We aim to acknowledge a report within <N> days.

## Supported versions

Only the latest 0.x release gets security fixes. Upgrade to it before you
report, if you can.

| Version          | Supported |
| ---------------- | --------- |
| latest 0.x       | yes       |
| older releases   | no        |

## Threat model

The graft server is the trust boundary: it holds the history, checks every
request and does every write. Tokens are signed bearer tokens with scopes;
anyone who holds a token can use it, within its scopes, until it expires or
is revoked. A repository is the read boundary: a token that can read a
repository can read every path in it, because there are no per-path read
restrictions yet. A node verifies tokens locally, with no call back to an
identity service. Content is verified by hash at every tier, so a corrupted or substituted object is rejected, not served.

## Token rules for users

- Never put a token in a command argument, a log, or command output. graft
  takes no token argument: it reads a token only from a file
  (`--token-file`, or the token file of a server stored with `graft login`)
  or from the `GRAFT_TOKEN` environment variable.
- Keep a token file private (mode `0600`). `graft login` reads the token on
  standard input and stores it with mode `0600`; `graft ci exchange` writes
  its token file with mode `0600`; `graft import` hands the token to the
  import tool through a `0600` file in a private temporary directory, never
  through an argument.
- Ask for short lifetimes. The server caps every minted token at its
  configured maximum, and a token you mint can't outlive the token you used.
- If a token leaks, have the server operator revoke it. Revocation is recorded
  by token id, so the server never stores the token itself. A revoked token is
  refused for writes at once; reads may still succeed until it expires, so
  keep lifetimes short.

Server operators: see [Tokens and scopes](docs/guide/server.md#tokens-and-scopes)
for minting, scopes and revocation.

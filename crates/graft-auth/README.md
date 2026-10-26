# graft-auth

Tokens, scopes and path rules for graft (`docs/design.md` §12).

## Token format

A graft token is a [PASETO](https://github.com/paseto-standard/paseto-spec)
`v4.public` token: Ed25519 over the pre-authentication encoding of the
header, the message, an empty footer and an empty implicit assertion.

```text
v4.public.<base64url(message || signature)>
```

The message is compact JSON with the fields in this order (optional fields
are left out when absent):

| Field | Meaning |
| --- | --- |
| `sub` | subject: user, session or service name |
| `kind` | `human`, `agent`, `ci` or `service` |
| `session` | session id (set by the caller's orchestrator, if any), optional |
| `work_item` | work item (set by the caller's orchestrator, if any), optional |
| `scopes` | scope strings, design §12.1 text forms |
| `exp` | expiry, integer seconds since the epoch |
| `jti` | unique token id, for the denylist |

Unknown fields are ignored on verify, so a later server can add claims
without breaking older verifiers.

### Why PASETO v4.public, and why our own ~60 lines of it

- **PASETO, not JWT.** v4.public fixes the algorithm (Ed25519) in the
  version header. There is no `alg` field to downgrade or confuse, and no
  `none`. It is compact and URL-safe.
- **Not a bespoke format.** The envelope is standard, so any PASETO v4
  library can verify a graft token with our public key. The unit test
  `paseto_vector_4_s_1` checks our encoder and verifier against the official
  test vector.
- **Hand-rolled envelope on `ed25519-dalek`, not `pasetors`.** The
  envelope is a length-prefixed concatenation and one signature; the
  cryptography stays in `ed25519-dalek` (`verify_strict`). Doing the
  envelope ourselves keeps the dependency tree small and keeps the claim set
  ours: `pasetors`' claims API expects RFC 3339 times, while graft uses
  integer `exp` like the rest of its API.
- **No footer.** graft has one signing key per server. A token with a footer
  is refused as malformed. Key rotation, when it comes, can add a `kid`
  footer as a new, explicit format version.

The encoding is frozen by the golden tokens in `tests/tokens.rs`. Changing it
is a durable-format change: it needs a migration and an operator decision.

## Verification

`verify` checks, in order: size (at most `MAX_TOKEN_LEN`), header and shape,
signature, claims (every scope must parse), expiry (`now >= exp` is expired),
then the `jti` denylist. It needs no store lookup; the server backs
`Denylist` with the MetaStore on writes, and pure readers can pass
`NoDenylist`.

## Scopes

| Text form | Grants |
| --- | --- |
| `repo:<name>:read` | reads on the repository |
| `repo:<name>:write[:<prefix>]` | writes, to `<prefix>` and below when given; implies read |
| `repo:<name>:land:<line>` | landing on that line; implies read |
| `repo:<name>:attest` | posting attestations (`Attest`) only; implies nothing, and only `admin` implies it |
| `repo:<name>:admin` | everything on the repository |
| `objects:read` | content-addressed object reads (node daemon); only itself |

`authorize(principal, needed, path)` checks one needed scope. For writes the
caller passes each changed path; a prefixed `write` never grants a write that
names no path. Denials carry the needed scope and the path, never a token.

## Redaction

`Token`, `SigningKey` and `VerifyingKey` redact themselves in `Debug`, and
none implements `Display`. `AuthError` never carries a token. The tests in
`tests/redaction.rs` check `Debug`/`Display` output and captured JSON logs.

## CI exchange (`oidc`)

`graft_auth::oidc` turns a CI job's identity token (GitHub Actions OIDC, or a
Woodpecker job token) into a short graft token with `kind = ci`. The caller
supplies the issuer's already-fetched JWKS; the module does no network I/O.
`Exchange::exchange` checks, in order: size and shape, `alg` (`RS256` or
`EdDSA`; `none`, HMAC and everything else are refused before a key is looked
at), the named `kid`, the signature, `iss`, `aud`, `exp` (no leeway),
`nbf`/`iat` (60 s leeway), the lifetime, then the `Binding` for the token's
repository, ref and workflow. A valid token for a repository with no binding
is `PermissionDenied`; an invalid one is `Unauthenticated`. The minted token
carries exactly the binding's `read`/`attest` scopes and expires at the
earlier of 15 minutes and the CI token's own `exp`. See `ci/README.md` for the
workflow side and the trust model.

A binding holds `read` and `attest` only (`write`, `land` and `admin` are
refused). A binding with `attest` is exchanged only through
`Exchange::exchange_pinned`: the caller names the changeset and supplies a
check that the token's `sha` claim is the commit exported for it, and the
minted subject is `ci:<provider>:<repo>#<changeset>` (`oidc::pinned_changeset`).
The engine refuses an `Attest` from a `ci` principal not pinned to that
changeset. Attest bindings must name refs and (GitHub) pin the reusable
workflow; Woodpecker bindings are read-only.

# Server guide

This page covers a Graft server's local-development start, production
configuration, authentication, repository and line administration, operational
endpoints, and database upgrades.

## Quick start

For a loopback-only development server, run:

```sh
graft serve --dev
```

The command creates a `dev` repository, chooses a free loopback port, stores the
generated administrator token in the user configuration, and prints a `graft
clone` command. Its data defaults to `./.graft-serve` and its signing-key state
to `./.graft-serve-state`. Use `--repo`, `--listen`, `--data`, `--state`, or
`--server` to override those defaults. A non-loopback `--listen` also requires
`--insecure-bind`.

For a configured server, `graft serve` runs the production server role. It reads
`--config <file>`, then `GRAFT_SERVER_CONFIG`, then
`/etc/graft/server.toml`.

## Configuration reference

The configuration is TOML. Unknown keys and invalid combinations are rejected
at startup. Optional values shown as `none` are absent when omitted; required
values have no default and must be present when their table is configured.

A minimal production configuration is:

```toml
listen = "127.0.0.1:7878"
metrics = "127.0.0.1:9464"
objects_dir = "/var/lib/graft/objects"
meta_db = "/var/lib/graft/meta.db"
key_file = "/var/lib/graft/signing.key"
json_logs = true

[[lines]]
name = "main"
policy = "direct"
```

### Server keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | `"127.0.0.1:7878"` | Listener for gRPC, the JSON facade, MCP, metrics, and health endpoints. |
| `metrics` | string or none | none | Optional second listener serving only metrics and health endpoints. |
| `objects_dir` | path | `"/var/lib/graft/objects"` | Filesystem object-store root. |
| `meta_db` | path | `"/var/lib/graft/meta.db"` | SQLite metadata database. |
| `key_file` | path or none | none | Ed25519 signing-key seed. When omitted, `signing.key` beside `meta_db`; created on first use with mode `0600`. |
| `min_free_fraction` | number | `0.05` | Writes are refused when either storage volume's free fraction is lower. |
| `min_free_bytes` | integer | `0` | Writes are refused when either storage volume has fewer free bytes. |
| `json_logs` | boolean | `true` | Emit JSON logs instead of plain text. |
| `otlp_endpoint` | string or none | none | OTLP/HTTP trace endpoint. |
| `max_token_ttl_s` | integer | `43200` | Maximum lifetime, in seconds, for tokens minted through the API. |
| `shutdown_grace_s` | integer | `10` | Graceful-shutdown wait for in-flight calls, in seconds. |
| `max_watch_wait_ms` | integer | `60000` | Maximum idle wait requested by one watch call, in milliseconds. |
| `max_watch_lifetime_s` | integer | `3600` | Maximum watch-stream lifetime, also bounded by token expiry. |
| `max_watches_per_principal` | integer | `16` | Concurrent watch streams allowed per principal. |
| `max_watches` | integer | `1024` | Concurrent watch streams allowed server-wide. |
| `max_put_call_bytes` | integer | `67108864` | Maximum frame bytes buffered by one `PutObjects` call; cannot exceed 80 MiB. |
| `put_buffer_bytes` | integer | `536870912` | Shared frame-byte budget across in-flight `PutObjects` calls. |
| `put_wait_ms` | integer | `10000` | Maximum wait for shared put-buffer capacity; at most one hour. |
| `put_idle_ms` | integer | `30000` | Maximum gap between messages in one `PutObjects` call; at most one hour. |
| `ci` | table | defaults below | CI identity-exchange configuration. |
| `lines` | array of tables | `[]` | Per-line land-policy overrides. With no override, lines use `direct`. |
| `default_line` | table or none | none | Policy for lines not named by `lines`; it has no `name` key. |
| `gc` | table | defaults below | Scheduled garbage-collection configuration. |

### `[gc]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `gc.enabled` | boolean | `false` | Run the sweep scheduler. Deletion must also be enabled per repository. |
| `gc.interval_s` | integer | `3600` | Seconds between scheduled passes. |
| `gc.repos` | array of strings | `[]` | Repositories swept by the scheduler. |
| `gc.grace_s` | integer | `900` | Grace between marking an object doomed and deleting it; minimum 60 seconds. |
| `gc.retention_s` | integer | `604800` | Retention for abandoned and superseded drafts, in seconds. |
| `gc.verify` | boolean | `true` | Verify the plan against the ancestry walk before each pass. |

### `[ci]`, `[[ci.issuers]]`, and `[[ci.bindings]]`

With no issuer configured, CI token exchange is disabled.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `ci.max_token_ttl_s` | integer | `900` | Maximum exchanged-token lifetime; also capped by the source token. |
| `ci.issuers` | array of tables | `[]` | Trusted CI issuers. |
| `ci.bindings` | array of tables | `[]` | Mappings from CI repositories to Graft repositories. |
| `ci.issuers[].provider` | `github` or `woodpecker` | required | CI provider. |
| `ci.issuers[].issuer` | string | required | Exact expected issuer claim. |
| `ci.issuers[].audience` | string | required | Deployment-specific expected audience. |
| `ci.issuers[].jwks_file` | path or none | none | JWKS file read at startup; mutually exclusive with `jwks_url`. |
| `ci.issuers[].jwks_url` | string or none | none | HTTPS JWKS URL, or HTTP on loopback; mutually exclusive with `jwks_file`. |
| `ci.issuers[].jwks_refresh_s` | integer | `3600` | Refresh interval for URL-backed keys. |
| `ci.issuers[].leeway_s` | integer | `60` | Clock-skew allowance for `nbf` and `iat`; maximum 300 seconds. |
| `ci.issuers[].max_lifetime_s` | integer | `3600` | Maximum accepted `exp - iat`. |
| `ci.bindings[].provider` | `github` or `woodpecker` | required | Provider whose identities the binding accepts. |
| `ci.bindings[].ci_repo` | string | required | CI repository in `owner/name` form. |
| `ci.bindings[].repo_id` | string or none | none | Numeric CI repository ID that must match when set. |
| `ci.bindings[].graft_repo` | string | required | Graft repository available to the job. |
| `ci.bindings[].refs` | array of strings | `[]` | Allowed refs; an empty list allows any ref. |
| `ci.bindings[].workflows` | array of strings | `[]` | Allowed workflows; GitHub bindings require a workflow restriction. |
| `ci.bindings[].scopes` | array of strings | required | One or more `read` or `attest` scopes on `graft_repo`. |

### `[[lines]]`, `[default_line]`, and path rules

`[[lines]]` entries require `name`. `[default_line]` accepts the same keys
except `name`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `lines[].name` | string | required | Line receiving this policy. Not valid in `[default_line]`. |
| `lines[].policy` | `direct`, `attested`, or `queued` | `direct` | Land policy. Only `direct` is enabled in this release; the others are parked. |
| `lines[].checks` | array of strings | `[]` | Required check names for non-direct policies. A direct line takes no checks. |
| `lines[].attest_merged` | boolean | `false` | Require attestations over merged changes; valid only with `attested`. |
| `lines[].authority` | `graft-primary` or `git-primary` | `graft-primary` | Which side of a mirrored line accepts writes. |
| `lines[].path_rules` | array of tables | `[]` | Prefix-specific merge and parked check/review rules. |
| `lines[].post_land` | table or none | none | Parked post-land policy. |
| `lines[].path_rules[].prefix` | string | required | Repository path prefix matched by the rule. |
| `lines[].path_rules[].checks` | array of strings | `[]` | Parked path-specific checks. |
| `lines[].path_rules[].human_review` | boolean | `false` | Parked path-specific human-review requirement. |
| `lines[].path_rules[].merge` | `line`, `none`, or omitted | omitted | `line` performs three-way line merge; `none` makes every concurrent change conflict. Omission uses the engine's normal merge behavior. |
| `lines[].post_land.checks` | array of strings | `[]` | Parked checks run by a post-land policy. |
| `lines[].post_land.auto_revert` | boolean | `false` | Parked automatic revert after a failed post-land check. |

Policies other than `direct`, all check fields, human-review requirements, and
post-land behavior are not enabled in this release. A direct land never runs a
build, test, check, or checker. Path merge rules are available independently:
use `merge = "line"` for bounded text merge or `merge = "none"` for a conflict
whenever both sides changed a matching path.

## Tokens and scopes

Tokens are signed bearer tokens. Clients read them from a token file or from
`GRAFT_TOKEN`; command-line token values are not accepted. Each scope is one of:

| Scope | Grants |
| --- | --- |
| `repo:<name>:read` | Read the repository. |
| `repo:<name>:write` | Write any repository path and read the repository. |
| `repo:<name>:write:<prefix>` | Write only the prefix and its descendants, and read the repository. |
| `repo:<name>:land:<line>` | Land on one line and read the repository. |
| `repo:<name>:attest` | Post attestations only; it does not grant read. |
| `repo:<name>:drafts` | Read other sessions' open drafts by path only. |
| `repo:<name>:admin` | Perform all repository operations, including attestations and draft-overlap reads. |
| `objects:read` | Read objects for repositories the caller can read. |

Mint an operator token offline with the server administration command, piping
stdout directly to a mode-`0600` file or secret store:

```sh
graft serve --config /etc/graft/server.toml admin token mint \
  --subject operator --kind human \
  --scope repo:example:admin --ttl 3600 > token.tmp
chmod 600 token.tmp
```

The default administration-command TTL is 30 days. API minting defaults to the
server's `max_token_ttl_s`, refuses a requested lifetime above that limit, and
never creates a token that outlives its caller. A non-service caller can mint
only its own identity and no broader scopes; a service principal may delegate a
different subject within its own scopes.

Revoke a live token by passing it on standard input, never as an argument:

```sh
graft serve --config /etc/graft/server.toml admin token revoke < token.tmp
```

Revocation stores only the token ID and expiry. It takes effect for writes;
read-only requests continue to accept an otherwise valid revoked token until it
expires.

## Repositories and lines

Create a repository with the offline administration command:

```sh
graft serve --config /etc/graft/server.toml admin repo create example
```

The command is idempotent. With a server running, create a line from an
existing changeset or ref:

```sh
graft --repo example line create release --from main
```

Line creation is compare-and-swap from absence: it never overwrites an existing
line. Land policy is server-wide by line name, not per repository. In this
release, use the default `direct` policy; configured `attested` and `queued`
policies are parked and not enabled.

## Metrics and health

Every main listener serves:

- `GET /healthz`: liveness; returns `ok` while the HTTP service is running.
- `GET /readyz`: readiness; returns `ready` only after startup is ready to serve.
- `GET /metrics`: Prometheus text exposition, including a fresh free-space
  admission sample.

When `metrics` is configured, the second listener serves only those three
endpoints. Keep it on a trusted network; these endpoints do not require bearer
authentication.

## Upgrade and schema versions

The current SQLite schema version is **7**, stored in SQLite's `user_version`.
Opening the metadata database applies each missing migration in order, one
migration per transaction, before the server begins serving. Existing data is
preserved across supported forward upgrades. Released migrations are append-only
and are never edited; a crash leaves the database at either the old version or
the next version, never between them. A database whose schema version is newer
than the running binary is refused rather than downgraded.

Back up the metadata database and object store before replacing a server binary.
Keep the signing key with the deployment: replacing it invalidates existing
tokens.

## Running with compose

Compose deployment instructions will be added below.

## Backup and restore

Backup and restore instructions will be added below.

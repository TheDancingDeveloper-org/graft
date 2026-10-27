# Runbook: v0.2 exit on graft (post-land checks)

This runbook turns on post-land checks for one line of the repository
`graft` on graft-dev, and runs the three v0.2 exit measurements (roadmap
v0.2, D40):

- **M1:** every land settles (good, or bad and reverted) within the bound:
  p95 ≤ 30 min and max ≤ 4 h. Time a line is held Blocked is excluded and
  reported separately.
- **M2:** a planted bad land is reverted with no person involved, and it
  reaches a batch PR only together with its revert.
- **M3:** a reviewer works one day's lands through `graft review-queue` or
  MCP, without opening the git host.

The plan is one rehearsal day on the scratch line `exit-rehearsal`, then
`backlog-1` for two weeks. graft stays git-primary on `main` throughout.
Nothing runs in the land path: landing on a checked line is still direct,
and checks only track each land after it has landed.

## The parts

| part | where | what it does |
| --- | --- | --- |
| `[[lines]] exit-rehearsal` | `deploy/server.toml` | `checks = "post-land"`, `required = ["ci/fmt", "ci/clippy", "ci/test"]`, `retries = 1`. Each land gets a `pending` land-checks row. |
| `[ci]` binding | `deploy/server.toml` | admits `graft-post-land-check.yml@refs/heads/main` to the CI token exchange, so its attest job can post passes |
| `[[check_refs]]` | `deploy/mirror-graft.toml` | `graft-mirror-graft` exports every land on the line, records the export, and pushes `refs/heads/graft/check/exit-rehearsal/<changeset>` |
| `graft-post-land` | `deploy/compose.yml`, profile `post-land` | the checker (`graft-attester --mode post-land --runner github`). It dispatches `graft-post-land.yml` on each check ref, reads the jobs, posts failures, bisects and lands reverts. |
| `post-land-token` | `/etc/graft`, minted by `up.sh` | the checker's graft token: `read`, `attest`, `write` on `graft`, `land` on the line. 0600, uid 10001. |
| `github-token` | `/etc/graft`, placed by hand | the mirrors' GitHub token, also read by the checker (operator decision: reuse it). 0600, uid 10001. |
| `bench/postland/` | a host with the graft CLI and Python 3 | recorder, planter and report (see `bench/README.md`) |

The GitHub checks run on the org self-hosted pool. The attest job runs on
the `[graft-dev]` runner and checks out no repository code. The checker
keeps at most `GRAFT_POST_LAND_MAX_IN_FLIGHT` runs (default 2) queued or in
progress at once, so it doesn't crowd out the merge queues.

## Before you start

1. **The image.** It must include the mirror's check refs and the
   checker's GitHub runner (main at 2f6a05f or later). The deploy PR repins
   `deploy/image.env` to it. Pre-flight in `up.sh` refuses a
   `[[check_refs]]` config on an older image.
2. **The workflows.** `graft-post-land.yml` and `graft-post-land-check.yml`
   must be on `main`. GitHub only dispatches a workflow that is on the
   default branch.
3. **The GitHub token's scope.** The checker sends
   `/etc/graft/github-token` to GitHub as a header. The token needs
   **Actions: read and write** on `TheDancingDeveloper-org/graft`, on top of
   the contents access the mirror uses. If it lacks Actions access, every
   dispatch gets a 403. The checker treats that as an outage: it posts no
   verdict, and lands stay `pending`. So M1 fails, but nothing is wrongly
   reverted. Check the token's permissions before the rehearsal.
4. **Security.** Both token files are opened read-only. They never appear
   in argv, the environment, logs or `Debug` output. The GitHub token is
   shared by the mirror and the checker and has broader scope than
   dispatch needs (operator decision, accepted). Any workflow on any branch
   can target the `[graft-dev]` runner (an accepted risk on graft-dev, to be
   revisited before any production use). **Every batch review must flag a
   batch that touches `.github/workflows/`.**

## 1. Create the line

```sh
GRAFT_TRIAL_REPO=graft GRAFT_TRIAL_LINE=exit-rehearsal trial/create-trial-line.sh
```

This forks the line at `main`'s current tip. A re-run leaves an existing
line alone. Lands from before the server config took effect have no
land-checks row. The checker starts from the first land it sees.

## 2. Turn the checker on

The profile is off unless enabled. Turn it on through the host settings
file, so that every later deploy keeps it on:

```sh
sudo sh -c 'printf "GRAFT_POST_LAND=1\n" >> /etc/graft/deploy.env'
# optional: GRAFT_POST_LAND_LINE (default exit-rehearsal),
# GRAFT_POST_LAND_MAX_IN_FLIGHT (default 2), GRAFT_POST_LAND_TIMEOUT_S (default 3000)
```

Then deploy as usual: the `deploy-dev` workflow, or `deploy/up.sh` on the
VM. With `GRAFT_POST_LAND=1`, `up.sh`:

- refuses to continue if `/etc/graft/github-token` is missing or empty;
- mints `/etc/graft/post-land-token` if it is absent or older than 20 days
  (subject `graft-post-land-graft`, TTL 30 days);
- builds the checker image (target `checker` of `deploy/attester/Dockerfile`,
  with no toolchain) and starts `graft-dev-post-land`.

To check it:

```sh
docker logs --tail 50 graft-dev-post-land       # follows the line; no token in the output
docker logs --tail 50 graft-dev-mirror-graft    # check refs pushed and settled refs deleted
```

To turn it off, remove the line from `deploy.env` and run
`docker compose --env-file deploy/image.env -f deploy/compose.yml --profile post-land rm -sf graft-post-land`.
Lands on the line stay `pending` until it is back. Nothing in the land
path waits for it.

## 3. Run the recorder

The recorder needs a **read-only** token of its own. Mint it into a file
owned by the user that runs the recorder, without printing it:

```sh
sudo install -d -m 0700 -o "$USER" /var/lib/postland
( umask 077; docker exec graft-dev-server graft-server admin token mint \
    --subject postland-recorder --kind service --scope repo:graft:read \
    --ttl 2592000 > /var/lib/postland/recorder-token )
```

Record the line's LSN before the first land you want to measure. That is
`START_LSN`, the head LSN from `graft api watch`. Then run the recorder in
the background, for example in a `tmux` session or a user systemd unit:

```sh
python3 -I bench/postland/record.py --repo graft --line exit-rehearsal \
    --endpoint http://graft-dev:17888 --token-file /var/lib/postland/recorder-token \
    --from-lsn "$START_LSN" --from-pos 0 --interval 60 --loop \
    --state /var/lib/postland/state.json --out /var/lib/postland/log.jsonl
```

`--from-lsn` and `--from-pos` are read on the first poll only. A restart
resumes from the state file with no duplicate rows. A gap in the
recorder's polls is logged, and its rows are marked `late`.

## 4. Keep the Blocked windows file

The server records no Blocked state or event. **Whenever the line is held
Blocked, whoever unblocks it records the window** in
`/var/lib/postland/blocked.json`:

```json
[
  {"from": "2026-10-28T09:14:00Z", "to": "2026-10-28T10:02:00Z"}
]
```

- `from` is the time of the checker's log line `line ... blocked at land ...`
  (`docker logs graft-dev-post-land`).
- `to` is the time the line was unblocked.

`report.py --blocked /var/lib/postland/blocked.json` subtracts these windows
from each land-to-settled sample and shows them as `blocked_s`. With no
file, the column reads `not recorded`. Keep the file up to date as each
block happens, not afterwards from memory.

## 5. Plant a bad land (M2)

The planter lands a bad change and then one good follower, so skip-ahead
fails at the tip and bisect has to find the plant. It runs in a working
copy of the line, with a token that may write to `graft` and land on the
line:

```sh
( umask 077; docker exec graft-dev-server graft-server admin token mint \
    --subject postland-plant --kind service --scope repo:graft:write \
    --scope repo:graft:land:exit-rehearsal --ttl 86400 > /var/lib/postland/plant-token )
# A named server in the graft user config whose token file is
# /var/lib/postland/plant-token, here called `plant`; the working copy
# records it, so plant.py's `graft land` uses that token.
graft clone graft /var/lib/postland/plant-wc --line exit-rehearsal --server plant
python3 -I bench/postland/plant.py --variant fmt --root /var/lib/postland/plant-wc \
    --record /var/lib/postland/plants.jsonl
```

The token never goes on the command line or in the environment.

- On the rehearsal day, plant `--variant fmt` (fails `ci/fmt` within
  minutes) and then `--variant test` (fails `ci/test`, the full path).
- During the two weeks, plant each variant at least once a week, at a time
  of day when other agents are landing too.
- After each plant, change nothing on the line by hand. The evidence is
  that the revert's actor is `graft-post-land-graft`, and the plant reads
  `reverted` and its revert reads `good`.

## 6. The reviewer day (M3)

Once a week, oversight starts a model session (Opus, low effort) whose
settings deny `gh`, the GitHub MCP servers, and web fetches to
`github.com`. The session gets a graft token with `read` and `attest` only.
Record the line's LSN at the start of the day. The reviewer loops
`graft review-queue --since-lsn <lsn>` (or the MCP tool), reads each land
with graft's diff and show, and posts an attestation of kind `review`. The
day passes if three things hold at the end of the day:

- the queue from the start LSN is empty;
- the recorder shows one `review` attestation per land;
- the session's tool log shows no call to GitHub.

## 7. Reports

On day 7 and day 14, write the results page:

```sh
python3 -I bench/postland/report.py --jsonl /var/lib/postland/log.jsonl \
    --plants /var/lib/postland/plants.jsonl --batches batches.json \
    --blocked /var/lib/postland/blocked.json \
    --checker-actor graft-post-land-graft \
    --prometheus "$PROM" --enabled-at "$ENABLED_AT" \
    --out docs/trials/<date>-v0-2-exit.md
```

`batches.json` lists each batch PR in the window with the
`Graft-Changeset` trailers of its commits. The page lands by direct PR.
File any failed measurement as a bug at once, with the evidence.

## 8. Token rotation (before day 30)

Graft tokens on graft-dev live 30 days at most.

- **Checker:** `up.sh` re-mints `post-land-token` on any deploy once it is
  older than 20 days, and restarts the checker. If no deploy is due, run
  one by hand before day 30. To force a re-mint of the checker token alone,
  remove the file (`sudo rm /etc/graft/post-land-token`) and run `up.sh`.
  `GRAFT_REMINT=1` would re-mint every mirror token as well.
- **Recorder and planter:** re-mint them by hand, as in steps 3 and 5,
  before day 30. The recorder reads its token file on every poll, so it
  needs no restart.
- **The GitHub token** is placed by hand and rotated with the mirrors'.

## 9. Switch to backlog-1

After the rehearsal day passes (both plants were reverted unattended, and
the measured settle time fits the bound), switch in a separate deploy PR:

1. `deploy/server.toml`: give `backlog-1` the same `checks`, `required` and
   `retries` as `exit-rehearsal`.
2. `deploy/mirror-graft.toml`: add `[[check_refs]] line = "backlog-1"` with
   prefix `graft/check/backlog-1/`. Remove the `exit-rehearsal` entry, or
   keep it if that line stays checked. The prefixes must not overlap.
3. Before that PR's deploy, set `GRAFT_POST_LAND_LINE=backlog-1` in
   `/etc/graft/deploy.env` and remove `/etc/graft/post-land-token`. The
   token's `land` scope names the line, so it must be re-minted. One
   checker serves one line.
4. Run `pr-batch` for `backlog-1` with `--checks post-land`, so batches cut
   only the settled prefix.
5. Restart the recorder with `--line backlog-1`, a new state file and log,
   and the new start LSN. Take the 14-day D31 baseline from Prometheus at
   the switch (`--enabled-at`).

Don't enable checks on any other repository's line, and don't make any
check required in the land path.

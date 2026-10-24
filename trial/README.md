# trial/

Scripts and the agent guide for graft trials that deliver through an
integration line and a batch PR (decisions D21, D26, D28).

| File | What |
| --- | --- |
| `agent-guide.md` | how an agent delivers a task through graft, not git |
| `create-trial-line.sh` | forks the trial line (`GRAFT_TRIAL_LINE`) from the repo's followed base line with `graft line create --from`; `GRAFT_TRIAL_REPO` (default `rusttorrent`) and `GRAFT_TRIAL_BASE` (default `main`; for vogt, `rust-core`) pick the repo |
| `install-cli.sh` | builds and installs the `graft` CLI from `origin/main` |
| `mint-agent-tokens.sh` | mints one agent token per trial session on graft-dev (`read`, `write`, `drafts`, `land:<line>`, for `GRAFT_TRIAL_REPO`), so the full pre-land check runs; pass `--session ses_…` (the worker's Vogt session) to bind it, so the server refuses a draft claiming another session |

Results go in `docs/trials/`.

# trial/

Scripts and the agent guide for graft trials that deliver through an
integration line and a batch PR (decisions D21, D26, D28).

| File | What |
| --- | --- |
| `agent-guide.md` | how an agent delivers a task through graft, not git |
| `create-trial-line.sh` | imports GitHub `main` into graft-dev and forks the trial line |
| `install-cli.sh` | builds and installs the `graft` CLI from `origin/main` |
| `mint-agent-tokens.sh` | mints one agent token per trial session on graft-dev |

Results go in `docs/trials/`.

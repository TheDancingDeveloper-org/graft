# WI-2345: adversarial harness design note

- `docs/notes/adversarial-harness.md`: the design for an adversarial test
  harness in four parts: racing lands under injected faults (WI-2345),
  merge3 against git (WI-2346), hostile git repositories (WI-2347) and
  hostile API clients (WI-2348). It covers a seeded adversary, a history
  recorder, a model checker, planted bugs that prove the harness catches
  them, and CI and soak budgets. Docs only; no design rule changes.

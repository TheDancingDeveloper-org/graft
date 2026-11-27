# Engineering record

These documents are the working record of how graft was designed and built:
plans, measurement write-ups, design notes and operations runbooks. They are
kept for maintainers and contributors who want the reasoning behind a
decision. They are **not** the user documentation, and they describe the
project as it was when each was written, including references to internal
work items (`WI-n`), plan items (`G<phase>.<n>`) and decisions (`D<n>`).

If you want to use graft, start at the [user guide](../guide/README.md). The
authoritative description of current behaviour is the guide and the code; the
current design is [docs/design.md](../design.md) and the decision log is
[docs/decisions.md](../decisions.md).

| Area | Contents |
| --- | --- |
| [plan.md](plan.md) | The phased build plan and the engineering backlog, kept as history |
| [landing.md](landing.md) | How this project lands changes through its own graft mirror |
| [notes/](notes) | Design notes for individual features (line fork points, draft overlap, the mirror service, the adversarial harness, and others) |
| [trials/](trials) | Write-ups of benchmark runs, rehearsals and live trials, with dates |
| [runbooks/](runbooks) | Operational procedures: authority switch, backup and restore, batch repair |
| [dev-stack.md](dev-stack.md) | The local and shared development stack |
| [known-flakes.md](known-flakes.md) | Quarantined CI jobs and the signature of each |

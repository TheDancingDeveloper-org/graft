- graft-harness: review follow-ups to the adversarial harness (WI-2345).
  The checker now fails an acknowledged land that carries no changes, a
  ref write without its `RefMoved` journal entry in the same transaction,
  and a no-op ref move. Replay commands for `interleaving_only`,
  `hostile_wide` and the planted-bug tests now replay the right shape and
  seed. A new probe races adjacent-line, delete-vs-edit and binary lands.
  The planted-bugs policy test covers every shipped workspace crate and no
  longer passes `--offline`.

//! `graft lsp`: owned by work item G3.6 (see docs/plan.md, wave 3.B note).

use crate::Exit;

/// Runs the LSP server over stdio.
pub(crate) fn run(_args: &[String]) -> Exit {
    eprintln!(
        "graft lsp: not implemented yet (see docs/plan.md, G3.6); {:?}",
        graft_lsp::COMMANDS
    );
    Exit::Usage
}

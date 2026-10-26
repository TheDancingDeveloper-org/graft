//! Command implementations, one module per command. Work items add their
//! own module file and one sorted `pub(crate) mod` line here.

pub(crate) mod attest;
pub(crate) mod blame;
pub(crate) mod cat;
pub(crate) mod ci;
pub(crate) mod clone;
pub(crate) mod commit;
pub(crate) mod completions;
pub(crate) mod config;
pub(crate) mod diff;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod land;
pub(crate) mod line;
pub(crate) mod log;
pub(crate) mod login;
pub(crate) mod ls;
pub(crate) mod lsp;
pub(crate) mod mcp;
pub(crate) mod pick;
pub(crate) mod revert;
pub(crate) mod show;
pub(crate) mod sparse;
pub(crate) mod stack;
pub(crate) mod status;
pub(crate) mod sync;
pub(crate) mod tag;
pub(crate) mod version;
pub(crate) mod workspace;

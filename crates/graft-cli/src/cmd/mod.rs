//! Command implementations, one module per command. Work items add their
//! own module file and one sorted `pub(crate) mod` line here.

pub(crate) mod blame;
pub(crate) mod cat;
pub(crate) mod clone;
pub(crate) mod diff;
pub(crate) mod log;
pub(crate) mod ls;
pub(crate) mod lsp;
pub(crate) mod show;
pub(crate) mod sparse;
pub(crate) mod status;
pub(crate) mod version;

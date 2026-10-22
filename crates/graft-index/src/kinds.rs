//! The derived kinds of `docs/design.md` §4.9.

use crate::{DerivedKind, Mode, ParentScope};

macro_rules! kind {
    ($(#[$doc:meta])* $name:ident, $str:literal, $parents:expr, $mode:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug)]
        pub struct $name;
        impl DerivedKind for $name {
            const NAME: &'static str = $str;
            const VERSION: u32 = 1;
            const PARENTS: ParentScope = $parents;
            const MODE: Mode = $mode;
        }
    };
}

kind!(
    /// `1 + max(parent generations)`.
    Generation, "generation", ParentScope::All, Mode::Eager
);
kind!(
    /// First-parent ancestors at distance 2^k.
    SkipList, "skiplist", ParentScope::First, Mode::Eager
);
kind!(
    /// `(line, pos)` of landed changesets.
    LinePos, "linepos", ParentScope::First, Mode::Eager
);
kind!(
    /// `(line, path-or-prefix, pos)` rows per changed path.
    LinePaths, "linepaths", ParentScope::First, Mode::Eager
);
kind!(
    /// Line → origin per `(changeset, path)`.
    Blame, "blame", ParentScope::First, Mode::Lazy
);

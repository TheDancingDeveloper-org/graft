//! `forward_workspace_api!` names every `WorkspaceApi` method with the trait's own
//! signature (WI-2639). The signatures are checked by the compiler: the
//! macro implements the trait below. The names are checked here against the
//! trait in `src/api.rs`, because a method the macro leaves out that has a
//! default would still compile.

use std::collections::BTreeSet;

use graft_proto::error::ApiResult;
use graft_proto::{ApiError, CallContext, WorkspaceApi};

macro_rules! names {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {
        const NAMES: &[&str] = &[$(stringify!($name)),*];
    };
}
graft_proto::forward_workspace_api!(names);

/// A `WorkspaceApi` written only by the macro: it compiles only if every listed
/// line matches a trait method's signature.
struct Probe;

macro_rules! refuse {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {$(
        async fn $name(&self, cx: &CallContext, $($arg: $ty),*) -> ApiResult<$out> {
            let _ = (cx, $($arg),*);
            Err(ApiError::unimplemented(stringify!($name)))
        }
    )*};
}

impl WorkspaceApi for Probe {
    graft_proto::forward_workspace_api!(refuse);
}

/// The leading identifier of `s` (letters, digits, `_`).
fn ident(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    &s[..end]
}

/// The `fn` names declared at the top level of the `WorkspaceApi` trait.
fn trait_methods() -> BTreeSet<String> {
    let api = include_str!("../src/api.rs");
    let at = api
        .find("pub trait WorkspaceApi:")
        .expect("WorkspaceApi in api.rs");
    let open = at + api[at..].find('{').unwrap();
    let mut depth = 0usize;
    let mut out = BTreeSet::new();
    for line in api[open..].lines() {
        if depth == 1
            && let Some(rest) = line.trim().strip_prefix("fn ")
        {
            out.insert(ident(rest).to_owned());
        }
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth == 0 {
            break;
        }
    }
    assert!(!out.is_empty(), "no WorkspaceApi methods read");
    out
}

#[test]
fn forward_workspace_api_lists_every_trait_method() {
    let listed: BTreeSet<String> = NAMES.iter().map(|n| (*n).to_owned()).collect();
    assert_eq!(listed.len(), NAMES.len(), "a method is listed twice");
    assert_eq!(listed, trait_methods());
    // The probe type exists only to be compiled.
    let _ = Probe;
}

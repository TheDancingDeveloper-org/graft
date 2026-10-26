//! Who made a change: the session, work item, model and co-authors that
//! the caller's orchestrator (if any) attaches to drafts and lands (D39).
//!
//! The names are orchestrator-neutral. Each one replaces a legacy name,
//! which stays readable forever: [`get`] and [`env`] prefer the new name
//! and fall back to the legacy one. Writers use only the new names.

use graft_model::changeset::Metadata;

/// Changeset metadata key for the session, if any.
pub const SESSION_KEY: &str = "agent.session";
/// Changeset metadata key for the work item, if any.
pub const WORK_ITEM_KEY: &str = "agent.work_item";
/// Changeset metadata key for the model that wrote the change, if any.
pub const MODEL_KEY: &str = "agent.model";
/// Changeset metadata key for the co-authors, one identity per line.
pub const COAUTHORS_KEY: &str = "agent.coauthors";

/// Legacy name of [`SESSION_KEY`], read as a fallback.
pub const LEGACY_SESSION_KEY: &str = "vogt.session";
/// Legacy name of [`WORK_ITEM_KEY`], read as a fallback.
pub const LEGACY_WORK_ITEM_KEY: &str = "vogt.work_item";
/// Legacy name of [`MODEL_KEY`], read as a fallback.
pub const LEGACY_MODEL_KEY: &str = "vogt.model";
/// Legacy name of [`COAUTHORS_KEY`], read as a fallback.
pub const LEGACY_COAUTHORS_KEY: &str = "vogt.coauthors";

/// Environment variable holding the session, set by the caller's
/// orchestrator, if any.
pub const SESSION_ENV: &str = "GRAFT_SESSION_ID";
/// Environment variable holding the work item, set by the caller's
/// orchestrator, if any.
pub const WORK_ITEM_ENV: &str = "GRAFT_WORK_ITEM";

/// Legacy name of [`SESSION_ENV`], read as a fallback.
pub const LEGACY_SESSION_ENV: &str = "VOGT_SESSION_ID";
/// Legacy name of [`WORK_ITEM_ENV`], read as a fallback.
pub const LEGACY_WORK_ITEM_ENV: &str = "VOGT_WORK_ITEM";

/// Git commit trailer carrying the work item on export.
pub const WORK_ITEM_TRAILER: &str = "Graft-Work-Item";
/// Legacy name of [`WORK_ITEM_TRAILER`], read as a fallback.
pub const LEGACY_WORK_ITEM_TRAILER: &str = "Vogt-Work-Item";

/// A logical attribution field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// The session.
    Session,
    /// The work item.
    WorkItem,
    /// The model that wrote the change.
    Model,
    /// The co-authors.
    Coauthors,
}

impl Key {
    /// Every key, in declaration order.
    pub const ALL: [Key; 4] = [Key::Session, Key::WorkItem, Key::Model, Key::Coauthors];

    /// The metadata key writers use.
    pub fn name(self) -> &'static str {
        match self {
            Key::Session => SESSION_KEY,
            Key::WorkItem => WORK_ITEM_KEY,
            Key::Model => MODEL_KEY,
            Key::Coauthors => COAUTHORS_KEY,
        }
    }

    /// The legacy metadata key, read as a fallback.
    pub fn legacy(self) -> &'static str {
        match self {
            Key::Session => LEGACY_SESSION_KEY,
            Key::WorkItem => LEGACY_WORK_ITEM_KEY,
            Key::Model => LEGACY_MODEL_KEY,
            Key::Coauthors => LEGACY_COAUTHORS_KEY,
        }
    }

    /// The environment variable and its legacy name, for the fields the
    /// caller sets through the environment (session and work item).
    pub fn env_names(self) -> Option<(&'static str, &'static str)> {
        match self {
            Key::Session => Some((SESSION_ENV, LEGACY_SESSION_ENV)),
            Key::WorkItem => Some((WORK_ITEM_ENV, LEGACY_WORK_ITEM_ENV)),
            Key::Model | Key::Coauthors => None,
        }
    }
}

/// The value of `key` in `meta`: the new key's value, else the legacy
/// key's.
pub fn get(meta: &Metadata, key: Key) -> Option<&str> {
    meta.get(key.name())
        .or_else(|| meta.get(key.legacy()))
        .map(String::as_str)
}

/// The value of `key` from the process environment: the new variable,
/// else the legacy one. Empty values count as unset. `None` for fields
/// with no environment variable.
pub fn env(key: Key) -> Option<String> {
    env_with(key, |name| std::env::var(name).ok())
}

/// [`env`] over a caller-supplied lookup, so callers and tests need not
/// touch the process environment.
pub fn env_with(key: Key, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    let (new, legacy) = key.env_names()?;
    let set = |name: &str| lookup(name).filter(|v| !v.is_empty());
    set(new).or_else(|| set(legacy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn meta(pairs: &[(&str, &str)]) -> Metadata {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn get_new_key_only() {
        for key in Key::ALL {
            assert_eq!(get(&meta(&[(key.name(), "new")]), key), Some("new"));
        }
    }

    #[test]
    fn get_legacy_key_only() {
        for key in Key::ALL {
            assert_eq!(get(&meta(&[(key.legacy(), "old")]), key), Some("old"));
        }
    }

    #[test]
    fn get_both_prefers_new() {
        for key in Key::ALL {
            let m = meta(&[(key.legacy(), "old"), (key.name(), "new")]);
            assert_eq!(get(&m, key), Some("new"));
        }
    }

    #[test]
    fn get_neither_is_none() {
        let m = meta(&[("other", "x")]);
        for key in Key::ALL {
            assert_eq!(get(&m, key), None);
        }
    }

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn env_new_legacy_both_neither() {
        for key in [Key::Session, Key::WorkItem] {
            let (new, legacy) = key.env_names().unwrap();
            assert_eq!(env_with(key, lookup(&[(new, "n")])).as_deref(), Some("n"));
            assert_eq!(
                env_with(key, lookup(&[(legacy, "o")])).as_deref(),
                Some("o")
            );
            let both = lookup(&[(new, "n"), (legacy, "o")]);
            assert_eq!(env_with(key, both).as_deref(), Some("n"));
            assert_eq!(env_with(key, lookup(&[])), None);
            // An empty new variable does not hide a set legacy one.
            let empty = lookup(&[(new, ""), (legacy, "o")]);
            assert_eq!(env_with(key, empty).as_deref(), Some("o"));
        }
    }

    #[test]
    fn env_fields_without_variables_are_none() {
        let all = lookup(&[(SESSION_ENV, "s"), (WORK_ITEM_ENV, "w")]);
        assert_eq!(env_with(Key::Model, &all), None);
        assert_eq!(env_with(Key::Coauthors, &all), None);
    }
}

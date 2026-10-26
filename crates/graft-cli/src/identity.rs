//! Who is working: the session and work item from `GRAFT_SESSION_ID` and
//! `GRAFT_WORK_ITEM` (the legacy names are a fallback), and the default
//! author. Drafts carry the session and work item as `agent.session` and
//! `agent.work_item` metadata, which the land keeps and `pr-batch` and
//! the exporter read.

use std::env;

use graft_model::{Author, AuthorKind, Metadata};
use graft_proto::attribution::{self, Key};

/// Metadata key for the session.
pub(crate) const SESSION_KEY: &str = attribution::SESSION_KEY;
/// Metadata key for the work item.
pub(crate) const WORK_ITEM_KEY: &str = attribution::WORK_ITEM_KEY;

/// The session (`GRAFT_SESSION_ID`, else the legacy name), if set.
pub(crate) fn session() -> Option<String> {
    attribution::env(Key::Session)
}

/// The work item (`GRAFT_WORK_ITEM`, else the legacy name), if set.
pub(crate) fn work_item() -> Option<String> {
    attribution::env(Key::WorkItem)
}

/// Adds the session and work item to `meta`, unless `--meta` already set
/// them. Writes only the new keys.
pub(crate) fn add_session_metadata(meta: &mut Metadata) {
    for (key, value) in [(SESSION_KEY, session()), (WORK_ITEM_KEY, work_item())] {
        if let Some(value) = value {
            meta.entry(key.to_owned()).or_insert(value);
        }
    }
}

/// The default author kind: `GRAFT_AUTHOR_KIND`, else `agent` when a
/// session id is present, else `fallback`.
pub(crate) fn default_kind_text(fallback: &str) -> String {
    env_trimmed("GRAFT_AUTHOR_KIND").unwrap_or_else(|| {
        if session().is_some() {
            "agent".to_owned()
        } else {
            fallback.to_owned()
        }
    })
}

fn env_trimmed(name: &str) -> Option<String> {
    let v = env::var(name).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_owned())
}

/// Parses an author kind.
pub(crate) fn parse_kind(text: &str) -> Option<AuthorKind> {
    match text {
        "human" => Some(AuthorKind::Human),
        "agent" => Some(AuthorKind::Agent),
        "bot" => Some(AuthorKind::Bot),
        "ci" => Some(AuthorKind::Ci),
        _ => None,
    }
}

/// The default author name and email: `GRAFT_AUTHOR_NAME` and
/// `GRAFT_AUTHOR_EMAIL`, then `$USER` or `$LOGNAME` for the name. With no
/// email the email is empty, so `pr-batch` gives the land the mirror
/// identity (WI-2412); it is never made up.
pub(crate) fn default_name_email() -> (String, String) {
    pick_name_email(env_trimmed)
}

fn pick_name_email(env: impl Fn(&str) -> Option<String>) -> (String, String) {
    let name = env("GRAFT_AUTHOR_NAME")
        .or_else(|| env("USER"))
        .or_else(|| env("LOGNAME"))
        .unwrap_or_else(|| "graft".to_owned());
    let email = env("GRAFT_AUTHOR_EMAIL").unwrap_or_default();
    (name, email)
}

/// The default author, with `kind`.
pub(crate) fn default_author(kind: AuthorKind) -> Author {
    let (name, email) = default_name_email();
    Author { name, email, kind }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_proto::attribution::{
        LEGACY_SESSION_ENV, LEGACY_WORK_ITEM_ENV, SESSION_ENV, WORK_ITEM_ENV,
    };

    #[test]
    fn kinds_parse() {
        assert_eq!(parse_kind("agent"), Some(AuthorKind::Agent));
        assert_eq!(parse_kind("human"), Some(AuthorKind::Human));
        assert_eq!(parse_kind("robot"), None);
    }

    #[test]
    fn explicit_meta_wins_over_the_environment() {
        let mut m = Metadata::new();
        m.insert(SESSION_KEY.to_owned(), "ses_given".to_owned());
        add_session_metadata(&mut m);
        assert_eq!(m[SESSION_KEY], "ses_given");
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn author_defaults_go_graft_then_user() {
        let env = [
            ("GRAFT_AUTHOR_NAME", "Graft Name"),
            ("GRAFT_AUTHOR_EMAIL", "graft@example.com"),
            ("USER", "u"),
        ];
        assert_eq!(
            pick_name_email(lookup(&env)),
            ("Graft Name".into(), "graft@example.com".into())
        );
        assert_eq!(
            pick_name_email(lookup(&[("USER", "u")])),
            ("u".into(), String::new())
        );
        assert_eq!(
            pick_name_email(lookup(&[("LOGNAME", "l")])),
            ("l".into(), String::new())
        );
        let (name, email) = pick_name_email(lookup(&[]));
        assert_eq!(name, "graft");
        assert!(email.is_empty(), "never @localhost");
    }

    fn attribution(pairs: &[(&str, &str)]) -> (Option<String>, Option<String>) {
        let lookup = lookup(pairs);
        (
            attribution::env_with(Key::Session, &lookup),
            attribution::env_with(Key::WorkItem, &lookup),
        )
    }

    #[test]
    fn new_env_names_win() {
        assert_eq!(
            attribution(&[
                (SESSION_ENV, "s-new"),
                (WORK_ITEM_ENV, "w-new"),
                (LEGACY_SESSION_ENV, "s-old"),
                (LEGACY_WORK_ITEM_ENV, "w-old"),
            ]),
            (Some("s-new".into()), Some("w-new".into()))
        );
    }

    #[test]
    fn legacy_env_names_are_the_fallback() {
        assert_eq!(
            attribution(&[
                (LEGACY_SESSION_ENV, "s-old"),
                (LEGACY_WORK_ITEM_ENV, "w-old")
            ]),
            (Some("s-old".into()), Some("w-old".into()))
        );
    }

    #[test]
    fn neither_env_is_unset() {
        assert_eq!(attribution(&[]), (None, None));
    }

    #[test]
    fn written_keys_are_the_new_ones() {
        assert_eq!(SESSION_KEY, "agent.session");
        assert_eq!(WORK_ITEM_KEY, "agent.work_item");
    }
}

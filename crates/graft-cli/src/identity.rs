//! Who is working: the Vogt session and work item from `VOGT_SESSION_ID`
//! and `VOGT_WORK_ITEM`, and the default author. Drafts carry the session
//! and work item as `vogt.session` and `vogt.work_item` metadata, which the
//! land keeps and `pr-batch` and the exporter read.

use std::env;

use graft_model::{Author, AuthorKind, Metadata};

/// Metadata key for the Vogt session.
pub(crate) const SESSION_KEY: &str = "vogt.session";
/// Metadata key for the Vogt work item.
pub(crate) const WORK_ITEM_KEY: &str = "vogt.work_item";

/// A non-empty, trimmed environment variable.
fn var(name: &str) -> Option<String> {
    let v = env::var(name).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_owned())
}

/// The Vogt session (`VOGT_SESSION_ID`), if set.
pub(crate) fn session() -> Option<String> {
    var("VOGT_SESSION_ID")
}

/// The Vogt work item (`VOGT_WORK_ITEM`), if set.
pub(crate) fn work_item() -> Option<String> {
    var("VOGT_WORK_ITEM")
}

/// Adds the session and work item to `meta`, unless `--meta` already set
/// them.
pub(crate) fn add_vogt_metadata(meta: &mut Metadata) {
    for (key, value) in [(SESSION_KEY, session()), (WORK_ITEM_KEY, work_item())] {
        if let Some(value) = value {
            meta.entry(key.to_owned()).or_insert(value);
        }
    }
}

/// The default author kind: `GRAFT_AUTHOR_KIND`, else `agent` inside a
/// Vogt session, else `fallback`.
pub(crate) fn default_kind_text(fallback: &str) -> String {
    var("GRAFT_AUTHOR_KIND").unwrap_or_else(|| {
        if session().is_some() {
            "agent".to_owned()
        } else {
            fallback.to_owned()
        }
    })
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
/// `GRAFT_AUTHOR_EMAIL`, else the user name.
pub(crate) fn default_name_email() -> (String, String) {
    let user = var("USER").unwrap_or_else(|| "graft".to_owned());
    let name = var("GRAFT_AUTHOR_NAME").unwrap_or_else(|| user.clone());
    let email = var("GRAFT_AUTHOR_EMAIL").unwrap_or_else(|| format!("{user}@localhost"));
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
        add_vogt_metadata(&mut m);
        assert_eq!(m[SESSION_KEY], "ses_given");
    }
}

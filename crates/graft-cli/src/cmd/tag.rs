//! `graft tag`: creates or lists immutable tags (`docs/design.md` §4.6).
//! `create` is an `UpdateRef` of kind `Tag` by compare-and-swap from absent,
//! so it never moves or overwrites a ref: an existing tag at another target
//! is a `ref_conflict`, and a retry at the same target answers the tag as it
//! is. Tags live under `tags/`; creating one needs `repo:<r>:write`.

use graft_proto::api::RepoApi;
use graft_proto::types::{ListRefs, Page, RefKind, UpdateRef};
use serde_json::json;

use crate::cli::{Global, TagAction, TagArgs};
use crate::conn::{call_context, connect, runtime, working_copy};
use crate::error::Failure;
use crate::render::{print_json, ref_view, short};

/// The ref namespace tags live in.
const TAG_PREFIX: &str = "tags/";

/// `name` as a ref name under `tags/`.
fn tag_ref(name: &str) -> String {
    if name.starts_with(TAG_PREFIX) {
        name.to_owned()
    } else {
        format!("{TAG_PREFIX}{name}")
    }
}

/// Runs a tag action.
pub(crate) fn run(g: &Global, args: &TagArgs) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        match &args.action {
            TagAction::Create { name, at } => {
                let at = conn.resolve(&cx, at).await?;
                let view = conn
                    .remote
                    .update_ref(
                        &cx,
                        UpdateRef {
                            repo: conn.repo.clone(),
                            name: tag_ref(name),
                            expected: None,
                            new: at,
                            kind: RefKind::Tag,
                        },
                    )
                    .await?;
                if g.json {
                    return print_json(&ref_view(&view));
                }
                outln!("created tag {} at {}", view.name, short(view.target));
                Ok(())
            }
            TagAction::List {
                prefix,
                limit,
                cursor,
            } => {
                let refs = conn
                    .remote
                    .list_refs(
                        &cx,
                        ListRefs {
                            repo: conn.repo.clone(),
                            prefix: Some(tag_ref(prefix.as_deref().unwrap_or(""))),
                            page: Page {
                                limit: *limit,
                                token: cursor.clone(),
                            },
                        },
                    )
                    .await?;
                // A full page may have more after it; the last ref name
                // resumes, whatever its kind.
                let next = (refs.len() as u32 >= *limit && *limit > 0)
                    .then(|| refs.last().map(|r| r.name.clone()))
                    .flatten();
                let tags: Vec<_> = refs.iter().filter(|r| r.kind == RefKind::Tag).collect();
                if g.json {
                    return print_json(&json!({
                        "tags": tags.iter().map(|r| ref_view(r)).collect::<Vec<_>>(),
                        "next": next,
                    }));
                }
                for r in &tags {
                    outln!("{:<24} {}", r.name, short(r.target));
                }
                if let Some(next) = next {
                    outln!("more: --cursor {next}");
                }
                Ok(())
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::tag_ref;

    #[test]
    fn tag_names_land_under_tags() {
        assert_eq!(tag_ref("v1.0"), "tags/v1.0");
        assert_eq!(tag_ref("tags/v1.0"), "tags/v1.0");
        assert_eq!(tag_ref(""), "tags/");
    }
}

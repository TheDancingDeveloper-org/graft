//! `Revert` (`docs/design.md` §7.2, §8.3; decision D6's auto-revert).
//!
//! A revert inverts a changeset `c` against its first parent: for each path
//! `c` changes, it puts back the parent's entry, or deletes the path when
//! the parent had none there (a directory counts as none, as in a rebase;
//! the files inside it are their own changes of `c`). The inverse sits on
//! `c`, whose root is the parent's, and is then rebased onto `onto` with
//! line merge (decision D16), so it conflicts only where later changes
//! touch the same lines. A revert onto `c` itself needs no rebase.
//!
//! The result is deterministic, so a retried revert returns the same id: its
//! author is the caller, its time is `c`'s (no server time, design rule 5),
//! its message names `c`, and its metadata is `graft.reverts = c`, plus
//! `graft.merged-paths` when the rebase merged. With `workspace`, it goes on
//! top of that workspace's stack, which must sit at `onto`; without one it
//! is stored abandoned, like a rebase's output, until a land or a workspace
//! takes it.
//!
//! Cost: one tree lookup per path `c` changes, plus the rebase.

use graft_model::{Author, AuthorKind, Change, Changeset, ChangesetId};
use graft_proto::types::{MergeMode, Revert};
use graft_proto::{ApiError, CallContext, PrincipalKind};
use graft_store::{MetaStore, ObjectStore};

use crate::access::{Need, require, require_changes};
use crate::rebase::{REBASED_FROM, key};
use crate::source::Trees;
use crate::{Engine, errors};

/// The metadata key naming the changeset a revert inverts.
pub const REVERTS: &str = "graft.reverts";

/// The author a revert records for its caller.
fn caller(cx: &CallContext) -> Author {
    Author {
        name: cx.principal.subject.clone(),
        email: String::new(),
        kind: match cx.principal.kind {
            PrincipalKind::Human => AuthorKind::Human,
            PrincipalKind::Agent => AuthorKind::Agent,
            PrincipalKind::Ci => AuthorKind::Ci,
            PrincipalKind::Service => AuthorKind::Bot,
        },
    }
}

/// The message of the revert of `id`, whose message is `message`.
fn message(id: ChangesetId, message: &str) -> String {
    let subject = message.lines().next().unwrap_or_default();
    format!("Revert \"{subject}\"\n\nThis reverts changeset {id}.\n")
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_revert(
        &self,
        cx: &CallContext,
        req: Revert,
    ) -> Result<ChangesetId, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        if req.workspace.is_some() {
            require(cx, &req.repo, Need::Workspace)?;
        }
        let repo = self.repo_id(&req.repo).await?;
        let (c, _) = self.changeset(repo, req.changeset).await?;
        require_changes(cx, &req.repo, &c.changes)?;
        let parent = c
            .first_parent()
            .ok_or_else(|| errors::invalid(None, "revert-root"))?;
        if c.changes.is_empty() {
            return Err(errors::invalid(None, "revert-empty"));
        }
        // The target exists and is not collected.
        self.changeset(repo, req.onto).await?;
        let parent_root = self.root_of(repo, parent).await?;

        let trees = Trees::new(&*self.objects, repo);
        let mut changes = Vec::with_capacity(c.changes.len());
        for change in &c.changes {
            let path = change.path();
            let entry = trees
                .lookup(parent_root, path)
                .await
                .map_err(errors::source)?;
            changes.push(match key(&entry) {
                Some((kind, id)) => Change::Put {
                    path: path.clone(),
                    kind,
                    id,
                },
                None => Change::Delete { path: path.clone() },
            });
        }
        let mut metadata = graft_model::Metadata::new();
        metadata.insert(REVERTS.to_owned(), req.changeset.to_string());
        let inverse = Changeset {
            parents: vec![req.changeset],
            root: parent_root,
            changes,
            author: caller(cx),
            authored_at: c.authored_at,
            message: message(req.changeset, &c.message),
            metadata,
        };

        let (result, uploads) = if req.onto == req.changeset {
            (inverse, Vec::new())
        } else {
            let id = inverse.id();
            let plan = self
                .plan(repo, &[(id, inverse)], req.onto, MergeMode::Line)
                .await?;
            let Some(mut cs) = plan.new.first().cloned() else {
                // Every change is already undone on the target.
                return Err(errors::invalid(None, "revert-empty"));
            };
            let uploads = plan.uploads(&cs);
            // The inverse on `c` is never stored: nothing to point back to.
            cs.metadata.remove(REBASED_FROM);
            (cs, uploads)
        };
        let created = match &req.workspace {
            Some(name) => {
                self.insert_held(cx, &req.repo, repo, name, result, uploads)
                    .await?
            }
            None => self.insert_unheld(repo, result, uploads).await?,
        };
        Ok(created.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::ObjectId;

    #[test]
    fn messages_name_the_reverted_changeset() {
        let id = ChangesetId(ObjectId([7; 32]));
        let text = message(id, "Fix the parser\n\nLonger body.");
        assert_eq!(
            text,
            format!("Revert \"Fix the parser\"\n\nThis reverts changeset {id}.\n")
        );
        assert!(message(id, "").starts_with("Revert \"\""));
    }
}

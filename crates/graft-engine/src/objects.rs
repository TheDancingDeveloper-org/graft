//! `GetObjects`, `HasObjects` and `PutObjects` (`docs/design.md` §7.2,
//! §6.6 step 1).

use std::collections::HashSet;

use graft_model::{ObjectId, ObjectKind};
use graft_proto::types::{GetObjects, HasObjects, Objects, PutObjects};
use graft_proto::{ApiError, CallContext, Frame};
use graft_store::{MetaStore, MetaTxn, ObjectStore, RepoId, Write};

use crate::access::{Need, require};
use crate::{Engine, errors};

/// The holder an upload is recorded under until a changeset holds the
/// object: `upload:<expiry ms>:<uploader>`. The GC releases it after the
/// expiry.
pub(crate) fn upload_holder(expires_ms: i64, subject: &str) -> String {
    format!("upload:{expires_ms}:{subject}")
}

/// A blob frame checked for upload: its id and the bytes to store.
pub(crate) struct Upload {
    pub(crate) id: ObjectId,
    pub(crate) bytes: Vec<u8>,
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// Checks one uploaded frame: a blob, within the size limit, whose body
    /// decodes to the length it declares. Its id is the hash of the payload.
    pub(crate) fn check_blob(&self, frame: &Frame) -> Result<Upload, ApiError> {
        if frame.kind != ObjectKind::Blob {
            return Err(errors::invalid(None, "blob-only"));
        }
        if frame.uncompressed_len > self.config.max_blob_bytes {
            return Err(errors::limit("blob-bytes", self.config.max_blob_bytes));
        }
        let payload = frame.decode().map_err(|_| errors::invalid(None, "frame"))?;
        Ok(Upload {
            id: ObjectId::hash(ObjectKind::Blob, &payload),
            bytes: frame.to_bytes(),
        })
    }

    /// Stores uploads (put-if-absent, durable on return).
    pub(crate) async fn store_uploads(
        &self,
        repo: RepoId,
        uploads: Vec<Upload>,
    ) -> Result<(), ApiError> {
        for u in uploads {
            self.objects
                .put(repo, u.id, u.bytes)
                .await
                .map_err(errors::store)?;
        }
        Ok(())
    }

    pub(crate) async fn do_put_objects(
        &self,
        cx: &CallContext,
        req: PutObjects,
    ) -> Result<Vec<ObjectId>, ApiError> {
        require(cx, &req.repo, Need::Write(None))?;
        let repo = self.repo_id(&req.repo).await?;
        if req.blobs.len() > self.config.max_ids as usize {
            return Err(errors::limit("ids", self.config.max_ids.into()));
        }
        let total: u64 = req.blobs.iter().map(|f| f.body.len() as u64).sum();
        if total > self.config.max_put_bytes {
            return Err(errors::limit("put-bytes", self.config.max_put_bytes));
        }
        let uploads = req
            .blobs
            .iter()
            .map(|f| self.check_blob(f))
            .collect::<Result<Vec<_>, _>>()?;
        let ids: Vec<ObjectId> = uploads.iter().map(|u| u.id).collect();
        self.store_uploads(repo, uploads).await?;

        // Record the upload lease. No journal entry: an upload changes no
        // history, and the GC reads holders, not the journal.
        let lease = self.clock.now().ms + self.config.upload_lease_ms as i64;
        let holder = upload_holder(lease, &cx.principal.subject);
        let mut seen = HashSet::new();
        let writes = ids
            .iter()
            .filter(|id| seen.insert(**id))
            .map(|&object| Write::AddHolder {
                object,
                holder: holder.clone(),
            })
            .collect();
        self.meta
            .commit(
                repo,
                MetaTxn {
                    preconditions: Vec::new(),
                    writes,
                },
            )
            .await
            .map_err(errors::store)?;
        Ok(ids)
    }

    pub(crate) async fn do_has_objects(
        &self,
        cx: &CallContext,
        req: HasObjects,
    ) -> Result<Vec<ObjectId>, ApiError> {
        require(cx, &req.repo, Need::Objects)?;
        let repo = self.repo_id(&req.repo).await?;
        if req.ids.len() > self.config.max_ids as usize {
            return Err(errors::limit("ids", self.config.max_ids.into()));
        }
        let present: HashSet<ObjectId> = self
            .objects
            .has(repo, &req.ids)
            .await
            .map_err(errors::store)?
            .into_iter()
            .collect();
        Ok(req
            .ids
            .into_iter()
            .filter(|id| !present.contains(id))
            .collect())
    }

    /// Returns the stored frames, verified. Ids past the count limit, and
    /// those after the response reaches its byte budget, are `deferred`.
    pub(crate) async fn do_get_objects(
        &self,
        cx: &CallContext,
        req: GetObjects,
    ) -> Result<Objects, ApiError> {
        require(cx, &req.repo, Need::Objects)?;
        let repo = self.repo_id(&req.repo).await?;
        let mut seen = HashSet::new();
        let mut ids = req.ids.into_iter().filter(|id| seen.insert(*id));
        let mut out = Objects::default();
        let mut bytes = 0u64;
        for id in ids.by_ref().take(self.config.max_ids as usize) {
            if bytes >= self.config.max_get_bytes {
                out.deferred.push(id);
                continue;
            }
            let Some(raw) = self.objects.get(repo, id).await.map_err(errors::store)? else {
                out.missing.push(id);
                continue;
            };
            let frame = Frame::from_bytes(&raw).map_err(|_| ApiError::CorruptObject { id })?;
            if frame.kind == ObjectKind::Changeset || frame.decode_verified(id).is_err() {
                return Err(ApiError::CorruptObject { id });
            }
            if !out.found.is_empty() && bytes + raw.len() as u64 > self.config.max_get_bytes {
                out.deferred.push(id);
                bytes = self.config.max_get_bytes;
                continue;
            }
            bytes += raw.len() as u64;
            out.found.push((id, frame));
        }
        out.deferred.extend(ids);
        Ok(out)
    }
}

//! The server calls the mirror service makes (D41 §3.3–§3.6): list the
//! mirror rows, hold each repository's authority lease, read a row's
//! credential, write status, and claim and finish mirror requests. Every
//! fenced call carries the lease epoch. [`Server`] speaks them over gRPC;
//! tests use an in-process fake.

use std::future::Future;

use graft_proto::error::ApiResult;
use graft_proto::pb;
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind};

use crate::credentials::SecretToken;
use crate::server::{Server, status_error};

/// The most rows one `ListMirrors` page asks for.
pub const LIST_PAGE: u32 = 200;

/// The mirror service's view of the graft server.
pub trait Control: Send + Sync + 'static {
    /// One page of mirror rows, `enabled` only when given.
    fn list_mirrors(
        &self,
        enabled: Option<bool>,
        page_token: String,
    ) -> impl Future<Output = ApiResult<pb::MirrorPage>> + Send;
    /// Takes `repo`'s lease if it is free or lapsed.
    fn acquire_lease(
        &self,
        repo: &str,
        instance: &str,
    ) -> impl Future<Output = ApiResult<pb::Lease>> + Send;
    /// Extends the lease held under `epoch`.
    fn renew_lease(
        &self,
        repo: &str,
        epoch: u64,
    ) -> impl Future<Output = ApiResult<pb::Lease>> + Send;
    /// Frees the lease held under `epoch`.
    fn release_lease(&self, repo: &str, epoch: u64) -> impl Future<Output = ApiResult<()>> + Send;
    /// The row's credential, if one is set. Answered only under the
    /// current epoch.
    fn credential(
        &self,
        repo: &str,
        upstream: &str,
        epoch: u64,
    ) -> impl Future<Output = ApiResult<Option<SecretToken>>> + Send;
    /// Writes the row's status; it carries its epoch.
    fn set_status(
        &self,
        repo: &str,
        upstream: &str,
        status: pb::MirrorStatus,
    ) -> impl Future<Output = ApiResult<()>> + Send;
    /// Claims the oldest claimable request on `repo`, if any.
    fn claim_request(
        &self,
        repo: &str,
        epoch: u64,
    ) -> impl Future<Output = ApiResult<Option<pb::MirrorRequest>>> + Send;
    /// Records (`done = false`) or finishes a claimed request.
    fn complete_request(
        &self,
        repo: &str,
        id: u64,
        epoch: u64,
        result: pb::MirrorRequestOutcome,
        done: bool,
    ) -> impl Future<Output = ApiResult<()>> + Send;
    /// Fails a claimed request with a typed code.
    fn fail_request(
        &self,
        repo: &str,
        id: u64,
        epoch: u64,
        error_code: &str,
    ) -> impl Future<Output = ApiResult<()>> + Send;
}

/// The call context of the service's own calls. The server reads the
/// principal from the bearer token, not from this.
pub(crate) fn service_cx(what: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-mirror".to_owned(),
            kind: PrincipalKind::Mirror,
            session: None,
            work_item: None,
            scopes: vec!["repo:*:mirror".to_owned()],
        },
        request_id: format!("mirror-serve-{what}"),
        deadline_ms: None,
    }
}

impl Control for Server {
    async fn list_mirrors(
        &self,
        enabled: Option<bool>,
        page_token: String,
    ) -> ApiResult<pb::MirrorPage> {
        let req = self.request(
            &service_cx("list-mirrors"),
            pb::ListMirrorsRequest {
                repo: String::new(),
                enabled,
                limit: LIST_PAGE,
                page_token,
            },
        )?;
        let resp = self
            .mirror()
            .list_mirrors(req)
            .await
            .map_err(status_error)?;
        Ok(resp.into_inner())
    }

    async fn acquire_lease(&self, repo: &str, instance: &str) -> ApiResult<pb::Lease> {
        let req = self.request(
            &service_cx("acquire-lease"),
            pb::AcquireLeaseRequest {
                repo: repo.to_owned(),
                instance: instance.to_owned(),
            },
        )?;
        let resp = self
            .mirror()
            .acquire_lease(req)
            .await
            .map_err(status_error)?;
        Ok(resp.into_inner())
    }

    async fn renew_lease(&self, repo: &str, epoch: u64) -> ApiResult<pb::Lease> {
        let req = self.request(
            &service_cx("renew-lease"),
            pb::LeaseRequest {
                repo: repo.to_owned(),
                epoch,
            },
        )?;
        let resp = self.mirror().renew_lease(req).await.map_err(status_error)?;
        Ok(resp.into_inner())
    }

    async fn release_lease(&self, repo: &str, epoch: u64) -> ApiResult<()> {
        let req = self.request(
            &service_cx("release-lease"),
            pb::LeaseRequest {
                repo: repo.to_owned(),
                epoch,
            },
        )?;
        self.mirror()
            .release_lease(req)
            .await
            .map_err(status_error)?;
        Ok(())
    }

    async fn credential(
        &self,
        repo: &str,
        upstream: &str,
        epoch: u64,
    ) -> ApiResult<Option<SecretToken>> {
        let req = self.request(
            &service_cx("get-credential"),
            pb::GetMirrorCredentialRequest {
                repo: repo.to_owned(),
                upstream: upstream.to_owned(),
                epoch,
            },
        )?;
        match self.mirror().get_mirror_credential(req).await {
            Ok(resp) => {
                let bytes = resp.into_inner().credential;
                // Not UTF-8 is not a usable token; never echo it.
                let text = String::from_utf8(bytes).map_err(|_| ApiError::Rejected {
                    policy: "mirror-credential".to_owned(),
                    detail: "the credential is not text".to_owned(),
                })?;
                Ok(SecretToken::new(text.trim().to_owned()))
            }
            Err(s) => match status_error(s) {
                ApiError::NotFound { .. } => Ok(None),
                e => Err(e),
            },
        }
    }

    async fn set_status(
        &self,
        repo: &str,
        upstream: &str,
        status: pb::MirrorStatus,
    ) -> ApiResult<()> {
        let req = self.request(
            &service_cx("set-status"),
            pb::SetMirrorStatusRequest {
                repo: repo.to_owned(),
                upstream: upstream.to_owned(),
                status: Some(status),
            },
        )?;
        self.mirror()
            .set_mirror_status(req)
            .await
            .map_err(status_error)?;
        Ok(())
    }

    async fn claim_request(&self, repo: &str, epoch: u64) -> ApiResult<Option<pb::MirrorRequest>> {
        let req = self.request(
            &service_cx("claim-request"),
            pb::ClaimRequestRequest {
                repo: repo.to_owned(),
                epoch,
            },
        )?;
        let resp = self
            .mirror()
            .claim_request(req)
            .await
            .map_err(status_error)?;
        Ok(resp.into_inner().request)
    }

    async fn complete_request(
        &self,
        repo: &str,
        id: u64,
        epoch: u64,
        result: pb::MirrorRequestOutcome,
        done: bool,
    ) -> ApiResult<()> {
        let req = self.request(
            &service_cx("complete-request"),
            pb::CompleteRequestRequest {
                repo: repo.to_owned(),
                id,
                epoch,
                result: Some(result),
                done,
            },
        )?;
        self.mirror()
            .complete_request(req)
            .await
            .map_err(status_error)?;
        Ok(())
    }

    async fn fail_request(
        &self,
        repo: &str,
        id: u64,
        epoch: u64,
        error_code: &str,
    ) -> ApiResult<()> {
        let req = self.request(
            &service_cx("fail-request"),
            pb::FailRequestRequest {
                repo: repo.to_owned(),
                id,
                epoch,
                error_code: error_code.to_owned(),
            },
        )?;
        self.mirror()
            .fail_request(req)
            .await
            .map_err(status_error)?;
        Ok(())
    }
}

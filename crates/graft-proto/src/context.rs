//! Per-call context: who is calling, and for which request.

/// What kind of principal is calling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalKind {
    /// A person.
    Human,
    /// An agent session.
    Agent,
    /// A CI job.
    Ci,
    /// A graft service (node daemon, attester).
    Service,
    /// The mirror service (D41). It may hold only `repo:*:mirror`, and is
    /// the one principal that moves a followed line.
    Mirror,
}

/// An authenticated caller, as established by `graft-auth` from a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// Subject (user, session or service name).
    pub subject: String,
    /// Kind of principal.
    pub kind: PrincipalKind,
    /// Session id, set by the caller's orchestrator, if any.
    pub session: Option<String>,
    /// Work item, set by the caller's orchestrator, if any.
    pub work_item: Option<String>,
    /// Granted scopes, in the text form of design §12.1.
    pub scopes: Vec<String>,
}

/// Passed to every API call.
#[derive(Clone, Debug)]
pub struct CallContext {
    /// The caller.
    pub principal: Principal,
    /// Client-generated request id, for logs and idempotency.
    pub request_id: String,
    /// Deadline in milliseconds since the epoch, if any.
    pub deadline_ms: Option<i64>,
}

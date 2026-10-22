//! Harness: a counting fake upstream over TCP, a node in front of it on a
//! socket in a temp directory, tokens, and clients.

#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::Request as HttpRequest;
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use graft_auth::{Claims, SigningKey};
use graft_client::{Endpoint, Remote};
use graft_model::{ObjectId, ObjectKind};
use graft_node::{Node, NodeConfig, NodeMetrics};
use graft_proto::pb;
use graft_proto::{CallContext, Frame, PROTOCOL_VERSION, Principal, PrincipalKind, VERSION_HEADER};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, Streaming};

/// The status the fake answers every forwarded call with.
pub const FAKE_MESSAGE: &str = "fake-upstream";

/// The key the fake server signs with.
pub fn signing_key() -> SigningKey {
    SigningKey::from_seed(&[7u8; 32])
}

/// A token with `scopes`, valid for an hour.
pub fn token(scopes: &[&str]) -> String {
    token_until(scopes, now_s() + 3600)
}

/// A token with `scopes` and expiry `exp`.
pub fn token_until(scopes: &[&str], exp: i64) -> String {
    let claims = Claims {
        principal: Principal {
            subject: "tester".into(),
            kind: PrincipalKind::Agent,
            session: Some("ses_test".into()),
            work_item: None,
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        },
        exp,
        jti: "jti-test".into(),
    };
    graft_auth::mint(&signing_key(), &claims)
        .expect("mint")
        .expose()
        .to_owned()
}

pub fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A blob frame and its id.
pub fn blob(i: usize) -> (ObjectId, Vec<u8>) {
    let payload = format!("blob number {i}\n").repeat(1 + i % 7).into_bytes();
    let id = ObjectId::hash(ObjectKind::Blob, &payload);
    (id, Frame::encode(ObjectKind::Blob, &payload).to_bytes())
}

/// What the fake upstream holds and has seen.
#[derive(Default)]
pub struct FakeState {
    /// Frames served, by `(repo, id)`; they may be corrupt on purpose.
    pub objects: HashMap<(String, ObjectId), Vec<u8>>,
    /// Times each `(repo, id)` was asked for.
    pub asked: HashMap<(String, ObjectId), usize>,
    /// Every call: its path and authorization header.
    pub calls: Vec<(String, Option<String>)>,
    /// Frames sent with every answer whether asked for or not.
    pub extra: Vec<(Vec<u8>, Vec<u8>)>,
    /// How long `GetObjects` takes.
    pub delay: Duration,
}

pub type Fake = Arc<Mutex<FakeState>>;

#[derive(Clone)]
struct FakeObjects(Fake);

#[tonic::async_trait]
impl pb::object_service_server::ObjectService for FakeObjects {
    type GetObjectsStream = tokio_stream::Iter<std::vec::IntoIter<Result<pb::Objects, Status>>>;

    async fn get_objects(
        &self,
        request: Request<pb::GetObjectsRequest>,
    ) -> Result<Response<Self::GetObjectsStream>, Status> {
        let req = request.into_inner();
        let delay = self.0.lock().unwrap().delay;
        tokio::time::sleep(delay).await;
        let mut page = pb::Objects::default();
        let mut state = self.0.lock().unwrap();
        for raw in req.ids {
            let id = ObjectId(raw.as_slice().try_into().expect("32-byte id"));
            let key = (req.repo.clone(), id);
            *state.asked.entry(key.clone()).or_default() += 1;
            match state.objects.get(&key) {
                Some(frame) => page.found.push(pb::FoundObject {
                    id: raw,
                    frame: frame.clone(),
                }),
                None => page.missing.push(raw),
            }
        }
        for (id, frame) in &state.extra {
            page.found.push(pb::FoundObject {
                id: id.clone(),
                frame: frame.clone(),
            });
        }
        Ok(Response::new(tokio_stream::iter(vec![Ok(page)])))
    }

    async fn has_objects(
        &self,
        _r: Request<pb::HasObjectsRequest>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        unreachable!("not routed")
    }

    async fn put_objects(
        &self,
        _r: Request<Streaming<pb::PutObjectsRequest>>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        unreachable!("not routed")
    }
}

/// A running fake upstream.
pub struct Upstream {
    pub addr: SocketAddr,
    pub state: Fake,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Upstream {
    pub async fn start() -> Self {
        Self::start_on("127.0.0.1:0", Fake::default()).await
    }

    /// Starts a fake whose public address is `addr`. Clients reach it
    /// through a relay that [`Upstream::stop`] cuts, connections and all, so
    /// a stopped upstream is really gone.
    pub async fn start_on(addr: &str, state: Fake) -> Self {
        let inner = TcpListener::bind("127.0.0.1:0").await.expect("bind fake");
        let inner_addr = inner.local_addr().unwrap();
        let public = TcpListener::bind(addr).await.expect("bind relay");
        let addr = public.local_addr().unwrap();
        let record = state.clone();
        let router = Router::new()
            .route_service(
                "/graft.v1.ObjectService/GetObjects",
                pb::object_service_server::ObjectServiceServer::new(FakeObjects(state.clone())),
            )
            .fallback(|| async {
                Status::failed_precondition(FAKE_MESSAGE).into_http::<axum::body::Body>()
            })
            .layer(middleware::from_fn(move |req: HttpRequest, next: Next| {
                let record = record.clone();
                async move {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    record
                        .lock()
                        .unwrap()
                        .calls
                        .push((req.uri().path().to_owned(), auth));
                    let mut resp = next.run(req).await;
                    resp.headers_mut().insert(
                        VERSION_HEADER,
                        HeaderValue::try_from(PROTOCOL_VERSION.to_string()).unwrap(),
                    );
                    resp
                }
            }));
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let mut relays = tokio::task::JoinSet::new();
            let server = tonic::transport::Server::builder()
                .add_routes(tonic::service::Routes::from(router))
                .serve_with_incoming(TcpListenerStream::new(inner));
            tokio::pin!(server);
            let mut rx = rx;
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    _ = &mut server => break,
                    accepted = public.accept() => {
                        let Ok((mut client, _)) = accepted else { continue };
                        relays.spawn(async move {
                            if let Ok(mut upstream) = tokio::net::TcpStream::connect(inner_addr).await {
                                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                            }
                        });
                    }
                }
            }
            // Dropping the relays closes every connection.
            relays.shutdown().await;
        });
        Self {
            addr,
            state,
            stop: Some(tx),
            task: Some(task),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn put(&self, repo: &str, id: ObjectId, frame: Vec<u8>) {
        self.state
            .lock()
            .unwrap()
            .objects
            .insert((repo.to_owned(), id), frame);
    }

    pub fn asked(&self, repo: &str, id: ObjectId) -> usize {
        self.state
            .lock()
            .unwrap()
            .asked
            .get(&(repo.to_owned(), id))
            .copied()
            .unwrap_or(0)
    }

    pub fn calls(&self) -> Vec<(String, Option<String>)> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn set_delay(&self, d: Duration) {
        self.state.lock().unwrap().delay = d;
    }

    /// Stops serving and closes the listener.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// A running node in a temp directory.
pub struct NodeRun {
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub node: Node,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), String>>>,
}

pub fn config(dir: &Path, upstream: &str) -> NodeConfig {
    let key = signing_key().verifying_key().to_bytes();
    NodeConfig {
        socket: dir.join("node.sock"),
        socket_mode: "0660".into(),
        cache_dir: dir.join("cache"),
        min_free_bytes: 0,
        upstream: upstream.to_owned(),
        server_public_key: key.iter().map(|b| format!("{b:02x}")).collect(),
        json_logs: false,
        ..NodeConfig::default()
    }
}

impl NodeRun {
    pub async fn start(config: NodeConfig) -> Self {
        let listener =
            graft_node::socket::bind(&config.socket, config.mode().unwrap()).expect("bind socket");
        let node = Node::open(&config, NodeMetrics::new().unwrap()).expect("open node");
        let (tx, rx) = oneshot::channel::<()>();
        let serving = node.clone();
        let task = tokio::spawn(async move {
            serving
                .serve(listener, async {
                    let _ = rx.await;
                })
                .await
        });
        Self {
            dir: config.cache_dir.clone(),
            socket: config.socket.clone(),
            node,
            stop: Some(tx),
            task: Some(task),
        }
    }

    /// Stops the node and waits for its serve task to finish, so the socket
    /// is free by the time this returns.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let done = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("node did not stop within 5s");
            done.expect("serve task panicked").expect("serve failed");
        }
    }

    /// Waits until no fetch is in flight, so every fetched object that will
    /// be cached is.
    pub async fn settle(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.node.metrics().inflight_objects.get() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "fetches never settled"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A client on the node's socket with `token`.
    pub async fn client(&self, token: &str) -> Remote {
        Remote::connect(Endpoint::NodeSocket(self.socket.clone()))
            .await
            .expect("connect")
            .with_token(token)
    }

    /// A raw channel to the node's socket.
    pub async fn channel(&self) -> tonic::transport::Channel {
        let path = self.socket.clone();
        tonic::transport::Endpoint::from_static("http://node.sock")
            .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
                let path = path.clone();
                async move {
                    let s = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
                }
            }))
            .await
            .expect("connect")
    }
}

/// A call context for client calls.
pub fn cx() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "tester".into(),
            kind: PrincipalKind::Agent,
            session: None,
            work_item: None,
            scopes: Vec::new(),
        },
        request_id: "test-1".into(),
        deadline_ms: None,
    }
}

/// Every cached object file under `dir`.
pub fn cached_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.join("objects")];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n.len() == 62) {
                out.push(p);
            }
        }
    }
    out
}

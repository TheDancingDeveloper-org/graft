//! Process-level harness: runs the `graft-server` binary as a child, talks
//! to it with the tonic clients generated in `graft-proto`, and speaks just
//! enough HTTP/1.1 for the health endpoints and the JSON facade.

#![allow(dead_code, unreachable_pub, unused_imports)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::pb;
use graft_proto::types::{CreateChangeset, Created, PutObjects};
use graft_proto::{ApiError, Frame, PROTOCOL_VERSION, VERSION_HEADER};
use graft_testkit::crash::TempDir;
use tonic::codec::CompressionEncoding;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

pub use graft_testkit::synth::{SynthSpec, generate};

/// The server binary.
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_graft-server")
}

/// A server's data directory and config file.
pub struct Site {
    pub dir: TempDir,
    pub config: PathBuf,
}

impl Site {
    /// A fresh site with `extra` TOML appended to the base config.
    pub fn new(tag: &str, extra: &str) -> Self {
        let dir = TempDir::new(tag);
        let config = dir.path().join("server.toml");
        let text = format!(
            "listen = \"127.0.0.1:0\"\nobjects_dir = \"{}\"\nmeta_db = \"{}\"\njson_logs = false\nshutdown_grace_s = 5\n{extra}\n",
            dir.path().join("objects").display(),
            dir.path().join("meta/meta.db").display(),
        );
        std::fs::write(&config, text).expect("write config");
        Self { dir, config }
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.dir.path().join("objects")
    }

    /// Starts the server and waits for it to listen.
    pub fn start(&self) -> Server {
        let started = Instant::now();
        let mut child = Command::new(bin())
            .arg("--config")
            .arg(&self.config)
            .arg("serve")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn graft-server");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut addr = None;
        let mut metrics = None;
        let mut line = String::new();
        while addr.is_none() || (self.has_metrics() && metrics.is_none()) {
            line.clear();
            let n = stdout.read_line(&mut line).expect("read stdout");
            assert!(n > 0, "graft-server exited before listening");
            if let Some(a) = line.trim().strip_prefix("graft-server listening on ") {
                addr = Some(a.parse().expect("listen address"));
            } else if let Some(a) = line.trim().strip_prefix("graft-server metrics on ") {
                metrics = Some(a.parse().expect("metrics address"));
            }
        }
        Server {
            child,
            _stdout: stdout,
            addr: addr.expect("address"),
            metrics,
            startup: started.elapsed(),
        }
    }

    fn has_metrics(&self) -> bool {
        std::fs::read_to_string(&self.config)
            .expect("config")
            .contains("\nmetrics =")
    }

    /// Runs `graft-server admin <args>` against this site.
    pub fn admin(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .arg("--config")
            .arg(&self.config)
            .arg("admin")
            .args(args)
            .output()
            .expect("run admin")
    }

    /// Runs `graft-server admin <args>` with `stdin` on its standard input
    /// (how a token reaches `admin token revoke`, never through argv).
    pub fn admin_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(bin())
            .arg("--config")
            .arg(&self.config)
            .arg("admin")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run admin");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait admin")
    }

    /// Mints a token with `scopes` on the host.
    pub fn mint(&self, scopes: &[&str]) -> String {
        self.mint_as("tester", scopes, 3600)
    }

    /// Mints an agent token for `subject` with `scopes`, valid for `ttl_s`.
    pub fn mint_as(&self, subject: &str, scopes: &[&str], ttl_s: u64) -> String {
        self.mint_kind(subject, "agent", scopes, ttl_s)
    }

    /// Mints a token of `kind` (`human`, `agent`, `ci` or `service`) for
    /// `subject` with `scopes`, valid for `ttl_s`.
    pub fn mint_kind(&self, subject: &str, kind: &str, scopes: &[&str], ttl_s: u64) -> String {
        let ttl = ttl_s.to_string();
        let mut args = vec![
            "token",
            "mint",
            "--subject",
            subject,
            "--kind",
            kind,
            "--ttl",
            &ttl,
        ];
        for s in scopes {
            args.extend(["--scope", s]);
        }
        let out = self.admin(&args);
        assert!(
            out.status.success(),
            "mint: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }
}

/// A running server.
pub struct Server {
    child: Child,
    _stdout: BufReader<ChildStdout>,
    pub addr: SocketAddr,
    pub metrics: Option<SocketAddr>,
    /// Spawn to listening.
    pub startup: Duration,
}

impl Server {
    /// The server's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Resident memory, from `/proc` (Linux), bytes.
    pub fn rss(&self) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.pid())).ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }

    /// `kill -9`.
    pub fn kill9(mut self) {
        self.child.kill().expect("kill");
        self.child.wait().expect("wait");
    }

    /// SIGTERM, then waits for a graceful exit.
    pub fn stop(mut self) -> ExitStatus {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill -TERM");
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "server did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub async fn channel(&self) -> Channel {
        Channel::from_shared(format!("http://{}", self.addr))
            .expect("uri")
            .connect()
            .await
            .expect("connect")
    }

    pub async fn clients(&self, token: Option<&str>) -> Clients {
        Clients::new(self.channel().await, Auth::new(token))
    }

    /// One HTTP/1.1 GET on the main listener.
    pub fn get(&self, path: &str, token: Option<&str>) -> HttpResponse {
        http_get(self.addr, path, token)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Adds the bearer token and the protocol version to every call.
#[derive(Clone)]
pub struct Auth {
    token: Option<String>,
    version: Option<String>,
}

impl Auth {
    pub fn new(token: Option<&str>) -> Self {
        Self {
            token: token.map(str::to_owned),
            version: Some(PROTOCOL_VERSION.to_string()),
        }
    }

    pub fn without_version(mut self) -> Self {
        self.version = None;
        self
    }
}

impl Interceptor for Auth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let md = req.metadata_mut();
        if let Some(t) = &self.token {
            md.insert(
                "authorization",
                format!("Bearer {t}").parse().expect("header"),
            );
        }
        if let Some(v) = &self.version {
            md.insert(VERSION_HEADER, v.parse().expect("header"));
        }
        md.insert("graft-request-id", "test-1".parse().expect("header"));
        Ok(req)
    }
}

type Svc = InterceptedService<Channel, Auth>;

/// The four service clients.
pub struct Clients {
    pub objects: pb::object_service_client::ObjectServiceClient<Svc>,
    pub repo: pb::repo_service_client::RepoServiceClient<Svc>,
    pub admin: pb::admin_service_client::AdminServiceClient<Svc>,
    pub workspaces: pb::workspace_service_client::WorkspaceServiceClient<Svc>,
}

const MAX_MESSAGE: usize = 80 << 20;

impl Clients {
    pub fn new(channel: Channel, auth: Auth) -> Self {
        let gz = CompressionEncoding::Gzip;
        Self {
            objects: pb::object_service_client::ObjectServiceClient::with_interceptor(
                channel.clone(),
                auth.clone(),
            )
            .send_compressed(gz)
            .accept_compressed(gz)
            .max_decoding_message_size(MAX_MESSAGE),
            repo: pb::repo_service_client::RepoServiceClient::with_interceptor(
                channel.clone(),
                auth.clone(),
            )
            .send_compressed(gz)
            .accept_compressed(gz)
            .max_decoding_message_size(MAX_MESSAGE),
            admin: pb::admin_service_client::AdminServiceClient::with_interceptor(
                channel.clone(),
                auth.clone(),
            )
            .send_compressed(gz)
            .accept_compressed(gz),
            workspaces: pb::workspace_service_client::WorkspaceServiceClient::with_interceptor(
                channel, auth,
            ),
        }
    }

    pub async fn create_repo(&mut self, name: &str) -> Result<(), ApiError> {
        self.admin
            .create_repo(pb::CreateRepoRequest { name: name.into() })
            .await
            .map(|_| ())
            .map_err(api)
    }

    /// Uploads blobs in batches.
    /// Creates or moves the line `name` to `tip`, from nothing.
    pub async fn set_line(
        &mut self,
        repo: &str,
        name: &str,
        tip: ChangesetId,
    ) -> Result<(), ApiError> {
        self.repo
            .update_ref(pb::UpdateRefRequest::from(graft_proto::types::UpdateRef {
                repo: repo.into(),
                name: name.into(),
                expected: None,
                new: tip,
                kind: graft_proto::types::RefKind::Line,
            }))
            .await
            .map(|_| ())
            .map_err(api)
    }

    pub async fn put_blobs(&mut self, repo: &str, blobs: &[Vec<u8>]) -> Result<(), ApiError> {
        for batch in blobs.chunks(512) {
            let messages: Vec<pb::PutObjectsRequest> = batch
                .chunks(128)
                .map(|part| {
                    PutObjects {
                        repo: repo.into(),
                        blobs: part
                            .iter()
                            .map(|b| Frame::encode(ObjectKind::Blob, b))
                            .collect(),
                    }
                    .into()
                })
                .collect();
            self.objects
                .put_objects(tokio_stream_iter(messages))
                .await
                .map_err(api)?;
        }
        Ok(())
    }

    /// Commits `snapshot` on top of `parent` (whose snapshot is `before`).
    pub async fn commit(
        &mut self,
        repo: &str,
        parent: Option<ChangesetId>,
        before: &BTreeMap<String, Vec<u8>>,
        snapshot: &BTreeMap<String, Vec<u8>>,
        n: usize,
    ) -> Result<Created, ApiError> {
        let changes = changes(before, snapshot);
        let new: Vec<Vec<u8>> = snapshot
            .iter()
            .filter(|(p, b)| before.get(*p) != Some(*b))
            .map(|(_, b)| b.clone())
            .collect();
        self.put_blobs(repo, &new).await?;
        let req = CreateChangeset {
            repo: repo.into(),
            parents: parent.into_iter().collect(),
            changes,
            author: author(),
            authored_at: Timestamp {
                ms: 1_700_000_000_000 + n as i64 * 1000,
                offset_min: 0,
            },
            message: format!("step {n}"),
            metadata: BTreeMap::new(),
            inline_blobs: Vec::new(),
            workspace: None,
        };
        let created = self
            .repo
            .create_changeset(pb::CreateChangesetRequest::from(req))
            .await
            .map_err(api)?
            .into_inner();
        Ok(Created::try_from(created).expect("valid created"))
    }

    /// Commits every snapshot of `synth` as a chain and returns the ids.
    pub async fn import(
        &mut self,
        repo: &str,
        snapshots: &[BTreeMap<String, Vec<u8>>],
    ) -> Vec<Created> {
        let mut out: Vec<Created> = Vec::new();
        let empty = BTreeMap::new();
        for (n, snap) in snapshots.iter().enumerate() {
            let before = if n == 0 { &empty } else { &snapshots[n - 1] };
            let parent = out.last().map(|c| c.id);
            out.push(
                self.commit(repo, parent, before, snap, n)
                    .await
                    .expect("commit"),
            );
        }
        out
    }

    /// Fetches the frames of `ids`, re-asking for deferred ones.
    pub async fn get_objects(&mut self, repo: &str, ids: &[ObjectId]) -> Vec<(ObjectId, Frame)> {
        let mut out = Vec::new();
        let mut want: Vec<ObjectId> = ids.to_vec();
        while !want.is_empty() {
            let batch: Vec<ObjectId> = want.drain(..want.len().min(1024)).collect();
            let mut stream = self
                .objects
                .get_objects(pb::GetObjectsRequest {
                    repo: repo.into(),
                    ids: batch.iter().map(|i| i.0.to_vec()).collect(),
                })
                .await
                .expect("get objects")
                .into_inner();
            while let Some(page) = stream.message().await.expect("page") {
                let page = graft_proto::types::Objects::try_from(page).expect("valid page");
                assert!(
                    page.missing.is_empty(),
                    "missing objects: {:?}",
                    page.missing
                );
                out.extend(page.found);
                want.extend(page.deferred);
            }
        }
        out
    }
}

fn tokio_stream_iter<T: Send + 'static>(
    items: Vec<T>,
) -> impl tonic::codegen::tokio_stream::Stream<Item = T> + Send + 'static {
    tonic::codegen::tokio_stream::iter(items)
}

pub fn author() -> Author {
    Author {
        name: "Synth".into(),
        email: "synth@example.invalid".into(),
        kind: AuthorKind::Agent,
    }
}

/// The changes that turn `before` into `after`.
pub fn changes(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, bytes) in after {
        if before.get(path) != Some(bytes) {
            out.push(Change::Put {
                path: RepoPath::new(path).expect("path"),
                kind: EntryKind::File,
                id: ObjectId::hash(ObjectKind::Blob, bytes),
            });
        }
    }
    for path in before.keys().filter(|p| !after.contains_key(*p)) {
        out.push(Change::Delete {
            path: RepoPath::new(path).expect("path"),
        });
    }
    out
}

/// The typed error in a status.
pub fn api(status: tonic::Status) -> ApiError {
    ApiError::from_status(&status)
        .unwrap_or_else(|| panic!("status without a graft error: {status:?}"))
}

/// An HTTP answer.
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.text()))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A minimal HTTP/1.1 GET (`Connection: close`).
pub fn http_get(addr: SocketAddr, path: &str, token: Option<&str>) -> HttpResponse {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: graft\r\n{auth}Connection: close\r\n\r\n"
    )
    .expect("write");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).expect("read");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut body = raw[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        body = dechunk(&body);
    }
    HttpResponse {
        status,
        headers,
        body,
    }
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk size");
        let size = usize::from_str_radix(
            std::str::from_utf8(&raw[..end])
                .expect("hex")
                .split(';')
                .next()
                .unwrap_or("0"),
            16,
        )
        .expect("chunk size");
        raw = &raw[end + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

/// `len` incompressible bytes, different for each `seed`.
pub fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 56) as u8
        })
        .collect()
}

/// Writes `count` valid blob frames of about `size` bytes straight into an
/// object store layout for repository id 1, without fsync (test set-up).
pub fn fill_objects(root: &Path, count: usize, size: usize) -> u64 {
    let mut total = 0u64;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut payload = vec![0u8; size];
    for n in 0..count {
        for chunk in payload.chunks_mut(8) {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        payload[..8].copy_from_slice(&(n as u64).to_le_bytes());
        let id = ObjectId::hash(ObjectKind::Blob, &payload);
        let hex = id.to_hex();
        let dir = root.join("1").join("objects").join(&hex[..2]);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let frame = Frame::encode(ObjectKind::Blob, &payload).to_bytes();
        total += frame.len() as u64;
        std::fs::write(dir.join(&hex[2..]), frame).expect("write object");
    }
    total
}

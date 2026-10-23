//! WI-2310: `OverlappingDrafts` over the draft path index, on both stores
//! (`docs/notes/draft-overlap.md`, D24).

mod common;

use common::*;
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, RepoApi, WorkspaceApi};
use graft_store::MetaStore;

const HOUR: u64 = 3600 * 1000;

struct Drafts<'f, M> {
    repo: Repo<'f, M>,
    root: ChangesetId,
}

/// A caller named `who` who can hold workspaces and see draft overlap.
fn agent(repo: &str, who: &str) -> CallContext {
    let mut cx = cx(&[
        &format!("repo:{repo}:write"),
        &format!("repo:{repo}:drafts"),
    ]);
    cx.principal.subject = who.to_owned();
    cx.principal.session = Some(format!("ses_{who}"));
    cx.principal.work_item = Some(format!("WI-{who}"));
    cx
}

impl<'f, M: MetaStore> Drafts<'f, M> {
    fn new(f: &'f Fixture<M>) -> Self {
        let repo = Repo::new(f, "r");
        let root = repo.commit(vec![], &[("README", b"hello")], &[]).id;
        repo.update_ref("main", None, root, RefKind::Line)
            .expect("main");
        repo.upload(&[b"1", b"2"]);
        Self { repo, root }
    }

    fn e(&self) -> &graft_engine::Engine<M, Faulty> {
        self.repo.e()
    }

    fn wref(&self, name: &str) -> WorkspaceRef {
        WorkspaceRef {
            repo: "r".to_owned(),
            name: name.to_owned(),
        }
    }

    /// Opens `ws` as `who` and commits one draft putting `paths`.
    fn draft(&self, who: &str, ws: &str, lease_ms: u64, paths: &[&str]) -> ChangesetId {
        let cx = agent("r", who);
        block_on(self.e().open_workspace(
            &cx,
            OpenWorkspace {
                repo: "r".to_owned(),
                name: Some(ws.to_owned()),
                line: "main".to_owned(),
                at: None,
                lease_ms,
            },
        ))
        .expect("open");
        self.commit(who, ws, self.root, paths)
    }

    fn commit(&self, who: &str, ws: &str, parent: ChangesetId, paths: &[&str]) -> ChangesetId {
        let mut changes: Vec<_> = paths.iter().map(|path| put(path, b"1")).collect();
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        let mut req = commit_req("r", vec![parent], changes);
        req.workspace = Some(ws.to_owned());
        // Same content in two workspaces would be one changeset.
        req.message = format!("{ws} on {parent}");
        block_on(self.e().create_changeset(&agent("r", who), req))
            .expect("commit")
            .id
    }

    fn query(
        &self,
        cx: &CallContext,
        paths: &[&str],
        include_own: bool,
        limit: u32,
        token: Option<String>,
    ) -> ApiResult<DraftOverlap> {
        block_on(self.e().overlapping_drafts(
            cx,
            OverlappingDrafts {
                repo: "r".to_owned(),
                line: "main".to_owned(),
                paths: paths.iter().map(|s| p(s)).collect(),
                include_own,
                page: Page { limit, token },
            },
        ))
    }

    /// Every hit, paging to the end, as `(path, workspace)`.
    fn all(&self, who: &str, paths: &[&str], include_own: bool) -> Vec<(String, String)> {
        let cx = agent("r", who);
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let page = self
                .query(&cx, paths, include_own, 50, token)
                .expect("query");
            out.extend(
                page.hits
                    .into_iter()
                    .map(|h| (h.path.to_string(), h.workspace)),
            );
            match page.next {
                Some(t) => token = Some(t),
                None => return out,
            }
        }
    }
}

fn hits(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

fn same_file_overlaps_and_disjoint_does_not<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    d.draft("alice", "wa", 0, &["src/x.rs"]);
    d.draft("bob", "wb", 0, &["src/x.rs", "src/y.rs"]);
    d.draft("carol", "wc", 0, &["doc/z.md"]);

    // Alice sees Bob on the file; not herself, not Carol.
    assert_eq!(
        d.all("alice", &["src/x.rs"], false),
        hits(&[("src/x.rs", "wb")])
    );
    // And Bob sees Alice.
    assert_eq!(
        d.all("bob", &["src/x.rs"], false),
        hits(&[("src/x.rs", "wa")])
    );
    // A directory prefix; overlapping inputs count once.
    assert_eq!(
        d.all("carol", &["src", "src/x.rs"], false),
        hits(&[("src/x.rs", "wa"), ("src/x.rs", "wb"), ("src/y.rs", "wb")])
    );
    assert_eq!(
        d.all("alice", &["src"], true),
        hits(&[("src/x.rs", "wa"), ("src/x.rs", "wb"), ("src/y.rs", "wb")])
    );
    // Disjoint drafts don't see each other.
    assert!(d.all("carol", &["src-x", "lib"], false).is_empty());
    assert!(d.all("alice", &["doc/y.md"], false).is_empty());
    assert_eq!(d.all("alice", &["doc"], false), hits(&[("doc/z.md", "wc")]));

    // The workspace entries: who, which session and work item, age, lease.
    // No head id or contents anywhere in the response.
    f.clock.advance(5_000);
    let page = d
        .query(&agent("r", "alice"), &["src/x.rs"], false, 10, None)
        .expect("query");
    assert_eq!(page.workspaces.len(), 1);
    let w = &page.workspaces[0];
    assert_eq!(w.workspace, "wb");
    assert_eq!(w.owner, "bob");
    assert_eq!(w.session.as_deref(), Some("ses_bob"));
    assert_eq!(w.work_item.as_deref(), Some("WI-bob"));
    assert!(w.draft_age_ms >= 5_000);
    assert!(w.lease_until_ms > 0);
    let text = format!("{page:?}");
    let head = block_on(d.e().get_workspace(&agent("r", "bob"), d.wref("wb")))
        .expect("get")
        .head
        .expect("head");
    assert!(!text.contains(&head.to_string()));
    assert!(!text.contains(&head.0.to_hex()[..16]));
}

fn closed_expired_and_emptied_workspaces_drop_out<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    d.draft("bob", "wb", HOUR, &["src/x.rs"]);
    d.draft("carol", "wc", 2 * HOUR, &["src/x.rs"]);
    let dave = d.draft("dave", "wd", 2 * HOUR, &["src/x.rs"]);
    d.draft("erin", "we", 2 * HOUR, &["src/x.rs"]);
    let everyone = hits(&[
        ("src/x.rs", "wb"),
        ("src/x.rs", "wc"),
        ("src/x.rs", "wd"),
        ("src/x.rs", "we"),
    ]);
    assert_eq!(d.all("alice", &["src"], false), everyone);

    // Closed.
    block_on(
        d.e()
            .close_workspace(&agent("r", "carol"), d.wref("wc"), true),
    )
    .expect("close");
    // Emptied: the head moves back to the base.
    block_on(d.e().set_head(
        &agent("r", "dave"),
        SetHead {
            workspace: d.wref("wd"),
            expected: Some(dave),
            new: None,
        },
    ))
    .expect("set head");
    assert_eq!(
        d.all("alice", &["src"], false),
        hits(&[("src/x.rs", "wb"), ("src/x.rs", "we")])
    );
    // Expired, before anything collects it.
    f.clock.advance((HOUR + 1) as i64);
    assert_eq!(d.all("alice", &["src"], false), hits(&[("src/x.rs", "we")]));
    // Ending the lapsed lease removes its rows.
    block_on(d.e().expire_leases("r", 10)).expect("expire");
    let (meta, _) = d.e().stores();
    let repo = block_on(meta.repo("r")).expect("repo").expect("exists");
    for gone in ["wb", "wc", "wd"] {
        assert!(
            block_on(meta.workspace_draft_paths(repo, gone, None, 10))
                .expect("paths")
                .is_empty(),
            "{gone}"
        );
    }
}

fn stack_edits_replace_the_paths<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    let one = d.draft("bob", "wb", 0, &["src/a.rs"]);
    let two = d.commit("bob", "wb", one, &["src/b.rs"]);
    assert_eq!(
        d.all("alice", &["src"], false),
        hits(&[("src/a.rs", "wb"), ("src/b.rs", "wb")])
    );
    // Moving the head down the stack drops the top draft's paths.
    block_on(d.e().set_head(
        &agent("r", "bob"),
        SetHead {
            workspace: d.wref("wb"),
            expected: Some(two),
            new: Some(one),
        },
    ))
    .expect("set head");
    assert_eq!(d.all("alice", &["src"], false), hits(&[("src/a.rs", "wb")]));
}

/// Moves `main` on by a commit putting `files` with content "1" or "2".
fn advance_main<M: MetaStore>(d: &Drafts<'_, M>, files: &[(&str, &[u8])]) -> ChangesetId {
    let tip = d.repo.commit(vec![d.root], files, &[]).id;
    d.repo
        .update_ref("main", Some(d.root), tip, RefKind::Line)
        .expect("advance main");
    tip
}

fn sync_replaces_the_paths<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    d.draft("bob", "wb", 0, &["src/a.rs", "src/b.rs"]);
    // `main` gets the same src/b.rs, so the rebased draft no longer
    // changes it.
    advance_main(&d, &[("src/b.rs", b"1")]);
    block_on(d.e().sync(&agent("r", "bob"), d.wref("wb"), None)).expect("sync");
    assert_eq!(d.all("alice", &["src"], false), hits(&[("src/a.rs", "wb")]));
}

fn resolve_replaces_the_paths<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    let head = d.draft("bob", "wb", 0, &["src/a.rs", "src/b.rs", "src/c.rs"]);
    // src/a.rs conflicts (added on both sides); src/b.rs is already there.
    let tip = advance_main(&d, &[("src/a.rs", b"2"), ("src/b.rs", b"1")]);
    block_on(d.e().resolve(
        &agent("r", "bob"),
        Resolve {
            workspace: d.wref("wb"),
            onto: tip,
            expected_head: Some(head),
            resolutions: vec![PathResolution {
                path: p("src/a.rs"),
                resolution: Resolution::Put {
                    kind: graft_model::EntryKind::File,
                    content: ResolvedContent::Inline(b"1".to_vec()),
                },
            }],
        },
    ))
    .expect("resolve");
    assert_eq!(
        d.all("alice", &["src"], false),
        hits(&[("src/a.rs", "wb"), ("src/c.rs", "wb")])
    );
}

/// A lease is live through its last millisecond, as `Engine::lapsed` reads
/// it.
fn a_lease_counts_until_its_end<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    d.draft("bob", "wb", HOUR, &["src/x.rs"]);
    f.clock.advance(HOUR as i64);
    assert_eq!(d.all("alice", &["src"], false), hits(&[("src/x.rs", "wb")]));
    f.clock.advance(1);
    assert!(d.all("alice", &["src"], false).is_empty());
}

fn needs_the_drafts_scope<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    d.draft("bob", "wb", 0, &["src/x.rs"]);
    for scopes in [
        vec!["repo:r:read"],
        vec!["repo:r:write"],
        vec!["repo:r:land:main"],
        vec!["repo:s:drafts"],
    ] {
        let err = d
            .query(&cx(&scopes), &["src"], false, 10, None)
            .expect_err("denied");
        assert_eq!(
            err,
            ApiError::PermissionDenied {
                scope: "repo:r:drafts".to_owned(),
                path: None,
            },
            "{scopes:?}"
        );
    }
    // `drafts` alone is enough, and so is `admin`.
    let page = d
        .query(&cx(&["repo:r:drafts"]), &["src"], false, 10, None)
        .expect("drafts");
    assert_eq!(page.hits.len(), 1);
    let page = d
        .query(&admin("r"), &["src"], false, 10, None)
        .expect("admin");
    assert_eq!(page.hits.len(), 1);
}

fn inputs_are_bounded<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    let cx = agent("r", "alice");
    let many: Vec<String> = (0..=MAX_OVERLAP_PATHS).map(|i| format!("p{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        d.query(&cx, &many, false, 10, None).expect_err("too many"),
        ApiError::LimitExceeded {
            limit: "paths".to_owned(),
            max: MAX_OVERLAP_PATHS as u64,
        }
    );
    assert_eq!(
        d.query(&cx, &["src"], false, MAX_OVERLAP_PAGE + 1, None)
            .expect_err("too big"),
        ApiError::LimitExceeded {
            limit: "limit".to_owned(),
            max: MAX_OVERLAP_PAGE.into(),
        }
    );
    assert!(matches!(
        d.query(&cx, &[], false, 10, None),
        Err(ApiError::InvalidChange { .. })
    ));
    // A token is bound to the inputs it was issued for.
    d.draft("bob", "wb", 0, &["src/a.rs"]);
    d.draft("carol", "wc", 0, &["src/a.rs"]);
    let page = d.query(&cx, &["src"], false, 1, None).expect("page");
    let token = page.next.expect("more");
    assert!(matches!(
        d.query(&cx, &["doc"], false, 1, Some(token.clone())),
        Err(ApiError::InvalidChange { .. })
    ));
    assert!(matches!(
        d.query(&cx, &["src"], false, 1, Some("junk".to_owned())),
        Err(ApiError::InvalidChange { .. })
    ));
    assert_eq!(
        d.query(&cx, &["src"], false, 1, Some(token))
            .expect("next")
            .hits[0]
            .workspace,
        "wc"
    );
    // Unknown line.
    assert!(matches!(
        block_on(d.e().overlapping_drafts(
            &cx,
            OverlappingDrafts {
                repo: "r".to_owned(),
                line: "nope".to_owned(),
                paths: vec![p("src")],
                include_own: false,
                page: Page::default(),
            },
        )),
        Err(ApiError::NotFound { .. })
    ));
}

/// A page scans at most 4 × limit index rows, whatever it filters out: with
/// nine of the caller's own rows ahead of one hit, a page of one comes back
/// empty with a token, and paging reaches the hit.
fn per_page_cost_is_bounded<M: MetaStore>(f: &Fixture<M>) {
    let d = Drafts::new(f);
    for i in 0..9 {
        d.draft("alice", &format!("a{i}"), 0, &["src/x.rs"]);
    }
    d.draft("bob", "zb", 0, &["src/x.rs"]);
    let cx = agent("r", "alice");
    let mut token = None;
    let mut pages = Vec::new();
    loop {
        let page = d.query(&cx, &["src"], false, 1, token).expect("page");
        assert!(page.hits.len() <= 1);
        pages.push(page.hits.len());
        match page.next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    // 10 rows at 4 per page: two empty pages, then the hit.
    assert_eq!(pages[..3], [0, 0, 1]);
    assert!(pages[3..].iter().all(|n| *n == 0));
    // Wider pages see the same set.
    assert_eq!(d.all("alice", &["src"], false), hits(&[("src/x.rs", "zb")]));
}

on_both_stores!(
    same_file_overlaps_and_disjoint_does_not,
    closed_expired_and_emptied_workspaces_drop_out,
    stack_edits_replace_the_paths,
    sync_replaces_the_paths,
    resolve_replaces_the_paths,
    a_lease_counts_until_its_end,
    needs_the_drafts_scope,
    inputs_are_bounded,
    per_page_cost_is_bounded,
);

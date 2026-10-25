//! LSP conformance (G3.6 acceptance): a scripted client drives `graft lsp`'s
//! server over the base protocol (initialize, hover, codeLens,
//! executeCommand, `graft/readFile`) against the testkit gRPC server with
//! the real engine behind it.

mod support;

use graft_lsp::{COMMANDS, Ended, GraftUri};
use graft_model::{AuthorKind, ChangesetId, RepoPath};
use serde_json::{Value, json};
use support::{Env, LIB_C1, LIB_C2, REPO, author};

fn short(id: ChangesetId) -> String {
    id.to_string()[..13].to_owned()
}

fn hover_text(msg: &Value) -> String {
    msg["result"]["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("a hover: {msg}"))
        .to_owned()
}

fn codes(diagnostics: &[Value]) -> Vec<(u64, u64, String)> {
    diagnostics
        .iter()
        .map(|d| {
            (
                d["range"]["start"]["line"].as_u64().unwrap(),
                d["severity"].as_u64().unwrap(),
                d["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn initialize_hover_and_code_lenses() {
    let mut env = Env::new().await;
    let (c1, c2) = (env.history.c1, env.history.c2);

    // Nothing is served before initialize.
    let early = env.hover(&env.uri("src/lib.rs"), 0).await;
    assert_eq!(early["error"]["code"], -32002);

    let init = env.initialize().await;
    let caps = &init["result"]["capabilities"];
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(caps["codeLensProvider"]["resolveProvider"], false);
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(caps["executeCommandProvider"]["commands"], json!(COMMANDS));
    assert_eq!(caps["experimental"]["graft"]["readFile"], "graft/readFile");
    assert_eq!(init["result"]["serverInfo"]["name"], "graft-lsp");

    // The base is the tip: no diagnostics.
    let lib = env.uri("src/lib.rs");
    assert_eq!(env.open(&lib).await, Vec::<Value>::new());

    // Line 0 came from the agent's changeset, line 1 from Ada's.
    let h0 = env.hover(&lib, 0).await;
    let text = hover_text(&h0);
    assert!(text.contains(&short(c2)), "{text}");
    assert!(text.contains("make a return 1"), "{text}");
    assert!(text.contains("agent\\-7 (agent)"), "{text}");
    assert!(text.contains("work item WI\\-2257"), "{text}");
    assert!(text.contains("session ses\\_test"), "{text}");
    assert!(!text.contains("local changes"), "{text}");
    assert_eq!(h0["result"]["range"]["start"]["line"], 0);
    assert_eq!(h0["result"]["range"]["end"]["line"], 1);
    let text = hover_text(&env.hover(&lib, 1).await);
    assert!(text.contains(&short(c1)), "{text}");
    assert!(text.contains("Ada (human)"), "{text}");
    assert!(text.contains("start the repository"), "{text}");
    assert!(!text.contains("with a body"), "{text}");
    // Past the end of the file there is nothing to say.
    assert_eq!(env.hover(&lib, 40).await["result"], Value::Null);

    let lenses = env.lenses(&lib).await;
    assert_eq!(lenses.len(), 2, "{lenses:?}");
    assert!(
        lenses[0].starts_with("changed 2 times, last by agent-7 (agent), "),
        "{lenses:?}"
    );
    assert_eq!(lenses[1], "base is up to date with main");

    // A local edit is flagged in the hover.
    env.write("src/lib.rs", "fn a() { 2 }\nfn b() {}\n");
    let text = hover_text(&env.hover(&lib, 0).await);
    assert!(text.contains("local changes"), "{text}");

    // A file outside the slice gets a warning, and no hover beyond blame.
    let readme = env.uri("README.md");
    assert_eq!(
        codes(&env.open(&readme).await),
        vec![(0, 2, "outside-slice".into())]
    );

    // Files outside the working copy get nothing.
    let other = graft_lsp::file_uri(&env.dir.path().join("elsewhere.rs"));
    assert_eq!(env.open(&other).await, Vec::<Value>::new());
    assert_eq!(env.hover(&other, 0).await["result"], Value::Null);
    assert_eq!(env.lenses(&other).await, Vec::<String>::new());

    // Upstream moves on: line 1 changes on main.
    let c3 = env
        .rig
        .land_on_main(
            &[("src/lib.rs", "fn a() { 1 }\nfn b() { 3 }\n")],
            author("Bo", AuthorKind::Human),
            &[],
            "make b return 3",
        )
        .await;
    env.client
        .notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": lib}}),
        )
        .await;
    let diags = env.client.diagnostics_for(&lib).await;
    assert_eq!(codes(&diags), vec![(1, 3, "upstream".into())]);
    assert!(
        diags[0]["message"].as_str().unwrap().contains(&short(c3)),
        "{diags:?}"
    );
    assert_eq!(diags[0]["source"], "graft");
    let lenses = env.lenses(&lib).await;
    assert_eq!(lenses[1], "base is 1 changeset behind main");

    // Shut down cleanly.
    let down = env.client.request("shutdown", Value::Null).await;
    assert_eq!(down["result"], Value::Null);
    let late = env.hover(&lib, 0).await;
    assert_eq!(late["error"]["code"], -32600);
    env.client.notify("exit", Value::Null).await;
    assert_eq!(env.client.ended().await, Ended::Clean);
}

#[tokio::test]
async fn graft_documents_and_blame_commands() {
    let mut env = Env::new().await;
    let (c1, c2) = (env.history.c1, env.history.c2);
    env.initialize().await;

    let at = |rev: &str| {
        GraftUri {
            repo: REPO.into(),
            rev: rev.into(),
            path: RepoPath::new("src/lib.rs").unwrap(),
        }
        .to_string()
    };

    // Any file at any changeset or ref, without a checkout.
    let main = env
        .client
        .request("graft/readFile", json!({"uri": at("main")}))
        .await;
    assert_eq!(main["result"]["text"], LIB_C2);
    assert_eq!(main["result"]["changeset"], c2.to_string());
    let old = env
        .client
        .request("graft/readFile", json!({"uri": at(&c1.to_string())}))
        .await;
    assert_eq!(old["result"]["text"], LIB_C1);
    // README.md is outside the slice, but readable this way.
    let readme = GraftUri {
        repo: REPO.into(),
        rev: "main".into(),
        path: RepoPath::new("README.md").unwrap(),
    };
    let got = env
        .client
        .request("graft/readFile", json!({"uri": readme.to_string()}))
        .await;
    assert_eq!(got["result"]["text"], "# r\n");

    // Bad URIs and other repositories are refused with typed errors.
    let bad = env
        .client
        .request("graft/readFile", json!({"uri": "graft://r/main"}))
        .await;
    assert_eq!(bad["error"]["code"], -32602);
    let other = env
        .client
        .request(
            "graft/readFile",
            json!({"uri": "graft://other/main/src/lib.rs"}),
        )
        .await;
    assert_eq!(other["error"]["code"], -32602);
    let missing = env
        .client
        .request("graft/readFile", json!({"uri": at("nope")}))
        .await;
    assert_eq!(missing["error"]["code"], -32803);
    assert_eq!(missing["error"]["data"]["error"], "not_found");

    // A graft: document has blame and history as of its changeset.
    let doc = at(&c1.to_string());
    assert_eq!(env.open(&doc).await, Vec::<Value>::new());
    let text = hover_text(&env.hover(&doc, 0).await);
    assert!(text.contains(&short(c1)), "{text}");
    let lenses = env.lenses(&doc).await;
    assert_eq!(lenses.len(), 1, "{lenses:?}");
    assert!(
        lenses[0].starts_with("changed 1 time, last by Ada (human)"),
        "{lenses:?}"
    );

    // graft.blameFile, by argument object or bare URI.
    let lib = env.uri("src/lib.rs");
    for arg in [json!({"uri": lib}), json!(lib)] {
        let out = env.command("graft.blameFile", json!([arg])).await;
        let ranges = out["result"]["ranges"].as_array().expect("ranges").clone();
        let got: Vec<(u64, u64, String)> = ranges
            .iter()
            .map(|r| {
                (
                    r["start"].as_u64().unwrap(),
                    r["end"].as_u64().unwrap(),
                    r["id"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![(0, 1, c2.to_string()), (1, 2, c1.to_string())],
            "{out}"
        );
        assert_eq!(ranges[0]["kind"], "agent");
    }
    let none = env.command("graft.blameFile", json!([])).await;
    assert_eq!(none["error"]["code"], -32602);
    let unknown = env.command("graft.nope", json!([])).await;
    assert_eq!(unknown["error"]["code"], -32602);
}

#[tokio::test]
async fn commit_diff_base_and_land_through_commands() {
    let mut env = Env::new().await;
    let base = env.history.c2;
    env.initialize().await;
    let lib = env.uri("src/lib.rs");
    env.open(&lib).await;

    // A commit needs a message, and something to commit.
    let refused = env.command("graft.commit", json!([{}])).await;
    assert_eq!(refused["error"]["code"], -32602);
    let nothing = env
        .command("graft.commit", json!([{"message": "nothing"}]))
        .await;
    assert_eq!(nothing["error"]["data"]["error"], "nothing_to_commit");

    env.write("src/lib.rs", "fn a() { 1 }\nfn b() { 2 }\n");
    let done = env
        .command(
            "graft.commit",
            json!([{"message": "make b return 2", "metadata": {"vogt.work_item": "WI-2257"}}]),
        )
        .await;
    let id = done["result"]["id"].as_str().expect("an id").to_owned();
    assert_eq!(done["result"]["parent"], base.to_string());
    assert_eq!(done["result"]["changes"], 1);
    let id = ChangesetId::parse(&id).unwrap();
    assert_eq!(env.working_copy().state().head, Some(id));
    let seen = env.client.take_seen();
    assert!(
        seen.iter().any(|m| m["method"] == "window/showMessage"
            && m["params"]["message"]
                .as_str()
                .unwrap()
                .contains(&short(id))),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| m["method"] == "workspace/codeLens/refresh"),
        "{seen:?}"
    );

    // The hover now shows the draft, with the configured author.
    let text = hover_text(&env.hover(&lib, 1).await);
    assert!(text.contains(&short(id)), "{text}");
    assert!(text.contains("Lin (human)"), "{text}");
    assert!(text.contains("draft"), "{text}");
    assert!(!text.contains("local changes"), "{text}");

    // graft.diffBase: the stack against the base, and the base document.
    let diff = env.command("graft.diffBase", json!([{"uri": lib}])).await;
    let r = &diff["result"];
    assert_eq!(r["base"], base.to_string());
    assert_eq!(r["head"], id.to_string());
    let base_uri = GraftUri {
        repo: REPO.into(),
        rev: base.to_string(),
        path: RepoPath::new("src/lib.rs").unwrap(),
    };
    assert_eq!(r["base_uri"], base_uri.to_string());
    assert_eq!(r["files"][0]["path"], "src/lib.rs");
    assert_eq!(r["files"][0]["change"], "put");
    let patch = r["files"][0]["patch"].as_str().expect("a patch");
    assert!(patch.contains("+fn b() { 2 }"), "{patch}");
    let all = env.command("graft.diffBase", json!([])).await;
    assert_eq!(all["result"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(all["result"]["base_uri"], Value::Null);

    // Land goes through the PR bridge and comes back queued.
    let land = env.command("graft.land", json!([{"squash": false}])).await;
    assert_eq!(land["result"]["outcome"], "queued", "{land}");
    assert_eq!(
        land["result"]["pr_url"],
        "https://github.com/example/r/pull/1"
    );
    assert_eq!(env.rig.prs(), vec![id]);
    let bad = env.command("graft.land", json!([{"squash": "yes"}])).await;
    assert_eq!(bad["error"]["code"], -32602);
}

#[tokio::test]
async fn a_conflicting_sync_publishes_conflict_regions() {
    let mut env = Env::new().await;
    env.initialize().await;
    let lib = env.uri("src/lib.rs");
    env.open(&lib).await;

    // Both sides change line 1.
    env.write("src/lib.rs", "fn a() { 1 }\nfn b() { mine }\n");
    let done = env
        .command("graft.commit", json!([{"message": "mine"}]))
        .await;
    assert!(done["result"]["id"].is_string(), "{done}");
    let theirs = env
        .rig
        .land_on_main(
            &[("src/lib.rs", "fn a() { 1 }\nfn b() { theirs }\n")],
            author("Bo", AuthorKind::Human),
            &[],
            "theirs",
        )
        .await;
    env.client.take_seen();

    let sync = env.command("graft.sync", json!([])).await;
    assert_eq!(sync["error"]["code"], -32803, "{sync}");
    assert_eq!(sync["error"]["data"]["error"], "conflict");
    assert_eq!(sync["error"]["data"]["paths"], json!(["src/lib.rs"]));
    assert_eq!(sync["error"]["data"]["onto"], theirs.to_string());

    // The conflict region is the line both sides changed, as an error.
    let diags = env.client.diagnostics_for(&lib).await;
    assert_eq!(codes(&diags), vec![(1, 1, "conflict".into())], "{diags:?}");
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains(&short(theirs)),
        "{diags:?}"
    );
    // The working copy is untouched.
    assert_eq!(
        std::fs::read_to_string(env.root.join("src/lib.rs")).unwrap(),
        "fn a() { 1 }\nfn b() { mine }\n"
    );

    // The conflict outlives a save, and a land is refused until a sync.
    env.client
        .notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": lib}}),
        )
        .await;
    let diags = env.client.diagnostics_for(&lib).await;
    assert_eq!(codes(&diags), vec![(1, 1, "conflict".into())]);
    let land = env.command("graft.land", json!([])).await;
    // The stack is still behind the tip, so land returns the stale base
    // (WI-2441) rather than a generic rejection.
    assert_eq!(land["error"]["data"]["error"], "stale_base", "{land}");
    assert_eq!(land["error"]["data"]["current_tip"], theirs.to_string());
    assert_eq!(
        codes(&env.client.diagnostics_for(&lib).await),
        vec![(1, 1, "conflict".into())]
    );
}

#[tokio::test]
async fn a_stale_base_land_publishes_conflict_regions() {
    let mut env = Env::new().await;
    env.initialize().await;
    let lib = env.uri("src/lib.rs");
    env.open(&lib).await;

    env.write("src/lib.rs", "fn a() { 1 }\nfn b() { mine }\n");
    let done = env
        .command("graft.commit", json!([{"message": "mine"}]))
        .await;
    assert!(done["result"]["id"].is_string(), "{done}");
    let theirs = env
        .rig
        .land_on_main(
            &[("src/lib.rs", "fn a() { 1 }\nfn b() { theirs }\n")],
            author("Bo", AuthorKind::Human),
            &[],
            "theirs",
        )
        .await;
    env.client.take_seen();

    let land = env.command("graft.land", json!([])).await;
    assert_eq!(land["error"]["data"]["error"], "stale_base", "{land}");
    assert_eq!(land["error"]["data"]["paths"], json!(["src/lib.rs"]));
    assert_eq!(land["error"]["data"]["current_tip"], theirs.to_string());
    let diags = env.client.diagnostics_for(&lib).await;
    assert_eq!(codes(&diags), vec![(1, 1, "conflict".into())], "{diags:?}");
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains(&short(theirs)),
        "{diags:?}"
    );
    assert_eq!(
        std::fs::read_to_string(env.root.join("src/lib.rs")).unwrap(),
        "fn a() { 1 }\nfn b() { mine }\n"
    );
}

#[tokio::test]
async fn a_sync_that_fast_forwards_clears_upstream_diagnostics() {
    let mut env = Env::new().await;
    env.initialize().await;
    let lib = env.uri("src/lib.rs");
    env.open(&lib).await;
    let c3 = env
        .rig
        .land_on_main(
            &[("src/lib.rs", "fn a() { 1 }\nfn b() { 3 }\n")],
            author("Bo", AuthorKind::Human),
            &[],
            "three",
        )
        .await;
    env.client
        .notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": lib}}),
        )
        .await;
    assert_eq!(
        codes(&env.client.diagnostics_for(&lib).await),
        vec![(1, 3, "upstream".into())]
    );

    let sync = env.command("graft.sync", json!([])).await;
    assert_eq!(sync["result"]["base"], c3.to_string(), "{sync}");
    assert_eq!(sync["result"]["changed"], 1);
    assert_eq!(env.client.diagnostics_for(&lib).await, Vec::<Value>::new());
    assert_eq!(
        std::fs::read_to_string(env.root.join("src/lib.rs")).unwrap(),
        "fn a() { 1 }\nfn b() { 3 }\n"
    );
    assert_eq!(env.lenses(&lib).await[1], "base is up to date with main");
}

#[tokio::test]
async fn the_working_copy_is_reloaded_when_it_changes_on_disk() {
    let mut env = Env::new().await;
    env.initialize().await;
    let lib = env.uri("src/lib.rs");
    env.open(&lib).await;
    let c3 = env
        .rig
        .land_on_main(
            &[("src/lib.rs", "fn a() { 1 }\nfn b() { 3 }\n")],
            author("Bo", AuthorKind::Human),
            &[],
            "three",
        )
        .await;
    assert_eq!(env.lenses(&lib).await[1], "base is 1 changeset behind main");

    // Another process (the CLI) updates the working copy.
    let remote = support::remote(&env.server).await;
    let mut wc = env.working_copy();
    let cache = wc
        .open_cache(graft_client::workcopy::DEFAULT_CACHE_BYTES)
        .unwrap();
    wc.update(&remote, &cache, &support::admin(), c3, &Default::default())
        .await
        .expect("update");
    assert_eq!(env.lenses(&lib).await[1], "base is up to date with main");
    let text = hover_text(&env.hover(&lib, 1).await);
    assert!(text.contains("Bo (human)"), "{text}");
}

#[tokio::test]
async fn protocol_errors_are_answered_and_the_session_survives() {
    let mut env = Env::new().await;
    env.initialize().await;

    env.client.send_raw(b"{not json").await;
    let msg = env.client.read().await;
    assert_eq!(msg["error"]["code"], -32700);
    assert_eq!(msg["id"], Value::Null);

    let unknown = env
        .client
        .request("textDocument/definition", json!({}))
        .await;
    assert_eq!(unknown["error"]["code"], -32601);
    let bad = env.client.request("textDocument/hover", json!({})).await;
    assert_eq!(bad["error"]["code"], -32602);

    // A string id is echoed back.
    env.client
        .send(&json!({"jsonrpc": "2.0", "id": "a-1", "method": "shutdown"}))
        .await;
    let down = env.client.response(json!("a-1")).await;
    assert_eq!(down["result"], Value::Null);
    env.client.notify("exit", Value::Null).await;
    assert_eq!(env.client.ended().await, Ended::Clean);
}

#[tokio::test]
async fn exit_without_shutdown_or_closed_input_is_unclean() {
    let mut env = Env::new().await;
    env.initialize().await;
    env.client.notify("exit", Value::Null).await;
    assert_eq!(env.client.ended().await, Ended::Unclean);

    let mut env = Env::new().await;
    env.initialize().await;
    let task = env.client.task.take().unwrap();
    drop(env.client);
    assert_eq!(task.await.unwrap().unwrap(), Ended::Unclean);
}

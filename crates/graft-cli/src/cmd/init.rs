//! `graft init`: creates a repository and its first working copy locally.

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{self, WcConfig};
use graft_client::{Backend, LocalRepo, Sparse};
use graft_model::{Changeset, Metadata};
use graft_proto::api::RepoApi;
use graft_proto::types::{CreateChangeset, CreateLine};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::cli::{DEFAULT_LINE, Global, InitArgs};
use crate::conn::runtime;
use crate::error::Failure;
use crate::render::{print_json, short};

pub(crate) fn run(g: &Global, args: &InitArgs) -> Result<(), Failure> {
    let dir = args.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let dir = absolute(&dir)?;
    let name = match &args.repo {
        Some(name) => name.clone(),
        None => dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Failure::usage("cannot infer repository name; pass --repo NAME"))?
            .to_owned(),
    };
    let repo_dir = dir.join(".graft").join("repo");
    let local = LocalRepo::create(&repo_dir, &name)?;

    runtime()?.block_on(async {
        let api = Backend::local(local.engine_handle(), name.clone());
        let cx = LocalRepo::context(&name, "local-init");
        let root_cs = Changeset::canonical_empty_root();
        let root = api
            .create_changeset(
                &cx,
                CreateChangeset {
                    repo: name.clone(),
                    parents: root_cs.parents.clone(),
                    changes: root_cs.changes.clone(),
                    author: root_cs.author.clone(),
                    authored_at: root_cs.authored_at,
                    message: root_cs.message.clone(),
                    metadata: Metadata::new(),
                    inline_blobs: Vec::new(),
                    workspace: None,
                },
            )
            .await?;
        api.create_line(
            &cx,
            CreateLine {
                repo: name.clone(),
                name: DEFAULT_LINE.to_owned(),
                from: root.id,
            },
        )
        .await?;
        let config = WcConfig {
            endpoint: None,
            repo: name.clone(),
            workspace: None,
        };
        let (mut wc, _) = workcopy::clone(
            &api,
            &cx,
            &dir,
            config,
            DEFAULT_LINE,
            None,
            Sparse::full(),
            &FetchOptions::default(),
        )
        .await?;
        let server = format!("local:{}", repo_dir.display());
        wc.set_server(Some(server))?;

        if g.json {
            return print_json(&json!({
                "repo": name,
                "line": DEFAULT_LINE,
                "base": root.id.to_string(),
                "dir": dir.display().to_string(),
            }));
        }
        outln!(
            "initialized {} at {} in {}",
            name,
            short(root.id),
            dir.display()
        );
        Ok(())
    })
}

fn absolute(path: &Path) -> Result<PathBuf, Failure> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

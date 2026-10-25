//! `graft login` and `graft logout`: named server credentials.

use std::fs;
use std::io::{self, Read};

use graft_client::Remote;
use graft_proto::ApiError;
use graft_proto::api::RepoApi;
use graft_proto::types::{ListRefs, Page};

use crate::cli::{Global, LoginArgs, LogoutArgs};
use crate::config::{self, ServerConfig};
use crate::error::Failure;
use crate::render::print_json;

pub(crate) fn run(g: &Global, args: &LoginArgs) -> Result<(), Failure> {
    if args.list {
        return list(g);
    }
    let name = args
        .name
        .as_deref()
        .ok_or_else(|| Failure::usage("graft login needs a server name, or --list"))?;
    config::validate_name(name)?;
    let endpoint = g
        .endpoint
        .as_deref()
        .ok_or_else(|| Failure::usage("graft login needs --endpoint URL"))?;
    let token = match &g.token_file {
        Some(path) => fs::read_to_string(path).map_err(|error| {
            Failure::new(
                crate::Exit::Failure,
                "token",
                format!(
                    "cannot read the token file {}: {}",
                    path.display(),
                    error.kind()
                ),
            )
        })?,
        None => {
            let mut token = String::new();
            io::stdin().read_to_string(&mut token)?;
            token
        }
    };
    let token = token.trim();
    if token.is_empty() {
        return Err(Failure::usage("the token is empty"));
    }

    let endpoint_value = graft_client::Endpoint::parse(endpoint);
    crate::conn::runtime()?.block_on(async {
        let remote = Remote::connect(endpoint_value).await?.with_token(token);
        let probe = remote
            .list_refs(
                &crate::conn::call_context(),
                ListRefs {
                    repo: "__graft_login_probe__".to_owned(),
                    prefix: None,
                    page: Page {
                        limit: 1,
                        token: None,
                    },
                },
            )
            .await;
        match probe {
            Ok(_) | Err(ApiError::NotFound { .. }) | Err(ApiError::PermissionDenied { .. }) => {
                Ok::<(), Failure>(())
            }
            Err(error) => Err(error.into()),
        }
    })?;

    let token_path = config::token_path(name)?;
    config::store_token(&token_path, token)?;
    let mut user = config::load()?;
    let first = user.servers.is_empty();
    user.servers.insert(
        name.to_owned(),
        ServerConfig {
            endpoint: endpoint.to_owned(),
            token_file: token_path,
        },
    );
    if first || user.default_server.is_none() {
        user.default_server = Some(name.to_owned());
    }
    config::save(&user)?;

    if g.json {
        return print_json(&serde_json::json!({
            "server": name,
            "endpoint": endpoint,
            "default": user.default_server.as_deref() == Some(name),
            "token": "<redacted>",
        }));
    }
    outln!("logged in to {name} at {endpoint}");
    Ok(())
}

pub(crate) fn logout(g: &Global, args: &LogoutArgs) -> Result<(), Failure> {
    config::validate_name(&args.name)?;
    let mut user = config::load()?;
    let server = user.servers.remove(&args.name).ok_or_else(|| {
        Failure::new(
            crate::Exit::Failure,
            "config",
            format!("server {} is not configured", args.name),
        )
    })?;
    config::remove_token(&server.token_file)?;
    if user.default_server.as_deref() == Some(args.name.as_str()) {
        user.default_server = user.servers.keys().next().cloned();
    }
    config::save(&user)?;
    if g.json {
        return print_json(&serde_json::json!({"server": args.name, "logged_out": true}));
    }
    outln!("logged out of {}", args.name);
    Ok(())
}

fn list(g: &Global) -> Result<(), Failure> {
    let user = config::load()?;
    if g.json {
        let servers = user
            .servers
            .iter()
            .map(|(name, server)| {
                serde_json::json!({
                    "name": name,
                    "endpoint": server.endpoint,
                    "default": user.default_server.as_deref() == Some(name),
                    "token": "<redacted>",
                })
            })
            .collect::<Vec<_>>();
        return print_json(&serde_json::json!({"servers": servers}));
    }
    for (name, server) in &user.servers {
        let marker = if user.default_server.as_deref() == Some(name) {
            "*"
        } else {
            " "
        };
        outln!("{marker} {name}\t{}", server.endpoint);
    }
    Ok(())
}

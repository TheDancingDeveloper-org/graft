//! `graft config show`: effective user configuration with redacted tokens.

use crate::cli::{ConfigArgs, ConfigCommand, Global};
use crate::config;
use crate::conn;
use crate::error::Failure;
use crate::render::print_json;

pub(crate) fn run(g: &Global, args: &ConfigArgs) -> Result<(), Failure> {
    match args.command {
        ConfigCommand::Show => show(g),
    }
}

fn show(g: &Global) -> Result<(), Failure> {
    let user = config::load()?;
    let wc = conn::working_copy_info()?;
    let effective = conn::resolve_settings(g, wc.as_ref(), &user)?;
    let servers = user
        .servers
        .iter()
        .map(|(name, server)| {
            serde_json::json!({
                "name": name,
                "endpoint": server.endpoint,
                "token_file": server.token_file,
                "token": "<redacted>",
            })
        })
        .collect::<Vec<_>>();
    let value = serde_json::json!({
        "config_file": config::config_path()?,
        "default_server": user.default_server,
        "servers": servers,
        "effective": {
            "server": effective.server,
            "endpoint": effective.endpoint.to_string(),
            "token": if effective.token.is_some() { "<redacted>" } else { "<none>" },
        },
    });
    if g.json {
        return print_json(&value);
    }
    outln!(
        "config_file = {}",
        value["config_file"].as_str().unwrap_or("")
    );
    if let Some(name) = value["default_server"].as_str() {
        outln!("default_server = {name}");
    }
    for server in value["servers"].as_array().into_iter().flatten() {
        outln!("\n[servers.{}]", server["name"].as_str().unwrap_or(""));
        outln!("endpoint = {}", server["endpoint"].as_str().unwrap_or(""));
        outln!(
            "token_file = {}",
            server["token_file"].as_str().unwrap_or("")
        );
        outln!("token = <redacted>");
    }
    outln!("\n[effective]");
    if let Some(name) = value["effective"]["server"].as_str() {
        outln!("server = {name}");
    }
    outln!(
        "endpoint = {}",
        value["effective"]["endpoint"].as_str().unwrap_or("")
    );
    outln!(
        "token = {}",
        value["effective"]["token"].as_str().unwrap_or("<none>")
    );
    Ok(())
}

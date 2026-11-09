//! `nuee mcp`: the verbs of one group as tools for a coding agent, over the Model Context Protocol
//! (newline-delimited JSON-RPC on stdin and stdout).
//!
//! The server is bound to the group given on its command line: an agent cannot point it at another
//! group, and what the group's machines run is still decided by their own `--allow-command`.

use std::process::{ExitCode, Stdio};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use crate::{Ctx, exit, fail};

/// Protocol version answered to a client that names none.
const PROTOCOL: &str = "2025-06-18";

/// Arguments of `nuee mcp`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The only group this server acts on
    #[arg(long, value_name = "LINK", env = "NUEE_GROUP", hide_env_values = true)]
    pub on: String,
}

fn tools() -> Value {
    let object = |properties: Value, required: Value| json!({ "type": "object", "properties": properties, "required": required });
    json!([
        {
            "name": "group_status",
            "description": "List the machines of the group and which one coordinates.",
            "inputSchema": object(json!({}), json!([])),
        },
        {
            "name": "build",
            "description": "Build a pushed commit on a machine of the group and bring its output back. The command must be one the machines allow.",
            "inputSchema": object(json!({
                "command": { "type": "string" },
                "keep": { "type": "array", "items": { "type": "string" }, "description": "Paths to bring back" },
                "rev": { "type": "string", "description": "Commit (default: HEAD of the working directory)" },
                "out": { "type": "string", "description": "Local directory for the output" },
                "fresh": { "type": "boolean", "description": "Run even if this commit already has a result" },
            }), json!(["command"])),
        },
        {
            "name": "shard",
            "description": "Cut a test command across the group's machines; {i} and {n} in the command are the piece and the number of pieces.",
            "inputSchema": object(json!({
                "shards": { "type": "integer", "minimum": 1, "maximum": 64 },
                "command": { "type": "string" },
                "rev": { "type": "string" },
                "out": { "type": "string" },
                "fresh": { "type": "boolean" },
            }), json!(["shards", "command"])),
        },
        {
            "name": "files",
            "description": "List the files and sites the group keeps.",
            "inputSchema": object(json!({}), json!([])),
        },
        {
            "name": "put_file",
            "description": "Give a local file or folder to the group.",
            "inputSchema": object(json!({
                "path": { "type": "string" },
                "name": { "type": "string" },
            }), json!(["path"])),
        },
        {
            "name": "get_file",
            "description": "Download one of the group's files.",
            "inputSchema": object(json!({
                "name": { "type": "string" },
                "out": { "type": "string", "description": "Local directory" },
            }), json!(["name"])),
        },
    ])
}

/// The command line of a tool; the group link goes on stdin. `None` for an unknown tool.
fn command_line(tool: &str, a: &Value) -> Option<Result<Vec<String>, String>> {
    let text = |key: &str| a[key].as_str().map(str::to_owned);
    let required = |key: &str| text(key).ok_or_else(|| format!("{key} is required"));
    let pieces = |verb: &[&str], rest: &mut Vec<String>| {
        let mut argv: Vec<String> = verb.iter().map(|v| (*v).to_owned()).collect();
        argv.append(rest);
        argv
    };
    let shared = |argv: &mut Vec<String>| {
        for key in ["rev", "out"] {
            if let Some(value) = text(key) {
                argv.extend([format!("--{key}"), value]);
            }
        }
        if a["fresh"].as_bool() == Some(true) {
            argv.push("--fresh".to_owned());
        }
    };
    Some((|| match tool {
        "group_status" => Ok(pieces(&["group", "status", "-"], &mut Vec::new())),
        "files" => Ok(pieces(&["group", "ls", "-"], &mut Vec::new())),
        "put_file" => {
            let mut rest = vec![required("path")?];
            if let Some(name) = text("name") {
                rest.extend(["--name".to_owned(), name]);
            }
            Ok(pieces(&["group", "put", "-"], &mut rest))
        }
        "get_file" => {
            let mut rest = vec![required("name")?];
            if let Some(out) = text("out") {
                rest.extend(["--out".to_owned(), out]);
            }
            Ok(pieces(&["group", "get", "-"], &mut rest))
        }
        "build" => {
            let mut argv = pieces(&["build", "--on", "-"], &mut Vec::new());
            shared(&mut argv);
            for path in a["keep"].as_array().into_iter().flatten() {
                argv.extend([
                    "--keep".to_owned(),
                    path.as_str().unwrap_or_default().to_owned(),
                ]);
            }
            argv.extend(["--".to_owned(), required("command")?]);
            Ok(argv)
        }
        "shard" => {
            let shards = a["shards"]
                .as_u64()
                .ok_or_else(|| "shards is required".to_owned())?;
            let mut argv = pieces(
                &["shard", &shards.to_string(), "--on", "-"],
                &mut Vec::new(),
            );
            shared(&mut argv);
            argv.extend(["--".to_owned(), required("command")?]);
            Ok(argv)
        }
        _ => Err(String::new()),
    })())
    .filter(|r| !matches!(r, Err(e) if e.is_empty()))
}

/// Runs one tool as a `nuee` process and returns its events as the tool's result.
async fn call(tool: &str, argv: &[String], link: &str, ctx: &Ctx) -> anyhow::Result<Value> {
    let mut cmd = tokio::process::Command::new(std::env::current_exe()?);
    cmd.arg("--json")
        .arg("--web-base")
        .arg(&ctx.global.web_base);
    if let Some(relay) = &ctx.global.relay {
        cmd.arg("--relay").arg(relay);
    }
    // The same identity and store as this server, whether they came from flags or the environment.
    if let Some(dir) = &ctx.global.config_dir {
        cmd.arg("--config-dir").arg(dir);
    }
    if let Some(dir) = &ctx.global.data_dir {
        cmd.arg("--data-dir").arg(dir);
    }
    let mut child = cmd
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(link.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    tracing::debug!(tool, status = ?out.status, "tool done");
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !out.status.success(),
    }))
}

/// The reply to one request; `None` for a notification.
async fn reply(request: &Value, link: &str, ctx: &Ctx) -> Option<Value> {
    let id = request.get("id")?.clone();
    let error = |code: i32, message: &str| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": request["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "nuee", "version": env!("CARGO_PKG_VERSION") },
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or_default();
            match command_line(name, &request["params"]["arguments"]) {
                None => return Some(error(-32602, "unknown tool")),
                Some(Err(message)) => return Some(error(-32602, &message)),
                Some(Ok(argv)) => match call(name, &argv, link, ctx).await {
                    Ok(result) => result,
                    Err(e) => return Some(error(-32603, &format!("{e:#}"))),
                },
            }
        }
        _ => return Some(error(-32601, "method not found")),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Serves until stdin closes.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let link = crate::link_arg(&args.on)?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| fail(exit::ERROR, format!("reading stdin: {e}")))?
    {
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(request) => reply(&request, &link, ctx).await,
            Err(e) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": e.to_string() },
            })),
        };
        if let Some(answer) = answer {
            stdout.write_all(answer.to_string().as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

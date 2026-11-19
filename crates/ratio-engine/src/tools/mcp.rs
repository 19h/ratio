use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

use crate::{api::ToolCall, config::McpServer, context::clip};

const MAX_FRAME: usize = 1024 * 1024;
const MODERN: &str = "2026-07-28";

#[derive(Deserialize)]
struct Request {
    server: String,
    name: Option<String>,
    #[serde(default)]
    arguments: Map<String, Value>,
}

pub(crate) fn schemas(servers: &BTreeMap<String, McpServer>) -> Vec<Value> {
    if servers.is_empty() {
        return Vec::new();
    }
    let server = json!({"type":"string","enum":servers.keys().collect::<Vec<_>>()});
    vec![
        super::schema(
            "mcp_list",
            "List allowed tools and their input schemas on a configured MCP server.",
            json!({"server":server}),
            &["server"],
        ),
        super::schema(
            "mcp_call",
            "Run an allowed MCP tool using its input schema from mcp_list.",
            json!({"server":server, "name":{"type":"string"}, "arguments":{"type":"object"}}),
            &["server", "name"],
        ),
    ]
}

pub(crate) async fn dispatch(
    servers: &BTreeMap<String, McpServer>,
    call: &ToolCall,
    workspace: &Path,
    time_limit: Duration,
    output_limit: usize,
    key: Option<&str>,
) -> String {
    let output = match timeout(time_limit, run(servers, call, workspace)).await {
        Ok(Ok(value)) => format!("Untrusted MCP tool output\n{value}"),
        Ok(Err(error)) => format!("Tool error: {error:#}"),
        Err(_) => "Tool error: MCP server timed out".into(),
    };
    let output = match key {
        Some(key) => output.replace(key, "[redacted]"),
        None => output,
    };
    clip(&output, output_limit)
}

async fn run(
    servers: &BTreeMap<String, McpServer>,
    call: &ToolCall,
    workspace: &Path,
) -> Result<Value> {
    ensure!(call.kind == "function", "unsupported tool call type");
    ensure!(
        call.function.arguments.len() <= 128 * 1024,
        "MCP call arguments too large"
    );
    let input: Request = crate::api::tool_arguments(&call.function.arguments)?;
    let server = servers
        .get(&input.server)
        .context("unknown configured MCP server")?;
    if call.function.name == "mcp_call" {
        let name = input.name.context("mcp_call requires name")?;
        ensure!(
            server.tools.contains(&name),
            "MCP tool is outside the configured allowlist"
        );
        let mut client = Client::open(server, workspace).await?;
        return client
            .rpc(
                "tools/call",
                json!({"name":name,"arguments":input.arguments}),
            )
            .await;
    }
    let mut client = Client::open(server, workspace).await?;
    let mut selected = BTreeMap::new();
    let mut cursor = None;
    for _ in 0..16 {
        let params = cursor.map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
        let result = client.rpc("tools/list", params).await?;
        for tool in result["tools"]
            .as_array()
            .context("MCP list result is missing tools")?
        {
            let name = tool["name"].as_str().context("MCP tool is missing name")?;
            if server.tools.iter().any(|allowed| allowed == name) {
                ensure!(
                    tool["inputSchema"].is_object(),
                    "MCP tool is missing an input schema"
                );
                selected.insert(name.to_owned(), json!({
                    "name":name, "description":tool["description"], "inputSchema":tool["inputSchema"]
                }));
            }
        }
        cursor = result["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return Ok(json!({"tools":selected.into_values().collect::<Vec<_>>() }));
        }
    }
    anyhow::bail!("MCP tools/list page limit reached")
}

struct Client {
    _child: Child,
    #[cfg(unix)]
    _group: super::ProcessGroup,
    #[cfg(windows)]
    _job: super::windows_job::Job,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u32,
    modern: bool,
}

impl Client {
    async fn open(server: &McpServer, workspace: &Path) -> Result<Self> {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .current_dir(workspace)
            .env_remove("OPENAI_API_KEY")
            .env_remove("RATIO_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        let job = super::windows_job::Job::prepare(&mut command)?;
        let mut child = command.spawn().context("start configured MCP server")?;
        #[cfg(unix)]
        let group = super::ProcessGroup(child.id());
        #[cfg(windows)]
        job.start(&child)?;
        let stdin = child.stdin.take().context("open MCP stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("open MCP stdout")?);
        let mut client = Self {
            _child: child,
            #[cfg(unix)]
            _group: group,
            #[cfg(windows)]
            _job: job,
            stdin,
            stdout,
            next_id: 0,
            modern: server.protocol == MODERN,
        };
        if !client.modern {
            let hello = client.rpc("initialize", json!({
                "protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":client_info()
            })).await?;
            ensure!(
                matches!(
                    hello["protocolVersion"].as_str(),
                    Some("2025-11-25" | "2025-06-18" | "2025-03-26" | "2024-11-05")
                ),
                "MCP server selected an unsupported protocol"
            );
            ensure!(
                hello.pointer("/capabilities/tools").is_some(),
                "MCP server does not advertise tools"
            );
            client
                .send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await?;
        }
        Ok(client)
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        ensure!(bytes.len() < MAX_FRAME, "MCP request frame too large");
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn rpc(&mut self, method: &str, mut params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        if self.modern {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": MODERN,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": client_info()
            });
        }
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        for _ in 0..256 {
            let value = self.receive().await?;
            if let Some(method) = value["method"].as_str() {
                if let Some(id) = value.get("id") {
                    let response = if method == "ping" {
                        json!({"jsonrpc":"2.0","id":id,"result":{}})
                    } else {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported client method"}})
                    };
                    self.send(&response).await?;
                }
                continue;
            }
            ensure!(value["id"] == id, "MCP response ID does not match request");
            if !value["error"].is_null() {
                anyhow::bail!(
                    "MCP server error: {}",
                    clip(&value["error"].to_string(), 2048)
                );
            }
            return value
                .get("result")
                .cloned()
                .context("MCP response is missing its result");
        }
        anyhow::bail!("MCP message limit reached")
    }

    async fn receive(&mut self) -> Result<Value> {
        let mut frame = Vec::new();
        loop {
            let bytes = self.stdout.fill_buf().await?;
            ensure!(!bytes.is_empty(), "MCP server closed stdout");
            let newline = bytes.iter().position(|&byte| byte == b'\n');
            let length = newline.map_or(bytes.len(), |index| index + 1);
            ensure!(
                frame.len() + length <= MAX_FRAME,
                "MCP response frame exceeds size limit"
            );
            frame.extend_from_slice(&bytes[..length]);
            self.stdout.consume(length);
            if newline.is_some() {
                let value: Value =
                    serde_json::from_slice(&frame).context("decode MCP JSON-RPC frame")?;
                ensure!(
                    value["jsonrpc"] == "2.0",
                    "unsupported MCP JSON-RPC version"
                );
                return Ok(value);
            }
        }
    }
}

fn client_info() -> Value {
    json!({"name":"ratio","version":env!("CARGO_PKG_VERSION")})
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::api::Function;

    #[tokio::test]
    async fn lists_allowed_tools_using_the_legacy_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let server = McpServer {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), r#"
                IFS= read -r frame
                case "$frame" in *initialize*) ;; *) exit 1 ;; esac
                printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}'
                IFS= read -r frame
                case "$frame" in *notifications/initialized*) ;; *) exit 1 ;; esac
                IFS= read -r frame
                case "$frame" in *tools/list*) ;; *) exit 1 ;; esac
                printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"query","inputSchema":{}},{"name":"unlisted","inputSchema":{}}]}}'
            "#.into()],
            tools: vec!["query".into()],
            protocol: "2025-11-25".into(),
        };
        let servers = BTreeMap::from([("fixture".into(), server)]);
        let call = |tool: &str, args: Value| ToolCall {
            id: "fixture".into(),
            kind: "function".into(),
            function: Function {
                name: tool.into(),
                arguments: args.to_string(),
            },
        };
        let listing = dispatch(
            &servers,
            &call("mcp_list", json!({"server":"fixture"})),
            directory.path(),
            Duration::from_secs(5),
            1024,
            None,
        )
        .await;
        assert!(listing.contains("query"), "{listing}");
        assert!(!listing.contains("unlisted"));
        let refused = run(
            &servers,
            &call("mcp_call", json!({"server":"fixture","name":"unlisted"})),
            directory.path(),
        )
        .await
        .unwrap_err();
        assert!(refused.to_string().contains("allowlist"));
    }
}

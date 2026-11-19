use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    rc::Rc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

use super::Access;
use crate::{
    api::ToolCall,
    config::{Config, McpLifetime, McpServer},
    context::clip,
};

const MAX_FRAME: usize = 1024 * 1024;
const MODERN: &str = "2026-07-28";
const OUTPUT_HEADER: &str = "Untrusted MCP tool output\n";
pub(crate) const UNKNOWN_CALL: &str = "MCP outcome unknown: ";

#[derive(Debug)]
struct ServerError(Value);

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "MCP server error: {}", self.0)
    }
}

impl std::error::Error for ServerError {}

type ConnectionKey = (String, String, PathBuf);
type ConnectionSlot = Rc<tokio::sync::Mutex<Option<Client>>>;

#[derive(Default)]
pub(crate) struct Connections {
    clients: RefCell<BTreeMap<ConnectionKey, ConnectionSlot>>,
    refreshed: Option<RefCell<BTreeMap<ConnectionKey, BTreeMap<String, bool>>>>,
}

impl Connections {
    pub(crate) fn new(resumed: bool) -> Self {
        Self {
            refreshed: resumed.then(RefCell::default),
            ..Self::default()
        }
    }

    pub(crate) fn clear(&self) {
        self.clients.borrow_mut().clear();
    }

    pub(crate) fn model_turn(&self, actor: &str, workspace: &Path) {
        if let Some(refreshed) = &self.refreshed {
            for ((_, owner, directory), tools) in refreshed.borrow_mut().iter_mut() {
                if owner == actor && directory == workspace {
                    for visible in tools.values_mut() {
                        *visible = true;
                    }
                }
            }
        }
    }

    pub(crate) fn close_worker(&self, actor: &str) {
        self.clients
            .borrow_mut()
            .retain(|(_, owner, _), _| owner != actor);
    }
}

#[derive(Deserialize)]
struct Request {
    server: String,
    name: Option<String>,
    query: Option<String>,
    offset: Option<usize>,
    #[serde(default)]
    arguments: Map<String, Value>,
}

enum Operation<'a> {
    Call(&'a str, &'a Map<String, Value>),
    List {
        name: Option<&'a str>,
        offset: usize,
    },
    Search {
        query: &'a str,
        offset: usize,
    },
}

#[derive(Debug)]
enum Output {
    Response(Value),
    Names {
        names: Vec<String>,
        offset: usize,
    },
    Schemas {
        tools: Vec<Value>,
        offset: usize,
        match_kind: &'static str,
    },
}

fn available(server: &McpServer, access: Access) -> bool {
    matches!(access, Access::Work) || !server.inspection_tools.is_empty()
}

fn permitted(server: &McpServer, access: Access, name: &str) -> bool {
    server.permits(name)
        && (matches!(access, Access::Work) || server.inspection_tools.contains(name))
}

pub(crate) fn with_instructions<'a>(
    system: &'a str,
    servers: &BTreeMap<String, McpServer>,
    access: Access,
) -> Cow<'a, str> {
    let mut prompt = Cow::Borrowed(system);
    for (name, server) in servers {
        if available(server, access)
            && let Some(instructions) = server.instructions.as_deref().map(str::trim)
            && !instructions.is_empty()
        {
            prompt
                .to_mut()
                .push_str(&format!("\nMCP {name}: {instructions}"));
        }
    }
    prompt
}

pub(crate) fn schemas(servers: &BTreeMap<String, McpServer>, access: Access) -> Vec<Value> {
    let available = servers
        .iter()
        .filter(|(_, server)| available(server, access))
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    if available.is_empty() {
        return Vec::new();
    }
    let server = json!({"type":"string","enum":available});
    vec![
        super::schema(
            "mcp_list",
            "List permitted MCP tool names. Continue with next_offset. Use tool_search for schemas.",
            json!({"server":server, "offset":{"type":"integer","minimum":0}}),
            &["server"],
        ),
        super::schema(
            "tool_search",
            "Fetch current MCP tool schemas by exact name, substring, or fuzzy match. Continue with next_offset.",
            json!({"server":server, "query":{"type":"string"}, "offset":{"type":"integer","minimum":0}}),
            &["server", "query"],
        ),
        super::schema(
            "mcp_call",
            "Call a permitted MCP tool by exact name. Fetch its schema with tool_search.",
            json!({"server":server, "name":{"type":"string"}, "arguments":{"type":"object"}}),
            &["server", "name"],
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch(
    sessions: &Connections,
    actor: &str,
    servers: &BTreeMap<String, McpServer>,
    access: Access,
    call: &ToolCall,
    workspace: &Path,
    time_limit: Duration,
    output_limit: usize,
    key: Option<&str>,
) -> (String, String) {
    let same = |text: String| (text.clone(), text);
    let call_started = Cell::new(false);
    let (visible, full) = match timeout(
        time_limit,
        run(
            sessions,
            actor,
            servers,
            access,
            call,
            workspace,
            &call_started,
        ),
    )
    .await
    {
        Ok(Ok((_, Output::Response(value)))) => same(format!("{OUTPUT_HEADER}{value}")),
        Ok(Ok((_, Output::Names { names, offset }))) => {
            let page = output_page(&names, offset, output_limit);
            let omitted = page.is_empty() && offset < names.len();
            let next = offset + page.len() + usize::from(omitted);
            let next_offset = (next < names.len()).then_some(next);
            let mut visible =
                json!({"tool_names":page,"total":names.len(),"next_offset":next_offset});
            if omitted {
                visible["omitted"] = json!(1);
            }
            let full = if offset == 0 {
                json!({"tool_names":names,"total":names.len(),"next_offset":null})
            } else if omitted {
                json!({"tool_names":[names[offset]],"total":names.len(),"next_offset":next_offset})
            } else {
                visible.clone()
            };
            (
                format!("{OUTPUT_HEADER}{visible}"),
                format!("{OUTPUT_HEADER}{full}"),
            )
        }
        Ok(Ok((
            key,
            Output::Schemas {
                tools,
                offset,
                match_kind,
            },
        ))) => {
            let page = output_page(&tools, offset, output_limit);
            let next = offset + page.len();
            if page.is_empty() && offset < tools.len() {
                let next_offset = (offset + 1 < tools.len()).then_some(offset + 1);
                let tool = &tools[offset];
                let visible = json!({
                    "match":match_kind,"tools":[],"tool_names":[tool["name"]],"total":tools.len(),
                    "next_offset":next_offset,"error":"Schema exceeds tool output budget. See full tool audit."
                });
                let full = json!({"match":match_kind,"tools":[tool],"total":tools.len(),"next_offset":next_offset});
                (
                    format!("{OUTPUT_HEADER}{visible}"),
                    format!("{OUTPUT_HEADER}{full}"),
                )
            } else {
                if let Some(refreshed) = &sessions.refreshed {
                    let mut refreshed = refreshed.borrow_mut();
                    let known = refreshed.entry(key).or_default();
                    for name in page.iter().filter_map(|tool| tool["name"].as_str()) {
                        known.entry(name.into()).or_insert(false);
                    }
                }
                let next_offset = (next < tools.len()).then_some(next);
                let listing = json!({"match":match_kind,"tools":page,"total":tools.len(),"next_offset":next_offset});
                same(format!("{OUTPUT_HEADER}{listing}"))
            }
        }
        Ok(Err(error)) if error.to_string().starts_with("REFUSED") => same(format!("{error:#}")),
        Ok(Err(error)) => {
            sessions
                .clients
                .borrow_mut()
                .retain(|(_, owner, path), _| owner != actor || path != workspace);
            if error.to_string().starts_with(UNKNOWN_CALL) {
                same(format!("{error:#}"))
            } else {
                same(format!("Tool error: {error:#}"))
            }
        }
        Err(_) => {
            sessions
                .clients
                .borrow_mut()
                .retain(|(_, owner, path), _| owner != actor || path != workspace);
            if call_started.get() {
                same(format!(
                    "{UNKNOWN_CALL}server timed out after possible dispatch"
                ))
            } else {
                same("Tool error: MCP server timed out".into())
            }
        }
    };
    let redact = |output: String| match key {
        Some(key) => output.replace(key, "[redacted]"),
        None => output,
    };
    (clip(&redact(visible), output_limit), redact(full))
}

async fn run(
    sessions: &Connections,
    actor: &str,
    servers: &BTreeMap<String, McpServer>,
    access: Access,
    call: &ToolCall,
    workspace: &Path,
    call_started: &Cell<bool>,
) -> Result<(ConnectionKey, Output)> {
    ensure!(call.kind == "function", "unsupported tool call type");
    ensure!(
        call.function.arguments.len() <= 128 * 1024,
        "MCP call arguments too large"
    );
    let input: Request = crate::api::tool_arguments(&call.function.arguments)?;
    let server = servers
        .get(&input.server)
        .context("unknown configured MCP server")?;
    ensure!(
        available(server, access),
        "REFUSED: MCP server {:?} has no tools configured for inspection",
        input.server
    );
    if let Some(name) = input.name.as_deref() {
        ensure!(!name.trim().is_empty(), "MCP tool name is empty");
        ensure!(
            permitted(server, access, name),
            "REFUSED: MCP tool {name:?} is blocked by the configured tool policy for this phase. Use mcp_list for available tools."
        );
    }
    let key = (input.server.clone(), actor.into(), workspace.to_owned());
    let operation = match call.function.name.as_str() {
        "mcp_call" => {
            let name = input.name.as_deref().context("mcp_call requires name")?;
            ensure!(
                sessions.refreshed.as_ref().is_none_or(|refreshed| {
                    refreshed
                        .borrow()
                        .get(&key)
                        .and_then(|tools| tools.get(name))
                        == Some(&true)
                }),
                "REFUSED: Fetch the current schema with tool_search before calling {name:?} after restart."
            );
            Operation::Call(name, &input.arguments)
        }
        "mcp_list" => Operation::List {
            name: input.name.as_deref(),
            offset: input.offset.unwrap_or(0),
        },
        "tool_search" => {
            let query = input
                .query
                .as_deref()
                .map(str::trim)
                .filter(|query| !query.is_empty())
                .context("tool_search requires a nonempty query")?;
            Operation::Search {
                query,
                offset: input.offset.unwrap_or(0),
            }
        }
        _ => anyhow::bail!("unsupported MCP operation"),
    };
    let result = if server.lifetime == McpLifetime::Worker {
        let slot = sessions
            .clients
            .borrow_mut()
            .entry(key.clone())
            .or_default()
            .clone();
        let mut connection = slot.lock().await;
        if connection.is_none() {
            *connection = Some(Client::open(server, workspace).await?);
        }
        invoke(
            connection.as_mut().expect("client initialized"),
            server,
            access,
            operation,
            call_started,
        )
        .await
    } else {
        let mut client = Client::open(server, workspace).await?;
        invoke(&mut client, server, access, operation, call_started).await
    }?;
    Ok((key, result))
}

async fn invoke(
    client: &mut Client,
    server: &McpServer,
    access: Access,
    operation: Operation<'_>,
    call_started: &Cell<bool>,
) -> Result<Output> {
    match operation {
        Operation::Call(name, arguments) => {
            call_started.set(true);
            let result = client
                .rpc("tools/call", json!({"name":name,"arguments":arguments}))
                .await;
            if result
                .as_ref()
                .is_err_and(|error| !error.is::<ServerError>())
            {
                return result.map(Output::Response).with_context(|| {
                    format!("{UNKNOWN_CALL}server failed after possible dispatch")
                });
            }
            result.map(Output::Response)
        }
        Operation::List { name, offset } => {
            let names = discover(client, server, access, name, |_, _| Ok(()))
                .await?
                .into_keys()
                .collect::<Vec<_>>();
            let offset = offset.min(names.len());
            Ok(Output::Names { names, offset })
        }
        Operation::Search { query, offset } => {
            let catalog = discover(client, server, access, None, schema_entry).await?;
            let (match_kind, tools) = search(catalog, query);
            let offset = offset.min(tools.len());
            Ok(Output::Schemas {
                tools,
                offset,
                match_kind,
            })
        }
    }
}

async fn discover<T>(
    client: &mut Client,
    server: &McpServer,
    access: Access,
    name: Option<&str>,
    mut entry: impl FnMut(&str, &Value) -> Result<T>,
) -> Result<BTreeMap<String, T>> {
    let mut selected = BTreeMap::new();
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    loop {
        let params = cursor.map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
        let result = client.rpc("tools/list", params).await?;
        for tool in result["tools"]
            .as_array()
            .context("MCP list result is missing tools")?
        {
            let advertised = tool["name"].as_str().context("MCP tool is missing name")?;
            if permitted(server, access, advertised)
                && name.is_none_or(|requested| requested == advertised)
            {
                selected.insert(advertised.to_owned(), entry(advertised, tool)?);
                if name.is_some() {
                    return Ok(selected);
                }
            }
        }
        cursor = result["nextCursor"].as_str().map(str::to_owned);
        let Some(next) = &cursor else {
            return Ok(selected);
        };
        ensure!(
            seen.insert(next.clone()),
            "MCP tools/list repeated a pagination cursor"
        );
    }
}

fn schema_entry(name: &str, tool: &Value) -> Result<Value> {
    ensure!(
        tool["inputSchema"].is_object(),
        "MCP tool {name:?} is missing an input schema"
    );
    Ok(json!({"name":name,"description":tool["description"],"inputSchema":tool["inputSchema"]}))
}

fn search(mut catalog: BTreeMap<String, Value>, query: &str) -> (&'static str, Vec<Value>) {
    if let Some(tool) = catalog.remove(query) {
        return ("exact", vec![tool]);
    }
    let query = query.to_lowercase();
    let candidates = catalog
        .into_iter()
        .map(|(name, tool)| (name.to_lowercase(), tool))
        .collect::<Vec<_>>();
    if candidates.iter().any(|(name, _)| name.contains(&query)) {
        let mut matches = candidates
            .into_iter()
            .filter(|(name, _)| name.contains(&query))
            .collect::<Vec<_>>();
        matches.sort_by_key(|(name, _)| (name != &query, !name.starts_with(&query)));
        return (
            "substring",
            matches.into_iter().map(|(_, tool)| tool).collect(),
        );
    }

    let query = query.chars().take(129).collect::<Vec<_>>();
    if query.len() > 128 {
        return ("none", Vec::new());
    }
    let limit = (query.len() / 3).clamp(1, 4);
    let mut matches = candidates
        .into_iter()
        .filter_map(|(name, tool)| {
            std::iter::once(name.as_str())
                .chain(name.split(['_', '-', '.']))
                .filter(|part| !part.is_empty())
                .filter_map(|part| edit_distance(&query, part, limit))
                .min()
                .map(|distance| (distance, tool))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(distance, _)| *distance);
    let kind = if matches.is_empty() { "none" } else { "fuzzy" };
    (kind, matches.into_iter().map(|(_, tool)| tool).collect())
}

fn edit_distance(query: &[char], candidate: &str, limit: usize) -> Option<usize> {
    let candidate = candidate
        .chars()
        .take(query.len() + limit + 1)
        .collect::<Vec<_>>();
    if query.len().abs_diff(candidate.len()) > limit {
        return None;
    }
    let mut row = (0..=query.len()).collect::<Vec<_>>();
    for (index, character) in candidate.into_iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = index + 1;
        for (column, &expected) in query.iter().enumerate() {
            let above = row[column + 1];
            row[column + 1] = (above + 1)
                .min(row[column] + 1)
                .min(diagonal + usize::from(character != expected));
            diagonal = above;
        }
    }
    let distance = row[query.len()];
    (distance <= limit).then_some(distance)
}

fn output_page<T: Serialize>(items: &[T], offset: usize, output_limit: usize) -> &[T] {
    let budget = output_limit.saturating_sub(OUTPUT_HEADER.len());
    let mut remaining = budget.saturating_sub(128);
    let mut next = offset;
    for item in &items[offset..] {
        let cost = serde_json::to_vec(item)
            .expect("serializable MCP catalog entry")
            .len()
            + 1;
        if cost > remaining {
            break;
        }
        remaining -= cost;
        next += 1;
    }
    &items[offset..next]
}

pub async fn check(config: &Config, name: &str) -> Result<Value> {
    let server = config
        .mcp_servers
        .get(name)
        .context("unknown configured MCP server")?;
    let listing = timeout(
        Duration::from_secs(config.orchestration.command_timeout_secs),
        async {
            let mut client = Client::open(server, &config.cwd).await?;
            discover(&mut client, server, Access::Work, None, schema_entry).await
        },
    )
    .await
    .context("MCP check timed out")??;
    for tool in server
        .tools
        .iter()
        .flatten()
        .chain(&server.inspection_tools)
    {
        ensure!(
            listing.contains_key(tool),
            "configured MCP tool {tool:?} was not advertised"
        );
    }
    Ok(json!({"tools":listing.into_values().collect::<Vec<_>>() }))
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
                return Err(ServerError(value["error"].clone()).into());
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

    #[test]
    fn searches_exact_names_then_substrings_then_close_spellings() {
        let catalog = ["get_segments", "segments", "segments_info", "inspect"]
            .into_iter()
            .map(|name| (name.to_owned(), json!({"name":name})))
            .collect::<BTreeMap<_, _>>();
        let find = |query| {
            let (kind, tools) = search(catalog.clone(), query);
            (
                kind,
                tools
                    .into_iter()
                    .map(|tool| tool["name"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(find("segments"), ("exact", vec!["segments".to_owned()]));
        assert_eq!(
            find("SEGMENT"),
            (
                "substring",
                ["segments", "segments_info", "get_segments"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(find("segmetns").0, "fuzzy");
        assert!(find("segmetns").1.contains(&"get_segments".to_owned()));
        assert_eq!(find("unrelated_xyz"), ("none", Vec::new()));
    }

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
            tools: Some(BTreeSet::from(["query".into()])),
            allow_all: false,
            deny_tools: None,
            inspection_tools: BTreeSet::from(["query".into()]),
            instructions: None,
            protocol: "2025-11-25".into(),
            lifetime: McpLifetime::Call,
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
            &Connections::default(),
            "fixture",
            &servers,
            Access::Inspect,
            &call("mcp_list", json!({"server":"fixture"})),
            directory.path(),
            Duration::from_secs(5),
            1024,
            None,
        )
        .await
        .0;
        assert!(listing.contains("query"), "{listing}");
        assert!(!listing.contains("unlisted"));
        assert!(!listing.contains("inputSchema"));
        let refused = run(
            &Connections::default(),
            "fixture",
            &servers,
            Access::Inspect,
            &call("mcp_call", json!({"server":"fixture","name":"unlisted"})),
            directory.path(),
            &Cell::new(false),
        )
        .await
        .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("blocked by the configured tool policy")
        );
    }

    #[tokio::test]
    async fn worker_sessions_reuse_process_state_and_reset_at_the_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let server = McpServer {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), r#"
                IFS= read -r frame
                printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}'
                IFS= read -r frame
                id=1
                while IFS= read -r frame; do
                    case "$frame" in *tools/call*) ;; *) exit 1 ;; esac
                    id=$((id + 1))
                    printf '{"jsonrpc":"2.0","id":%s,"result":{"count":%s}}\n' "$id" "$id"
                done
            "#.into()],
            tools: Some(BTreeSet::from(["query".into()])),
            allow_all: false,
            deny_tools: None,
            inspection_tools: BTreeSet::from(["query".into()]),
            instructions: None,
            protocol: "2025-11-25".into(),
            lifetime: McpLifetime::Worker,
        };
        let servers = BTreeMap::from([("fixture".into(), server)]);
        let sessions = Connections::default();
        let call = ToolCall {
            id: "fixture".into(),
            kind: "function".into(),
            function: Function {
                name: "mcp_call".into(),
                arguments: json!({"server":"fixture","name":"query"}).to_string(),
            },
        };
        for (actor, expected) in [("worker_1", 2), ("worker_1", 3), ("worker_2", 2)] {
            let (_, Output::Response(result)) = run(
                &sessions,
                actor,
                &servers,
                Access::Inspect,
                &call,
                directory.path(),
                &Cell::new(false),
            )
            .await
            .unwrap() else {
                panic!("expected a tool result")
            };
            assert_eq!(result["count"], expected);
        }
        sessions.clear();
        let (_, Output::Response(result)) = run(
            &sessions,
            "worker_1",
            &servers,
            Access::Inspect,
            &call,
            directory.path(),
            &Cell::new(false),
        )
        .await
        .unwrap() else {
            panic!("expected a tool result")
        };
        assert_eq!(result["count"], 2);
    }

    #[tokio::test]
    async fn unrestricted_and_denied_tools_share_discovery_and_dispatch_policy() {
        let directory = tempfile::tempdir().unwrap();
        let server = McpServer {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), r#"
                IFS= read -r frame
                printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}'
                IFS= read -r frame
                id=1
                page=0
                while IFS= read -r frame; do
                    id=$((id + 1))
                    case "$frame" in
                        *tools/list*)
                            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"inspect_%03d","inputSchema":{"type":"object"}}]' "$id" "$page"
                            page=$((page + 1))
                            if [ "$page" -lt 100 ]; then printf ',"nextCursor":"%s"' "$page"; fi
                            printf '}}\n';;
                        *tools/call*)
                            printf '{"jsonrpc":"2.0","id":%s,"result":{"called":true}}\n' "$id";;
                        *) exit 1;;
                    esac
                done
            "#.into()],
            tools: None,
            allow_all: true,
            deny_tools: None,
            inspection_tools: BTreeSet::from(["inspect_098".into()]),
            instructions: None,
            protocol: "2025-11-25".into(),
            lifetime: McpLifetime::Call,
        };
        let excluded = "inspect_099";
        for deny in [false, true] {
            let mut server = server.clone();
            if deny {
                server.allow_all = false;
                server.deny_tools = Some(BTreeSet::from([excluded.into()]));
            }
            let servers = BTreeMap::from([("fixture".into(), server)]);
            let execute = |access, tool: &str, arguments: Value| {
                let call = ToolCall {
                    id: "fixture".into(),
                    kind: "function".into(),
                    function: Function {
                        name: tool.into(),
                        arguments: arguments.to_string(),
                    },
                };
                let servers = &servers;
                let workspace = directory.path();
                async move {
                    dispatch(
                        &Connections::default(),
                        "worker_1",
                        servers,
                        access,
                        &call,
                        workspace,
                        Duration::from_secs(5),
                        512,
                        None,
                    )
                    .await
                }
            };
            let (first, full) =
                execute(Access::Work, "mcp_list", json!({"server":"fixture"})).await;
            let json_from = |output: &str| {
                serde_json::from_str::<Value>(output.strip_prefix(OUTPUT_HEADER).unwrap()).unwrap()
            };
            assert!(!first.contains("inputSchema"));
            assert!(!full.contains("inputSchema"));
            let first = json_from(&first);
            let expected = if deny { 99 } else { 100 };
            assert_eq!(
                json_from(&full)["tool_names"].as_array().unwrap().len(),
                expected
            );
            let mut names = Vec::new();
            let mut page = first;
            loop {
                names.extend_from_slice(page["tool_names"].as_array().unwrap());
                let Some(offset) = page["next_offset"].as_u64() else {
                    break;
                };
                let (visible, _) = execute(
                    Access::Work,
                    "mcp_list",
                    json!({"server":"fixture","offset":offset}),
                )
                .await;
                page = json_from(&visible);
            }
            assert_eq!(names.len(), expected);
            assert_eq!(names.contains(&json!(excluded)), !deny);
            let (schema, _) = execute(
                Access::Inspect,
                "tool_search",
                json!({"server":"fixture","query":"SPECT"}),
            )
            .await;
            assert_eq!(
                json_from(&schema)["tools"][0]["inputSchema"]["type"],
                "object"
            );
            assert_eq!(json_from(&schema)["match"], "substring");
            assert_eq!(json_from(&schema)["total"], 1);
            let (schema, _) = execute(
                Access::Inspect,
                "tool_search",
                json!({"server":"fixture","query":"inspec_098"}),
            )
            .await;
            assert_eq!(json_from(&schema)["match"], "fuzzy");
            assert_eq!(json_from(&schema)["tools"][0]["name"], "inspect_098");
            let (called, _) = execute(
                Access::Work,
                "mcp_call",
                json!({"server":"fixture","name":excluded}),
            )
            .await;
            if deny {
                assert!(called.contains("blocked by the configured tool policy"));
            } else {
                assert_eq!(json_from(&called)["called"], true);
            }
            let (inspection, _) =
                execute(Access::Inspect, "mcp_list", json!({"server":"fixture"})).await;
            assert_eq!(json_from(&inspection)["tool_names"], json!(["inspect_098"]));
            let (called, _) = execute(
                Access::Inspect,
                "mcp_call",
                json!({"server":"fixture","name":"inspect_098"}),
            )
            .await;
            assert_eq!(json_from(&called)["called"], true);
            let (refused, _) = execute(
                Access::Inspect,
                "mcp_call",
                json!({"server":"fixture","name":excluded}),
            )
            .await;
            assert!(refused.starts_with("REFUSED"), "{refused}");
        }
    }
}

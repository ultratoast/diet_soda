//! MCP tool client. A connection serializes JSON-RPC exchanges; agents using
//! different servers can proceed concurrently without sharing protocol state.
use crate::{
    config::{expand_env, Config, McpConfig, McpTransport},
    model::ToolSpec,
    provider::SseDecoder,
};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeMap, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct McpTool {
    pub server: String,
    pub original_name: String,
    pub spec: ToolSpec,
}

enum Transport {
    Stdio {
        child: Child,
        input: ChildStdin,
        output: BufReader<ChildStdout>,
        #[cfg(unix)]
        _group: crate::process::ProcessGroup,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: BTreeMap<String, String>,
        session_id: Option<String>,
    },
}
struct Client {
    transport: Transport,
    next_id: u64,
    timeout: u64,
    tools: Vec<McpTool>,
    protocol: String,
    reusable: bool,
}

impl Client {
    async fn connect(
        name: &str,
        config: &McpConfig,
        workspace: &std::path::Path,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let transport = match &config.transport {
            McpTransport::Stdio { command, args, env } => {
                let mut cmd = Command::new(command);
                #[cfg(unix)]
                cmd.process_group(0);
                cmd.args(args)
                    .current_dir(workspace)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                for (key, value) in env {
                    cmd.env(key, expand_env(value)?);
                }
                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("Starting MCP server {name}"))?;
                let input = child.stdin.take().unwrap();
                let output = BufReader::new(child.stdout.take().unwrap());
                #[cfg(unix)]
                let group = crate::process::ProcessGroup(child.id().unwrap());
                Transport::Stdio {
                    child,
                    input,
                    output,
                    #[cfg(unix)]
                    _group: group,
                }
            }
            McpTransport::Http { url, headers } => {
                crate::config::validate_url(url)?;
                Transport::Http {
                    client: reqwest::Client::builder()
                        .timeout(Duration::from_secs(config.timeout_seconds))
                        .build()?,
                    url: url.clone(),
                    headers: headers.clone(),
                    session_id: None,
                }
            }
        };
        let mut client = Self {
            transport,
            next_id: 0,
            timeout: config.timeout_seconds,
            tools: vec![],
            protocol: "2025-03-26".into(),
            reusable: true,
        };
        let result = client.request("initialize", json!({"protocolVersion":client.protocol,"capabilities":{},"clientInfo":{"name":env!("CARGO_PKG_NAME"),"version":env!("CARGO_PKG_VERSION")}}), cancel).await?;
        client.protocol = result["protocolVersion"]
            .as_str()
            .context("MCP initialize omitted protocolVersion")?
            .into();
        if !["2024-11-05", "2025-03-26", "2025-06-18"].contains(&client.protocol.as_str()) {
            bail!("Unsupported MCP protocol {}", client.protocol);
        }
        client
            .exchange(
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                None,
                cancel,
            )
            .await?;
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let params = cursor
                .as_ref()
                .map(|c| json!({"cursor":c}))
                .unwrap_or(json!({}));
            let result = client.request("tools/list", params, cancel).await?;
            for tool in result["tools"]
                .as_array()
                .context("MCP tools/list omitted tools")?
            {
                let original = tool["name"].as_str().context("MCP tool omitted name")?;
                let index = client.tools.len();
                let full = format!("mcp_{name}__{original}");
                let exposed = if full.len() <= 64
                    && full
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    full
                } else {
                    format!("mcp_{name}_{index}")
                };
                if client.tools.iter().any(|t| t.spec.name == exposed) {
                    bail!("MCP tool name collision: {exposed}");
                }
                let input_schema = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or(json!({"type":"object"}));
                jsonschema::validator_for(&input_schema)
                    .map_err(|e| anyhow::anyhow!("MCP input schema: {e}"))?;
                client.tools.push(McpTool {
                    server: name.into(),
                    original_name: original.into(),
                    spec: ToolSpec {
                        name: exposed,
                        description: format!(
                            "[MCP {name}/{original}] {}",
                            tool["description"].as_str().unwrap_or("")
                        ),
                        input_schema,
                    },
                });
            }
            cursor = result["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                return Ok(client);
            }
        }
        bail!("MCP tools/list exceeded pagination limit")
    }
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let result = self
            .exchange(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                Some(id),
                cancel,
            )
            .await?;
        if let Some(error) = result.get("error") {
            bail!("MCP error: {error}");
        }
        Ok(result.get("result").cloned().unwrap_or(Value::Null))
    }
    async fn exchange(
        &mut self,
        message: Value,
        id: Option<u64>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let timeout = self.timeout;
        let work = async {
            match &mut self.transport {
                Transport::Stdio { input, output, .. } => {
                    input.write_all(&serde_json::to_vec(&message)?).await?;
                    input.write_all(b"\n").await?;
                    input.flush().await?;
                    if id.is_none() {
                        return Ok(Value::Null);
                    }
                    loop {
                        let line = read_line_bounded(output).await?;
                        let response: Value = serde_json::from_slice(&line)
                            .context("MCP stdout must contain JSON-RPC messages only")?;
                        if response.get("method").is_some() {
                            if let Some(server_id) = response.get("id") {
                                let reply = if response["method"] == "ping" {
                                    json!({"jsonrpc":"2.0","id":server_id,"result":{}})
                                } else {
                                    json!({"jsonrpc":"2.0","id":server_id,"error":{"code":-32601,"message":"Client capability not supported"}})
                                };
                                input.write_all(&serde_json::to_vec(&reply)?).await?;
                                input.write_all(b"\n").await?;
                                input.flush().await?;
                            }
                            continue;
                        }
                        if response["id"].as_u64() == id {
                            return Ok(response);
                        }
                    }
                }
                Transport::Http {
                    client,
                    url,
                    headers,
                    session_id,
                } => {
                    let mut request = client
                        .post(url.as_str())
                        .header("Accept", "application/json, text/event-stream")
                        .header("MCP-Protocol-Version", &self.protocol)
                        .json(&message);
                    for (key, value) in headers.iter() {
                        request = request.header(key, expand_env(value)?);
                    }
                    if let Some(session) = session_id.as_ref() {
                        request = request.header("Mcp-Session-Id", session);
                    }
                    let response = request.send().await?;
                    if !response.status().is_success() {
                        bail!("MCP HTTP {}", response.status());
                    }
                    if let Some(session) = response.headers().get("Mcp-Session-Id") {
                        *session_id = Some(session.to_str()?.into());
                    }
                    if id.is_none() {
                        return Ok(Value::Null);
                    }
                    let sse = response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|s| s.contains("text/event-stream"));
                    let mut stream = response.bytes_stream();
                    let mut bytes = vec![];
                    let mut decoder = SseDecoder::default();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk?;
                        if sse {
                            for data in decoder.push(&chunk)? {
                                let value: Value = serde_json::from_str(&data)?;
                                if value["id"].as_u64() == id {
                                    return Ok(value);
                                }
                            }
                        } else {
                            bytes.extend_from_slice(&chunk);
                            if bytes.len() > 2_000_000 {
                                bail!("MCP response exceeds 2 MB");
                            }
                        }
                    }
                    if sse {
                        bail!("MCP stream ended without a response");
                    }
                    let value: Value = serde_json::from_slice(&bytes)?;
                    if value["id"].as_u64() != id {
                        bail!("MCP response ID mismatch");
                    }
                    Ok(value)
                }
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => bail!("Cancelled"),
            result = tokio::time::timeout(Duration::from_secs(timeout), work) => result.context("MCP request timed out")?,
        }
    }
    async fn shutdown(&mut self) {
        #[cfg(unix)]
        if let Transport::Stdio { _group, .. } = &self.transport {
            _group.terminate();
        }
        match &mut self.transport {
            Transport::Stdio { child, .. } => {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            Transport::Http {
                client,
                url,
                headers,
                session_id: Some(session),
            } => {
                let mut request = client
                    .delete(url.as_str())
                    .header("Mcp-Session-Id", session.as_str())
                    .header("MCP-Protocol-Version", &self.protocol);
                for (key, value) in headers {
                    if let Ok(value) = expand_env(value) {
                        request = request.header(key.as_str(), value);
                    }
                }
                let _ = tokio::time::timeout(Duration::from_secs(3), request.send()).await;
            }
            _ => {}
        }
    }
}
async fn read_line_bounded(reader: &mut BufReader<ChildStdout>) -> Result<Vec<u8>> {
    let mut line = vec![];
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            bail!("MCP server closed stdout");
        }
        let count = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(buffer.len());
        let done = buffer[count - 1] == b'\n';
        if line.len() + count > 2_000_000 {
            bail!("MCP message exceeds 2 MB");
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if done {
            return Ok(line);
        }
    }
}

#[derive(Default)]
pub struct McpManager {
    clients: Mutex<BTreeMap<String, Arc<Mutex<Client>>>>,
}
impl McpManager {
    async fn client(
        &self,
        name: &str,
        config: &Config,
        cancel: &CancellationToken,
    ) -> Result<Arc<Mutex<Client>>> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(name) {
            return Ok(client.clone());
        }
        let server = config.mcp_servers.get(name).context("Unknown MCP server")?;
        let client = Arc::new(Mutex::new(
            Client::connect(name, server, &config.workspace, cancel).await?,
        ));
        clients.insert(name.into(), client.clone());
        Ok(client)
    }
    pub async fn tools(
        &self,
        name: &str,
        config: &Config,
        cancel: &CancellationToken,
    ) -> Result<Vec<McpTool>> {
        Ok(self
            .client(name, config, cancel)
            .await?
            .lock()
            .await
            .tools
            .clone())
    }
    pub async fn call(
        &self,
        tool: &McpTool,
        args: Value,
        config: &Config,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let client = self.client(&tool.server, config, cancel).await?;
        let mut connection = tokio::select! {
            _ = cancel.cancelled() => bail!("Cancelled"),
            connection = client.lock() => connection,
        };
        if !connection.reusable {
            connection.shutdown().await;
            *connection = Client::connect(
                &tool.server,
                &config.mcp_servers[&tool.server],
                &config.workspace,
                cancel,
            )
            .await?;
        }
        // Mark before awaiting: even if the caller is dropped, the next borrower
        // reconnects instead of consuming an abandoned response.
        connection.reusable = false;
        let result = connection
            .request(
                "tools/call",
                json!({"name":tool.original_name,"arguments":args}),
                cancel,
            )
            .await;
        connection.reusable = result.is_ok();
        if result.is_err() {
            connection.shutdown().await;
        }
        result
    }
    pub async fn stop(&self, name: &str) {
        let client = self.clients.lock().await.remove(name);
        if let Some(client) = client {
            client.lock().await.shutdown().await;
        }
    }
    pub async fn shutdown(&self) {
        let clients = std::mem::take(&mut *self.clients.lock().await);
        for client in clients.values() {
            client.lock().await.shutdown().await;
        }
    }
}

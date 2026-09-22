//! MCP tool client. A connection serializes JSON-RPC exchanges; agents using
//! different servers can proceed concurrently without sharing protocol state.
use crate::{
    config::{expand_env, Config, McpConfig, McpTransport},
    model::ToolSpec,
    process::{self, EnvRequest},
    provider::SseDecoder,
};
use anyhow::{bail, Context, Result};
use futures_util::{future::join_all, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

/// Maximum bytes of stdio server stderr retained for diagnostics.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// How long a failed connect is remembered before the next call retries.
const CONNECT_FAILURE_TTL: Duration = Duration::from_secs(5);

/// Maximum length of an exposed MCP tool name, in bytes.
const MAX_TOOL_NAME_BYTES: usize = 64;

/// Deterministic fallback name for an MCP tool whose readable name does not fit
/// or is not a valid identifier.
///
/// The name is `mcp_<truncated-server>_<16-hex-FNV1a64>`; the hash covers the
/// server and original tool names joined by a NUL separator. This is stable
/// across `tools/list` ordering and pagination, unlike an index-based fallback,
/// while remaining within the 64-byte tool-name limit.
fn fallback_tool_name(server: &str, tool: &str) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    let mut feed = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    };
    for byte in server.bytes() {
        feed(byte);
    }
    feed(0);
    for byte in tool.bytes() {
        feed(byte);
    }
    let hash = format!("{hash:016x}");
    let prefix = "mcp_";
    let max_server = MAX_TOOL_NAME_BYTES - prefix.len() - 1 - hash.len();
    // Sanitize to ASCII before truncating: every other character becomes `_`.
    // This guarantees the component is valid and each character is one byte, so
    // the byte budget is never exceeded even for all-non-ASCII server names.
    let server: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(max_server)
        .collect();
    format!("{prefix}{server}_{hash}")
}

#[derive(Clone)]
pub struct McpTool {
    pub server: String,
    pub original_name: String,
    pub spec: ToolSpec,
}

/// Bounded capture of a stdio server's stderr for error diagnostics.
///
/// The drain task appends without holding the lock across an await; decoding is
/// lossy UTF-8 and only happens when an error needs context.
#[derive(Default)]
struct StderrTail {
    bytes: std::sync::Mutex<Vec<u8>>,
}

impl StderrTail {
    /// Appends a chunk, discarding older bytes beyond the 8 KiB cap.
    fn push(&self, chunk: &[u8]) {
        let Ok(mut bytes) = self.bytes.lock() else {
            return;
        };
        bytes.extend_from_slice(chunk);
        if bytes.len() > STDERR_TAIL_BYTES {
            let excess = bytes.len() - STDERR_TAIL_BYTES;
            bytes.drain(..excess);
        }
    }

    /// Returns the tail as lossy UTF-8, or `None` when it is empty.
    fn text(&self) -> Option<String> {
        let Ok(bytes) = self.bytes.lock() else {
            return None;
        };
        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }
}

/// Drains a child's stderr to EOF on a detached task so the pipe can never
/// block the child. The task ends when the child closes stderr, including on
/// kill/shutdown, so it does not outlive the process.
fn spawn_stderr_drain(stderr: ChildStderr, tail: Arc<StderrTail>) {
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => tail.push(&buf[..n]),
            }
        }
    });
}

enum Transport {
    Stdio {
        // Boxed so the enum stays small; the Http variant is much lighter.
        child: Box<Child>,
        input: ChildStdin,
        output: BufReader<ChildStdout>,
        stderr_tail: Arc<StderrTail>,
        // Retained for the full connection lifetime so dropping the transport
        // tears down the child's process tree: a Unix process group or a
        // kill-on-close Windows job object. Exactly one exists per platform.
        #[cfg(unix)]
        _group: crate::process::ProcessGroup,
        #[cfg(windows)]
        _job: crate::winjob::JobObject,
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
                let isolated = process::isolated_env(&EnvRequest::custom(env.clone()), workspace)?;
                let mut cmd = Command::new(command);
                #[cfg(unix)]
                cmd.process_group(0);
                cmd.args(args)
                    .current_dir(workspace)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                cmd.env_clear();
                for (key, value) in &isolated {
                    cmd.env(key, value);
                }
                // Windows has no process group; start the child suspended so it
                // can be assigned to a job before running. This must be the last
                // command mutation before spawn.
                #[cfg(windows)]
                crate::winjob::prepare_command(&mut cmd);
                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("Starting MCP server {name}"))?;
                // Assign the still-suspended Windows child to a kill-on-close
                // job before any pipe is touched. `assign` resumes the child and
                // guarantees no suspended child survives a setup error. The Unix
                // process group is kept instead.
                #[cfg(windows)]
                let _job = crate::winjob::JobObject::assign(&mut child)?;
                let input = child.stdin.take().unwrap();
                let output = BufReader::new(child.stdout.take().unwrap());
                let stderr_tail = Arc::new(StderrTail::default());
                spawn_stderr_drain(child.stderr.take().unwrap(), stderr_tail.clone());
                #[cfg(unix)]
                let group = crate::process::ProcessGroup(child.id().unwrap());
                Transport::Stdio {
                    child: Box::new(child),
                    input,
                    output,
                    stderr_tail,
                    #[cfg(unix)]
                    _group: group,
                    #[cfg(windows)]
                    _job,
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
                let full = format!("mcp_{name}__{original}");
                let exposed = if full.len() <= MAX_TOOL_NAME_BYTES
                    && full
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    full
                } else {
                    fallback_tool_name(name, original)
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
        let stderr_tail = match &self.transport {
            Transport::Stdio { stderr_tail, .. } => Some(stderr_tail.clone()),
            Transport::Http { .. } => None,
        };
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
        let outcome = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow::anyhow!("Cancelled")),
            result = tokio::time::timeout(Duration::from_secs(timeout), work) => match result {
                Ok(inner) => inner,
                Err(_) => Err(anyhow::anyhow!("MCP request timed out")),
            },
        };
        match stderr_tail {
            Some(tail) => outcome.map_err(|error| match tail.text() {
                Some(text) => error.context(format!("MCP server stderr:\n{text}")),
                None => error,
            }),
            None => outcome,
        }
    }
    async fn shutdown(&mut self) {
        #[cfg(unix)]
        if let Transport::Stdio { _group, .. } = &self.transport {
            _group.terminate();
        }
        #[cfg(windows)]
        if let Transport::Stdio { _job, .. } = &self.transport {
            // Best-effort: a failed job termination must not skip the direct
            // child kill/wait below.
            let _ = _job.terminate();
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
    // One async gate per server name. Gates are created lazily and never
    // removed, so an in-flight connect cannot be orphaned by a concurrent
    // stop/restart: both contend on the same gate instance.
    gates: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    // Short negative cache of the last connect/initialize/tools-list failure
    // per server, keyed to its expiry. Call/runtime failures after a successful
    // connection are never stored here.
    failures: Mutex<BTreeMap<String, (String, Instant)>>,
    // Set for the duration of a full `shutdown`, so a connect racing the
    // snapshot/stop window fails transiently instead of inserting a client the
    // shutdown would miss. Never held across an await; it only gates connect.
    shutting_down: AtomicBool,
}
impl McpManager {
    /// Returns the shared per-name gate, creating it on first use.
    async fn gate(&self, name: &str) -> Arc<Mutex<()>> {
        self.gates
            .lock()
            .await
            .entry(name.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
    /// Returns the cached connect error for `name` while it is still fresh.
    async fn cached_failure(&self, name: &str) -> Option<String> {
        let failures = self.failures.lock().await;
        failures
            .get(name)
            .and_then(|(error, expiry)| (*expiry > Instant::now()).then(|| error.clone()))
    }
    /// Records a connect failure, replacing any older entry for the server.
    async fn record_failure(&self, name: &str, error: String) {
        self.failures
            .lock()
            .await
            .insert(name.into(), (error, Instant::now() + CONNECT_FAILURE_TTL));
    }
    /// Drops the cached connect failure for `name`, if any.
    async fn clear_failure(&self, name: &str) {
        self.failures.lock().await.remove(name);
    }
    async fn client(
        &self,
        name: &str,
        config: &Config,
        cancel: &CancellationToken,
    ) -> Result<Arc<Mutex<Client>>> {
        // Refuse to start a connect while a full shutdown is snapshotting and
        // stopping servers: a client inserted in that window could outlive the
        // shutdown. The map lock is only held for the lookup below.
        if self.shutting_down.load(Ordering::Acquire) {
            bail!("MCP manager is shutting down; retry shortly");
        }
        // Fast path: the clients map lock is held only for lookup.
        if let Some(client) = self.clients.lock().await.get(name) {
            return Ok(client.clone());
        }
        // A recent connect failure is returned immediately, before contending
        // on the gate, so a downed server costs one attempt per TTL.
        if let Some(error) = self.cached_failure(name).await {
            bail!("{error}");
        }
        // Serialize connection attempts per server name so concurrent callers
        // share one connection instead of racing to spawn duplicates.
        let gate = self.gate(name).await;
        let _guard = gate.lock().await;
        // Re-check under the gate: shutdown may have started while we waited.
        if self.shutting_down.load(Ordering::Acquire) {
            bail!("MCP manager is shutting down; retry shortly");
        }
        // Another caller may have connected while we waited for the gate.
        if let Some(client) = self.clients.lock().await.get(name) {
            return Ok(client.clone());
        }
        // Another caller may have failed while we waited for the gate; honor
        // that fresh failure instead of retrying immediately.
        if let Some(error) = self.cached_failure(name).await {
            bail!("{error}");
        }
        let server = config.mcp_servers.get(name).context("Unknown MCP server")?;
        let client = match Client::connect(name, server, &config.workspace, cancel).await {
            Ok(client) => Arc::new(Mutex::new(client)),
            Err(error) => {
                // A cancelled attempt is not evidence the server is down, so it
                // must not poison the negative cache for later callers.
                if !cancel.is_cancelled() {
                    self.record_failure(name, format!("{error:#}")).await;
                }
                return Err(error);
            }
        };
        // A successful connect clears any stale failure and becomes the shared
        // entry; the map lock is only held for the insert.
        self.clear_failure(name).await;
        self.clients
            .lock()
            .await
            .insert(name.into(), client.clone());
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
        // Execution-path guard: an MCP tool that was not advertised by the
        // server must not reach `tools/call`. The dispatch layer is the
        // authoritative source for advertised tools; this check is the
        // defense-in-depth equivalent so a tool name the model invents
        // cannot hit the server even if a caller passes a bogus spec.
        if !connection
            .tools
            .iter()
            .any(|t| t.original_name == tool.original_name)
        {
            bail!(
                "Unknown tool `{}` for MCP server `{}`; only advertised tools can be invoked",
                tool.original_name,
                tool.server
            );
        }
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
        // Take the same gate a connect would, so a concurrent in-flight
        // connect cannot insert a client that outlives this stop.
        let gate = self.gate(name).await;
        let _guard = gate.lock().await;
        // An explicit stop/restart is a request to retry, so the negative
        // cache entry is dropped here rather than waiting out its TTL.
        self.clear_failure(name).await;
        let client = self.clients.lock().await.remove(name);
        if let Some(client) = client {
            client.lock().await.shutdown().await;
        }
    }
    pub async fn shutdown(&self) {
        // Block new connects for the whole snapshot/stop window. Release pairs
        // with the Acquire loads in `client`, so a connect that sees `true`
        // also sees the shutdown's earlier state. Reset at the end keeps the
        // manager reusable after `/reload`.
        self.shutting_down.store(true, Ordering::Release);
        // Snapshot every known server's gate without holding either map lock
        // across an await. Gates are never removed, so the cloned handles stay
        // valid for the whole shutdown and still serialize against in-flight
        // connects and concurrent stops.
        let servers: Vec<(String, Arc<Mutex<()>>)> = self
            .gates
            .lock()
            .await
            .iter()
            .map(|(name, gate)| (name.clone(), gate.clone()))
            .collect();
        // Stop every server concurrently instead of one at a time. Each task
        // takes the same per-name gate a connect would, so an in-flight connect
        // finishes before its client is removed, then shuts it down. Best-effort:
        // failures are ignored and one slow server cannot delay the others.
        join_all(servers.into_iter().map(|(name, gate)| async move {
            let _guard = gate.lock().await;
            // Full shutdown also clears the negative cache so a later explicit
            // restart retries instead of replaying an old failure.
            self.clear_failure(&name).await;
            let client = self.clients.lock().await.remove(&name);
            if let Some(client) = client {
                client.lock().await.shutdown().await;
            }
        }))
        .await;
        self.shutting_down.store(false, Ordering::Release);
    }
}

//! Shared test server and fixtures. The server writes a deterministic HTTP/1.1
//! response per request and exposes richer [`Reply`] variants so individual
//! tests can pre-stage header delays, per-chunk delays, and stalls without
//! changing the simple path the rest of the suite relies on.
#![allow(dead_code)]
use diet_soda::{
    config::{Config, ProviderConfig, ProviderKind},
    engine::Engine,
    model::UiEvent,
    session::Session,
};
use serde_json::{json, Value};
use std::fmt::Write;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, Mutex},
    task::JoinHandle,
    time::sleep,
};

#[derive(Debug)]
pub struct Request {
    pub headers: String,
    pub body: String,
}
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub headers: Vec<(String, String)>,
    /// Delay before sending the response headers. Use to drive header-timeout
    /// tests against the provider's streaming deadline.
    pub header_delay: Option<std::time::Duration>,
    /// Delay inserted between consecutive body chunks. The split is the same
    /// 7-byte chunks the simple path uses, so the simple path is the
    /// zero-delay default.
    pub chunk_delay: Option<std::time::Duration>,
    /// After writing the headers (and optionally the body), wait this long
    /// before closing the socket. Used to test idle-timeout enforcement on
    /// streams that hang after partial output.
    pub stall: Option<std::time::Duration>,
}
impl Reply {
    pub fn json(body: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            body: body.to_string(),
            headers: vec![],
            header_delay: None,
            chunk_delay: None,
            stall: None,
        }
    }
    pub fn sse(events: Vec<Value>, done: bool) -> Self {
        let mut body = String::new();
        for event in events {
            write!(body, "data: {event}\r\n\r\n").unwrap();
        }
        if done {
            body.push_str("data: [DONE]\r\n\r\n");
        }
        Self {
            status: 200,
            content_type: "text/event-stream".into(),
            body,
            headers: vec![],
            header_delay: None,
            chunk_delay: None,
            stall: None,
        }
    }
}
pub struct Server {
    pub url: String,
    pub requests: mpsc::UnboundedReceiver<Request>,
    pub count: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub async fn server(replies: Vec<Reply>) -> Server {
    server_with_gate(replies, None).await
}
/// Hold responses 2 and 3 until both requests arrive, proving child concurrency
/// without fragile elapsed-time assertions.
pub async fn parallel_server(replies: Vec<Reply>) -> Server {
    server_with_gate(replies, Some(Arc::new(tokio::sync::Barrier::new(2)))).await
}
async fn server_with_gate(replies: Vec<Reply>, gate: Option<Arc<tokio::sync::Barrier>>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let url = format!("http://{local_addr}");
    let port = local_addr.port();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let (tx, rx) = mpsc::unbounded_channel();
    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = count.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            let replies = replies.clone();
            let count = count_clone.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                // A bounded read budget covers the cancellation case:
                // when a client closes the connection before sending its
                // body, `socket.read()` would otherwise block forever
                // and the runtime would hang on shutdown. A generous
                // 10-second budget keeps the fixture deterministic
                // without spuriously aborting tests with realistic
                // client latency.
                let mut bytes = vec![];
                let mut buffer = [0; 4096];
                let end = loop {
                    let read = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        socket.read(&mut buffer),
                    )
                    .await;
                    let n = match read {
                        Ok(Ok(n)) => n,
                        _ => return, // timeout or EOF: drop the connection
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..end]).to_string();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + length {
                    let read = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        socket.read(&mut buffer),
                    )
                    .await;
                    let n = match read {
                        Ok(Ok(n)) => n,
                        _ => return,
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let body = String::from_utf8_lossy(&bytes[end..end + length]).to_string();
                let sequence = count.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = tx.send(Request { headers, body });
                let reply = replies.lock().await.pop_front().unwrap_or_else(|| Reply {
                    status: 500,
                    content_type: "text/plain".into(),
                    body: "No fixture response".into(),
                    headers: vec![],
                    header_delay: None,
                    chunk_delay: None,
                    stall: None,
                });
                if (2..=3).contains(&sequence) {
                    if let Some(gate) = gate {
                        gate.wait().await;
                    }
                }
                if let Some(delay) = reply.header_delay {
                    sleep(delay).await;
                }
                let mut extra = String::new();
                for (key, value) in &reply.headers {
                    let value = value.replace("{PORT}", &port.to_string());
                    write!(extra, "{key}: {value}\r\n").unwrap();
                }
                // Stalls advertise a deliberately inflated Content-Length
                // so the client keeps waiting on the response body past
                // the configured deadline. reqwest does not enforce a
                // total-body deadline by default, so the bytes the server
                // already wrote still flow through; the missing bytes are
                // what trips the per-chunk idle timer.
                let advertised_len = if reply.stall.is_some() {
                    reply
                        .body
                        .len()
                        .saturating_add(1024)
                        .max(reply.body.len() + 1)
                } else {
                    reply.body.len()
                };
                let header = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                    reply.status,
                    reply.content_type,
                    advertised_len,
                    extra
                );
                let _ = socket.write_all(header.as_bytes()).await;
                for chunk in reply.body.as_bytes().chunks(7) {
                    if let Some(delay) = reply.chunk_delay {
                        sleep(delay).await;
                    }
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                }
                if let Some(delay) = reply.stall {
                    // Keep the socket open and do not write any more
                    // bytes. The client's per-chunk idle timer fires
                    // while the TCP connection is still alive, exercising
                    // the timeout error path. Capping the sleep at the
                    // declared delay keeps the test fixture from
                    // consuming a connection-pool slot long after the
                    // assertion has already passed.
                    sleep(delay).await;
                }
            });
        }
    });
    Server {
        url,
        requests: rx,
        count,
        task,
    }
}
pub fn answer(text: &str) -> Reply {
    Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":text}}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"cost":0.000123}}),
        ],
        true,
    )
}
pub fn tool_call(name: &str, arguments: Value) -> Reply {
    Reply::sse(
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":name,"arguments":arguments.to_string()}}]}}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"cost":0.000123}}),
        ],
        true,
    )
}
pub fn config(url: &str, directory: &std::path::Path) -> Config {
    // Tests that exercise bash-permissions policy load it from this
    // directory; writing the default policy here keeps existing tests on the
    // default `unified` mode without each test having to stage the file.
    let _ = std::fs::create_dir_all(directory);
    let _ = std::fs::write(
        directory.join("bash-permissions.json"),
        diet_soda::tools::DEFAULT_BASH_PERMISSIONS,
    );
    let mut config = Config {
        workspace: directory.into(),
        sessions_dir: directory.join("sessions"),
        skills_dir: directory.join("skills"),
        workflows_dir: directory.join("workflows"),
        exports_dir: directory.join("exports"),
        config_dir: directory.into(),
        ..Config::default()
    };
    config.providers.insert(
        "openrouter".into(),
        ProviderConfig {
            kind: ProviderKind::Openrouter,
            base_url: url.into(),
            api_key_env: None,
            timeout_seconds: 5,
        },
    );
    config
}
pub fn engine(config: Config) -> (Engine, mpsc::UnboundedReceiver<UiEvent>) {
    let session = Session::open(&config.sessions_dir, None).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    (Engine::new(config, session, tx), rx)
}

//! OTerminal's built-in MCP server: lets Claude Code (and any other MCP client) drive
//! the app's source control, editor, browser tabs and terminals.
//!
//! It speaks the MCP SSE transport on a loopback port, at the address and with the
//! headers file that the `oterminal` entry of a project's `.mcp.json` reads:
//!
//! ```json
//! {
//!   "type": "sse",
//!   "url": "${OTERMINAL_MCP_URL:-http://127.0.0.1:47820/sse}",
//!   "headersHelper": "cat \"${OTERMINAL_MCP_HEADERS:-$HOME/.oterminal/mcp-headers.json}\""
//! }
//! ```

mod tools;

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, FutureExt, StreamExt as _,
    channel::mpsc::{self, UnboundedReceiver, UnboundedSender},
    io::BufReader,
    select_biased,
};
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor};
use parking_lot::Mutex;
use serde_json::{Value, json};
use smol::net::{TcpListener, TcpStream};
use std::{net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};
use util::ResultExt as _;

const SERVER_NAME: &str = "oterminal";
const PROTOCOL_VERSION: &str = "2024-11-05";
/// The port project `.mcp.json` files fall back to outside OTerminal's own terminals.
const DEFAULT_PORT: u16 = 47820;
const URL_ENV: &str = "OTERMINAL_MCP_URL";
const HEADERS_ENV: &str = "OTERMINAL_MCP_HEADERS";
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(25);

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub fn init(cx: &mut App) {
    cx.spawn(async move |cx| {
        if let Err(error) = run(cx).await {
            log::error!("OTerminal MCP server stopped: {error:#}");
        }
    })
    .detach();
}

struct ServerState {
    executor: BackgroundExecutor,
    token: String,
    sessions: Mutex<HashMap<String, UnboundedSender<String>>>,
    incoming: UnboundedSender<IncomingMessage>,
}

/// A JSON-RPC message posted by a client, and the SSE stream its response goes to.
struct IncomingMessage {
    message: Value,
    responses: UnboundedSender<String>,
}

async fn run(cx: &mut AsyncApp) -> Result<()> {
    let token = random_hex(32);
    let listener = bind().await?;
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/sse");
    let authorization = format!("Bearer {token}");

    let headers_path = cx
        .background_spawn({
            let authorization = authorization.clone();
            async move { write_headers_file(port, &authorization) }
        })
        .await?;
    log::info!("OTerminal MCP server listening on {url}");

    cx.update(|cx| {
        terminal::set_local_terminal_env(vec![
            (URL_ENV.to_string(), url.clone()),
            (
                HEADERS_ENV.to_string(),
                headers_path.to_string_lossy().into_owned(),
            ),
        ]);
        agent_servers::set_builtin_mcp_server(
            SERVER_NAME.to_string(),
            url,
            vec![("Authorization".to_string(), authorization)],
            cx,
        );
    });

    let (incoming_tx, incoming_rx) = mpsc::unbounded();
    let state = Arc::new(ServerState {
        executor: cx.background_executor().clone(),
        token,
        sessions: Mutex::new(HashMap::default()),
        incoming: incoming_tx,
    });
    let _accept_task = cx.background_spawn(accept_connections(listener, state));

    handle_messages(incoming_rx, cx).await;
    Ok(())
}

/// Takes the default port when it is free, so the address in project `.mcp.json` files
/// stays right; a second running instance gets a free port for its own terminals.
async fn bind() -> Result<TcpListener> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, DEFAULT_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(error) => {
            log::warn!("OTerminal MCP port {DEFAULT_PORT} is taken ({error}); using a free port");
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .context("binding the OTerminal MCP server")
        }
    }
}

/// Writes the `{"Authorization": "Bearer …"}` file a project `.mcp.json` prints with its
/// `headersHelper`. The instance on the default port owns the shared file, which is what
/// Claude Code started outside OTerminal reads.
fn write_headers_file(port: u16, authorization: &str) -> Result<PathBuf> {
    let directory = util::paths::home_dir().join(".oterminal");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let file_name = if port == DEFAULT_PORT {
        "mcp-headers.json".to_string()
    } else {
        format!("mcp-headers-{port}.json")
    };
    let path = directory.join(file_name);
    let contents = serde_json::to_string(&json!({ "Authorization": authorization }))?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        // The mode above only applies to a newly created file.
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut file, contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn random_hex(byte_count: usize) -> String {
    (0..byte_count)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

async fn accept_connections(listener: TcpListener, state: Arc<ServerState>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let executor = state.executor.clone();
                let state = state.clone();
                executor
                    .spawn(async move {
                        if let Err(error) = serve_connection(stream, state).await {
                            log::debug!("OTerminal MCP connection ended: {error:#}");
                        }
                    })
                    .detach();
            }
            Err(error) => {
                log::error!("OTerminal MCP server stopped accepting connections: {error}");
                return;
            }
        }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_header_line(
    reader: &mut BufReader<TcpStream>,
    header_bytes: &mut usize,
) -> Result<String> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        bail!("connection closed before the request was complete");
    }
    *header_bytes += read;
    if *header_bytes > MAX_HEADER_BYTES {
        bail!("request headers are too large");
    }
    Ok(line.trim_end().to_string())
}

async fn read_request(stream: &TcpStream) -> Result<HttpRequest> {
    let mut reader = BufReader::new(stream.clone());
    let mut header_bytes = 0;

    let request_line = read_header_line(&mut reader, &mut header_bytes).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing request method")?.to_string();
    let target = parts.next().context("missing request target")?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut headers = HashMap::default();
    loop {
        let line = read_header_line(&mut reader, &mut header_bytes).await?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let body_length = match headers.get("content-length") {
        Some(length) => length
            .parse::<usize>()
            .context("invalid Content-Length header")?,
        None => 0,
    };
    if body_length > MAX_BODY_BYTES {
        bail!("request body is too large");
    }
    let mut body = vec![0; body_length];
    reader.read_exact(&mut body).await?;

    Ok(HttpRequest {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra_headers: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn serve_connection(mut stream: TcpStream, state: Arc<ServerState>) -> Result<()> {
    let request = read_request(&stream).await?;

    // A page on another site can reach a loopback port by pointing a host name at
    // 127.0.0.1 (DNS rebinding); the Host header then still names that site.
    let host = request.headers.get("host").map_or("", String::as_str);
    if !is_loopback_host(host) {
        return respond(
            &mut stream,
            "421 Misdirected Request",
            "text/plain",
            "",
            "Misdirected Request",
        )
        .await;
    }
    if let Some(origin) = request.headers.get("origin")
        && !is_loopback_origin(origin)
    {
        return respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            "",
            "Forbidden origin",
        )
        .await;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/healthz") => {
            let body = json!({
                "ok": true,
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            });
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                "",
                &body.to_string(),
            )
            .await
        }
        ("GET", "/sse") => {
            if !is_authorized(&request, &state.token) {
                return respond_unauthorized(&mut stream).await;
            }
            serve_event_stream(stream, state).await
        }
        ("POST", "/messages") => {
            if !is_authorized(&request, &state.token) {
                return respond_unauthorized(&mut stream).await;
            }
            receive_message(stream, request, state).await
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", "", "Not Found").await,
    }
}

async fn respond_unauthorized(stream: &mut TcpStream) -> Result<()> {
    respond(
        stream,
        "401 Unauthorized",
        "text/plain",
        "WWW-Authenticate: Bearer realm=\"oterminal-mcp\"\r\n",
        "",
    )
    .await
}

fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn is_loopback_origin(origin: &str) -> bool {
    url::Url::parse(origin).is_ok_and(|origin| {
        matches!(
            origin.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        )
    })
}

fn is_authorized(request: &HttpRequest, token: &str) -> bool {
    let Some(header) = request.headers.get("authorization") else {
        return false;
    };
    let Some((scheme, presented)) = header.trim().split_once(char::is_whitespace) else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && constant_time_eq(presented.trim(), token)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0, |difference, (left, right)| difference | (left ^ right))
            == 0
}

async fn serve_event_stream(mut stream: TcpStream, state: Arc<ServerState>) -> Result<()> {
    let session_id = random_hex(16);
    let (responses_tx, mut responses_rx) = mpsc::unbounded::<String>();
    state
        .sessions
        .lock()
        .insert(session_id.clone(), responses_tx);
    let _remove_session = util::defer({
        let state = state.clone();
        let session_id = session_id.clone();
        move || {
            state.sessions.lock().remove(&session_id);
        }
    });

    // Per the MCP SSE transport, the first event tells the client where to POST.
    let opening = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /messages?sessionId={session_id}\n\n"
    );
    stream.write_all(opening.as_bytes()).await?;
    stream.flush().await?;

    loop {
        let event = select_biased! {
            response = responses_rx.next() => match response {
                Some(response) => format!("event: message\ndata: {response}\n\n"),
                None => return Ok(()),
            },
            // Also how a client that went away is noticed: the write fails.
            _ = FutureExt::fuse(state.executor.timer(KEEP_ALIVE_INTERVAL)) => {
                ": keep-alive\n\n".to_string()
            }
        };
        stream.write_all(event.as_bytes()).await?;
        stream.flush().await?;
    }
}

async fn receive_message(
    mut stream: TcpStream,
    request: HttpRequest,
    state: Arc<ServerState>,
) -> Result<()> {
    let session_id = url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == "sessionId")
        .map(|(_, value)| value.into_owned());
    let responses =
        session_id.and_then(|session_id| state.sessions.lock().get(&session_id).cloned());
    let Some(responses) = responses else {
        return respond(
            &mut stream,
            "404 Not Found",
            "text/plain",
            "",
            "Unknown session",
        )
        .await;
    };

    let Ok(parsed) = serde_json::from_slice::<Value>(&request.body) else {
        return respond(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            "",
            "Invalid JSON",
        )
        .await;
    };

    // The response to the message itself is delivered over the SSE stream.
    respond(&mut stream, "202 Accepted", "text/plain", "", "Accepted").await?;

    let messages = match parsed {
        Value::Array(messages) => messages,
        message => vec![message],
    };
    for message in messages {
        state
            .incoming
            .unbounded_send(IncomingMessage {
                message,
                responses: responses.clone(),
            })
            .map_err(|_| anyhow!("the OTerminal MCP server is shutting down"))?;
    }
    Ok(())
}

async fn handle_messages(mut incoming: UnboundedReceiver<IncomingMessage>, cx: &mut AsyncApp) {
    while let Some(IncomingMessage { message, responses }) = incoming.next().await {
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        let outcome = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::definitions() })),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    send_response(
                        &responses,
                        id,
                        Err((INVALID_PARAMS, "tools/call requires a name string".into())),
                    );
                    continue;
                };
                let name = name.to_string();
                let arguments = match params.get("arguments") {
                    Some(arguments @ Value::Object(_)) => arguments.clone(),
                    _ => json!({}),
                };
                // Run on its own task so a slow tool (a push, say) does not hold up
                // the other calls and sessions.
                cx.spawn(async move |cx| {
                    let result = match tools::call(&name, arguments, cx).await {
                        Ok(output) => json!({
                            "content": [{ "type": "text", "text": tool_output_text(output) }],
                        }),
                        Err(error) => json!({
                            "content": [{ "type": "text", "text": format!("{error:#}") }],
                            "isError": true,
                        }),
                    };
                    send_response(&responses, id, Ok(result));
                })
                .detach();
                continue;
            }
            method if method.starts_with("notifications/") => continue,
            method => Err((METHOD_NOT_FOUND, format!("Unknown method: {method}"))),
        };
        send_response(&responses, id, outcome);
    }
}

fn tool_output_text(output: Value) -> String {
    match output {
        Value::String(text) => text,
        output => serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string()),
    }
}

fn send_response(
    responses: &UnboundedSender<String>,
    id: Option<Value>,
    outcome: Result<Value, (i64, String)>,
) {
    // A message without an id is a notification, which gets no response.
    let Some(id) = id else {
        return;
    };
    let response = match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    };
    responses
        .unbounded_send(response.to_string())
        .context("the MCP client disconnected before the response was sent")
        .log_err();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Read as _, Write as _};

    fn connect(port: u16) -> std::net::TcpStream {
        let stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream
    }

    fn next_event_data(events: &mut std::io::BufReader<std::net::TcpStream>) -> String {
        loop {
            let mut line = String::new();
            assert_ne!(events.read_line(&mut line).unwrap(), 0, "stream closed");
            if let Some(data) = line.strip_prefix("data: ") {
                return data.trim_end().to_string();
            }
        }
    }

    fn post(port: u16, endpoint: &str, authorization: &str, message: Value) -> String {
        let body = message.to_string();
        let mut stream = connect(port);
        write!(
            stream,
            "POST {endpoint} HTTP/1.1
Host: 127.0.0.1:{port}
Authorization: {authorization}
Content-Type: application/json
Content-Length: {}

{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.lines().next().unwrap_or_default().to_string()
    }

    fn run_client(port: u16) -> Vec<Value> {
        let mut unauthorized = connect(port);
        write!(
            unauthorized,
            "GET /sse HTTP/1.1
Host: 127.0.0.1:{port}

"
        )
        .unwrap();
        let mut response = String::new();
        unauthorized.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");

        let mut events = connect(port);
        write!(
            events,
            "GET /sse HTTP/1.1
Host: 127.0.0.1:{port}
Authorization: Bearer secret

"
        )
        .unwrap();
        let mut events = std::io::BufReader::new(events);
        let endpoint = next_event_data(&mut events);
        assert!(endpoint.starts_with("/messages?sessionId="), "{endpoint}");

        assert!(post(port, &endpoint, "Bearer wrong", json!({})).starts_with("HTTP/1.1 401"));
        assert!(
            post(
                port,
                "/messages?sessionId=unknown",
                "Bearer secret",
                json!({})
            )
            .starts_with("HTTP/1.1 404")
        );

        let requests = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "workspace.list", "arguments": {} },
            }),
            json!({ "jsonrpc": "2.0", "id": 4, "method": "no/such/method" }),
        ];
        let mut responses = Vec::new();
        for request in requests {
            let expects_response = request.get("id").is_some();
            let status = post(port, &endpoint, "Bearer secret", request);
            assert!(status.starts_with("HTTP/1.1 202"), "{status}");
            if expects_response {
                responses.push(serde_json::from_str(&next_event_data(&mut events)).unwrap());
            }
        }
        responses
    }

    #[gpui::test]
    async fn test_sse_session(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (incoming_tx, incoming_rx) = mpsc::unbounded();
        let state = Arc::new(ServerState {
            executor: cx.executor(),
            token: "secret".to_string(),
            sessions: Mutex::new(HashMap::default()),
            incoming: incoming_tx,
        });
        let _accept_task = cx
            .background_executor
            .spawn(accept_connections(listener, state));
        let _messages_task =
            cx.update(|cx| cx.spawn(async move |cx| handle_messages(incoming_rx, cx).await));

        let responses = smol::unblock(move || run_client(port)).await;

        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(responses[0]["result"]["serverInfo"]["name"], SERVER_NAME);

        let tool_names = responses[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(tool_names.contains(&"git.commit"), "{tool_names:?}");
        assert!(tool_names.contains(&"terminal.run"), "{tool_names:?}");

        assert_eq!(responses[2]["id"], 3);
        let text = responses[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            json!({ "workspaces": [] })
        );

        assert_eq!(responses[3]["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn test_loopback_host() {
        assert!(is_loopback_host("127.0.0.1:47820"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("[::1]:47820"));
        assert!(!is_loopback_host("example.com:47820"));
        assert!(!is_loopback_host(""));
    }

    #[test]
    fn test_loopback_origin() {
        assert!(is_loopback_origin("http://localhost:3000"));
        assert!(is_loopback_origin("http://127.0.0.1"));
        assert!(!is_loopback_origin("https://example.com"));
        assert!(!is_loopback_origin("not a url"));
    }

    #[test]
    fn test_authorization() {
        let request = |authorization: Option<&str>| HttpRequest {
            method: "GET".to_string(),
            path: "/sse".to_string(),
            query: String::new(),
            headers: authorization
                .into_iter()
                .map(|value| ("authorization".to_string(), value.to_string()))
                .collect(),
            body: Vec::new(),
        };
        assert!(is_authorized(&request(Some("Bearer secret")), "secret"));
        assert!(is_authorized(&request(Some("bearer  secret ")), "secret"));
        assert!(!is_authorized(&request(Some("Bearer other")), "secret"));
        assert!(!is_authorized(&request(Some("secret")), "secret"));
        assert!(!is_authorized(&request(None), "secret"));
    }
}

//! Persona-compatible loopback bridge and minimal Streamable HTTP MCP server.
//!
//! The control plane intentionally stays off the render thread. Requests are
//! validated on a loopback-only worker and reduced to bounded commands that
//! the fixed-step core drains once per tick.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::Catalog;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const COMMAND_QUEUE_CAPACITY: usize = 128;
const MCP_SESSION: &str = "pocket-persona-v1";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct VoiceState {
    pub phase: String,
    pub activity: String,
    #[serde(rename = "microphoneMuted")]
    pub microphone_muted: bool,
    #[serde(rename = "outputMuted")]
    pub output_muted: bool,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            phase: "inactive".into(),
            activity: "idle".into(),
            microphone_muted: false,
            output_muted: false,
        }
    }
}

impl VoiceState {
    pub fn speaking(&self) -> bool {
        self.phase == "active" && self.activity == "speaking" && !self.output_muted
    }

    fn valid(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "inactive" | "starting" | "active" | "stopping"
        ) && matches!(self.activity.as_str(), "idle" | "listening" | "speaking")
    }
}

#[derive(Clone, Debug)]
pub enum BridgeCommand {
    Voice(VoiceState),
    AudioLevel(f32),
    PlayAnimation(String),
    Window { visible: bool },
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusSnapshot {
    #[serde(rename = "modelConfigured")]
    pub model_configured: bool,
    #[serde(rename = "windowVisible")]
    pub window_visible: bool,
    #[serde(rename = "voiceState")]
    pub voice_state: VoiceState,
    #[serde(rename = "audioLevel")]
    pub audio_level: f32,
    #[serde(rename = "activeAnimation")]
    pub active_animation: String,
    #[serde(rename = "renderFps")]
    pub render_fps: f32,
    #[serde(rename = "frameTimeP95Ms")]
    pub frame_time_p95_ms: f32,
    #[serde(rename = "frameTimeP99Ms")]
    pub frame_time_p99_ms: f32,
    #[serde(rename = "frameTimeMaxMs")]
    pub frame_time_max_ms: f32,
}

impl StatusSnapshot {
    pub fn new() -> Self {
        Self {
            model_configured: true,
            window_visible: true,
            voice_state: VoiceState::default(),
            audio_level: 0.0,
            active_animation: "idle".into(),
            render_fps: 0.0,
            frame_time_p95_ms: 0.0,
            frame_time_p99_ms: 0.0,
            frame_time_max_ms: 0.0,
        }
    }
}

pub struct Bridge {
    receiver: mpsc::Receiver<BridgeCommand>,
    status: Arc<Mutex<StatusSnapshot>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Bridge {
    pub fn start(port: u16, catalog: Arc<Catalog>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("binding Pocket Persona bridge port {port}"))?;
        let address = listener.local_addr()?;
        let (sender, receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let status = Arc::new(Mutex::new(StatusSnapshot::new()));
        let thread_status = status.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        listener.set_nonblocking(true)?;
        let worker = std::thread::Builder::new()
            .name("pocket-persona-bridge".into())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _address)) => {
                            if let Err(error) =
                                handle_connection(stream, &catalog, &thread_status, &sender)
                            {
                                log::warn!("Pocket Persona bridge request: {error:#}");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => {
                            log::warn!("Pocket Persona bridge accept: {error}");
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
            })?;
        log::info!("Pocket Persona bridge: http://{address}");
        Ok(Self {
            receiver,
            status,
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn drain(&self) -> impl Iterator<Item = BridgeCommand> + '_ {
        self.receiver.try_iter()
    }

    pub fn update_status(&self, update: impl FnOnce(&mut StatusSnapshot)) {
        if let Ok(mut status) = self.status.lock() {
            update(&mut status);
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", "application/json".into())],
            body: serde_json::to_vec(&value).expect("JSON value serializes"),
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    catalog: &Catalog,
    status: &Arc<Mutex<StatusSnapshot>>,
    sender: &mpsc::SyncSender<BridgeCommand>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = read_request(&mut stream)?;
    let response = route(&request, catalog, status, sender);
    write_response(&mut stream, response)?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut bytes = Vec::with_capacity(4096);
    let mut scratch = [0u8; 4096];
    let (header_end, content_length) = loop {
        let read = stream.read(&mut scratch)?;
        if read == 0 {
            anyhow::bail!("request ended before its headers");
        }
        bytes.extend_from_slice(&scratch[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            anyhow::bail!("request exceeds {MAX_REQUEST_BYTES} bytes");
        }
        if let Some(end) = find_subslice(&bytes, b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let content_length = headers
                .lines()
                .skip(1)
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if end + 4 + content_length > MAX_REQUEST_BYTES {
                anyhow::bail!("request body exceeds the bridge limit");
            }
            break (end, content_length);
        }
    };
    let body_end = header_end + 4 + content_length;
    while bytes.len() < body_end {
        let read = stream.read(&mut scratch)?;
        if read == 0 {
            anyhow::bail!("request body is truncated");
        }
        bytes.extend_from_slice(&scratch[..read]);
    }

    let head = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().context("request line is missing")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().context("request method is missing")?;
    let path = request_parts.next().context("request path is missing")?;
    if request_parts.next() != Some("HTTP/1.1") {
        anyhow::bail!("only HTTP/1.1 is supported");
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').context("malformed request header")?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Ok(Request {
        method: method.into(),
        path: path.into(),
        headers,
        body: bytes[header_end + 4..body_end].to_vec(),
    })
}

fn route(
    request: &Request,
    catalog: &Catalog,
    status: &Arc<Mutex<StatusSnapshot>>,
    sender: &mpsc::SyncSender<BridgeCommand>,
) -> Response {
    if request
        .headers
        .get("host")
        .is_none_or(|host| !allowed_host(host))
    {
        return Response::empty(403);
    }
    let origin = request.headers.get("origin").map(String::as_str);
    if origin.is_some_and(|origin| !allowed_origin(origin)) {
        return Response::empty(403);
    }

    if request.method == "GET" && request.path == "/health" {
        let snapshot = status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| StatusSnapshot::new());
        return Response::json(200, json!({ "ok": true, "status": snapshot }));
    }

    if request.method == "OPTIONS" && request.path == "/events" {
        let mut response = Response::empty(204);
        add_cors(&mut response, origin);
        response
            .headers
            .push(("Access-Control-Allow-Methods", "POST, OPTIONS".into()));
        response
            .headers
            .push(("Access-Control-Allow-Headers", "content-type".into()));
        return response;
    }

    if request.method == "POST" && request.path == "/events" {
        let Ok(value) = serde_json::from_slice::<Value>(&request.body) else {
            return Response::empty(400);
        };
        let Some(command) = normalize_event(&value, catalog) else {
            return Response::empty(422);
        };
        if sender.try_send(command.clone()).is_err() {
            return Response::empty(503);
        }
        apply_status_for_command(status, &command);
        let mut response = Response::json(202, json!({ "accepted": true }));
        add_cors(&mut response, origin);
        return response;
    }

    if request.path == "/mcp" {
        if request.method == "GET" {
            let mut response = Response::empty(405);
            response.headers.push(("Allow", "POST, DELETE".into()));
            return response;
        }
        if request.method == "DELETE" {
            return Response::empty(200);
        }
        if request.method != "POST" {
            return Response::empty(405);
        }
        let Ok(value) = serde_json::from_slice::<Value>(&request.body) else {
            return json_rpc_error(Value::Null, -32700, "Parse error");
        };
        return route_mcp(value, catalog, status, sender);
    }

    Response::empty(404)
}

fn normalize_event(value: &Value, catalog: &Catalog) -> Option<BridgeCommand> {
    match value.get("type")?.as_str()? {
        "state" => {
            let state: VoiceState = serde_json::from_value(value.get("state")?.clone()).ok()?;
            state.valid().then_some(BridgeCommand::Voice(state))
        }
        "audio-level" => {
            let level = value.get("level")?.as_f64()?;
            level
                .is_finite()
                .then_some(BridgeCommand::AudioLevel(level.clamp(0.0, 1.0) as f32))
        }
        "animation" => {
            let name = value.get("animation_name")?.as_str()?;
            (valid_action_name(name)
                && catalog
                    .action(name)
                    .is_some_and(|action| !action.clips.is_empty()))
            .then(|| BridgeCommand::PlayAnimation(name.into()))
        }
        _ => None,
    }
}

fn route_mcp(
    value: Value,
    catalog: &Catalog,
    status: &Arc<Mutex<StatusSnapshot>>,
    sender: &mpsc::SyncSender<BridgeCommand>,
) -> Response {
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return json_rpc_error(Value::Null, -32600, "Invalid Request");
    }
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return json_rpc_error(id, -32600, "Invalid Request");
    };
    if method.starts_with("notifications/") {
        return Response::empty(202);
    }
    let result = match method {
        "initialize" => json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "Pocket Persona", "version": "0.1.0" },
            "instructions": "Pocket Persona controls the local native desktop character. It never speaks, records, transcribes, or sends audio."
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools(catalog) }),
        "tools/call" => {
            let Some(name) = value.pointer("/params/name").and_then(Value::as_str) else {
                return json_rpc_error(id, -32602, "Tool name is required");
            };
            return mcp_tool_call(
                id,
                name,
                value.pointer("/params/arguments"),
                catalog,
                status,
                sender,
            );
        }
        _ => return json_rpc_error(id, -32601, "Method not found"),
    };
    let mut response = Response::json(200, json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    response
        .headers
        .push(("Mcp-Session-Id", MCP_SESSION.into()));
    response
}

fn tools(catalog: &Catalog) -> Vec<Value> {
    let actions = describe_actions(catalog);
    vec![
        json!({
            "name": "play_animation",
            "title": "Play Pocket Persona animation",
            "description": format!("Play an installed character action once.\n{actions}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "animation": {
                        "type": "string",
                        "description": format!("Installed action name.\n{actions}")
                    }
                },
                "required": ["animation"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "list_animations",
            "title": "List Pocket Persona animations",
            "description": "Read installed action names, descriptions, and trigger scenarios.",
            "inputSchema": { "type": "object", "additionalProperties": false },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "control_window",
            "title": "Control Pocket Persona window",
            "description": "Show, hide, or toggle the native character window.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["show", "hide", "toggle"] }
                },
                "required": ["action"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "get_status",
            "title": "Get Pocket Persona status",
            "description": "Read model, window, voice, animation, and render state.",
            "inputSchema": { "type": "object", "additionalProperties": false },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
    ]
}

fn mcp_tool_call(
    id: Value,
    name: &str,
    arguments: Option<&Value>,
    catalog: &Catalog,
    status: &Arc<Mutex<StatusSnapshot>>,
    sender: &mpsc::SyncSender<BridgeCommand>,
) -> Response {
    let (text, is_error) = match name {
        "play_animation" => {
            let animation = arguments
                .and_then(|arguments| arguments.get("animation"))
                .and_then(Value::as_str);
            match animation.and_then(|name| catalog.action(name)) {
                Some(action) if !action.clips.is_empty() => {
                    let command = BridgeCommand::PlayAnimation(action.name.clone());
                    if sender.try_send(command.clone()).is_err() {
                        ("Pocket Persona is busy or shutting down.".into(), true)
                    } else {
                        apply_status_for_command(status, &command);
                        (format!("Pocket Persona is playing the {} action.", action.name), false)
                    }
                }
                _ => (
                    "That action is not currently playable. Call list_animations for the current catalog.".into(),
                    true,
                ),
            }
        }
        "list_animations" => (describe_actions(catalog), false),
        "control_window" => {
            let action = arguments
                .and_then(|arguments| arguments.get("action"))
                .and_then(Value::as_str);
            let current = status
                .lock()
                .map(|status| status.window_visible)
                .unwrap_or(true);
            let visible = match action {
                Some("show") => true,
                Some("hide") => false,
                Some("toggle") => !current,
                _ => {
                    return json_rpc_error(
                        id,
                        -32602,
                        "Window action must be show, hide, or toggle",
                    );
                }
            };
            let command = BridgeCommand::Window { visible };
            if sender.try_send(command.clone()).is_err() {
                ("Pocket Persona is busy or shutting down.".into(), true)
            } else {
                apply_status_for_command(status, &command);
                (
                    format!(
                        "Pocket Persona's window is now {}.",
                        if visible { "visible" } else { "hidden" }
                    ),
                    false,
                )
            }
        }
        "get_status" => {
            let snapshot = status
                .lock()
                .map(|status| status.clone())
                .unwrap_or_else(|_| StatusSnapshot::new());
            (
                serde_json::to_string(&snapshot).expect("status serializes"),
                false,
            )
        }
        _ => return json_rpc_error(id, -32602, "Unknown tool"),
    };
    let mut result = json!({
        "content": [{ "type": "text", "text": text }]
    });
    if is_error {
        result["isError"] = Value::Bool(true);
    }
    let mut response = Response::json(200, json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    response
        .headers
        .push(("Mcp-Session-Id", MCP_SESSION.into()));
    response
}

fn apply_status_for_command(status: &Arc<Mutex<StatusSnapshot>>, command: &BridgeCommand) {
    let Ok(mut status) = status.lock() else {
        return;
    };
    match command {
        BridgeCommand::Voice(voice) => status.voice_state = voice.clone(),
        BridgeCommand::AudioLevel(level) => status.audio_level = *level,
        BridgeCommand::PlayAnimation(name) => status.active_animation = name.clone(),
        BridgeCommand::Window { visible } => status.window_visible = *visible,
    }
}

fn describe_actions(catalog: &Catalog) -> String {
    let rows: Vec<String> = catalog
        .playable_actions()
        .map(|action| {
            format!(
                "- {}: {} Trigger scenario: {}",
                action.name, action.description, action.trigger_scenario
            )
        })
        .collect();
    if rows.is_empty() {
        "- No animation actions currently have playable clips.".into()
    } else {
        rows.join("\n")
    }
}

fn json_rpc_error(id: Value, code: i32, message: &str) -> Response {
    Response::json(
        200,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    )
}

fn add_cors(response: &mut Response, origin: Option<&str>) {
    if let Some(origin) = origin {
        response
            .headers
            .push(("Access-Control-Allow-Origin", origin.into()));
        response.headers.push(("Vary", "Origin".into()));
    }
}

fn allowed_host(host: &str) -> bool {
    allowed_loopback_authority(host)
}

fn allowed_origin(origin: &str) -> bool {
    if let Some(authority) = origin.strip_prefix("codex-app://") {
        return authority
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-'));
    }
    ["http://", "https://"].into_iter().any(|scheme| {
        origin
            .strip_prefix(scheme)
            .is_some_and(allowed_loopback_authority)
    })
}

fn allowed_loopback_authority(authority: &str) -> bool {
    ["localhost", "127.0.0.1", "[::1]"].into_iter().any(|host| {
        authority == host
            || authority
                .strip_prefix(&format!("{host}:"))
                .is_some_and(|port| !port.is_empty() && port.parse::<u16>().is_ok())
    })
}

fn valid_action_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && !value.ends_with('-')
        && !value.contains("--")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_response(stream: &mut TcpStream, response: Response) -> Result<()> {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        422 => "Unprocessable Entity",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(stream, "HTTP/1.1 {} {reason}\r\n", response.status)?;
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::catalog::{ActionRole, ActionSpec};

    use super::*;

    fn catalog() -> Catalog {
        Catalog {
            model_name: "Test".into(),
            model_path: PathBuf::from("model.vrm"),
            actions: vec![ActionSpec {
                name: "wave-hello".into(),
                description: "A friendly wave.".into(),
                trigger_scenario: "When greeting.".into(),
                role: ActionRole::Custom,
                clips: vec![PathBuf::from("wave.vrma")],
            }],
        }
    }

    fn request(method: &str, path: &str, body: Value) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: HashMap::from([("host".into(), "127.0.0.1:47831".into())]),
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    #[test]
    fn persona_events_are_normalized_and_clamped() {
        assert!(matches!(
            normalize_event(&json!({ "type": "audio-level", "level": 4.0 }), &catalog()),
            Some(BridgeCommand::AudioLevel(1.0))
        ));
        assert!(
            normalize_event(
                &json!({
                    "type": "state",
                    "state": {
                        "phase": "active",
                        "activity": "singing",
                        "microphoneMuted": false,
                        "outputMuted": false
                    }
                }),
                &catalog()
            )
            .is_none()
        );
        assert!(
            normalize_event(
                &json!({ "type": "animation", "animation_name": "not-installed" }),
                &catalog()
            )
            .is_none()
        );
    }

    #[test]
    fn bridge_rejects_loopback_prefix_spoofing() {
        assert!(allowed_host("localhost:47831"));
        assert!(allowed_origin("https://127.0.0.1:47831"));
        assert!(!allowed_host("localhost.evil.example"));
        assert!(!allowed_host("localhost:not-a-port"));
        assert!(!allowed_origin("https://localhost.evil.example"));
        assert!(!allowed_origin("http://127.0.0.1:47831.evil.example"));
        assert!(!allowed_origin("codex-app://trusted/path"));
    }

    #[test]
    fn mcp_lists_the_persona_compatible_tools() {
        let (sender, _receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let status = Arc::new(Mutex::new(StatusSnapshot::new()));
        let response = route(
            &request(
                "POST",
                "/mcp",
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
            ),
            &catalog(),
            &status,
            &sender,
        );
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        let names: Vec<&str> = body
            .pointer("/result/tools")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name")?.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "play_animation",
                "list_animations",
                "control_window",
                "get_status"
            ]
        );
    }

    #[test]
    fn mcp_negotiates_the_supported_protocol_version() {
        let (sender, _receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let status = Arc::new(Mutex::new(StatusSnapshot::new()));
        let response = route(
            &request(
                "POST",
                "/mcp",
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocolVersion": "2099-01-01" }
                }),
            ),
            &catalog(),
            &status,
            &sender,
        );
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            body.pointer("/result/protocolVersion")
                .and_then(Value::as_str),
            Some(MCP_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn unknown_animation_is_rejected_without_a_command() {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let status = Arc::new(Mutex::new(StatusSnapshot::new()));
        let response = route(
            &request(
                "POST",
                "/mcp",
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "play_animation",
                        "arguments": { "animation": "not-installed" }
                    }
                }),
            ),
            &catalog(),
            &status,
            &sender,
        );
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            body.pointer("/result/isError").and_then(Value::as_bool),
            Some(true)
        );
        assert!(receiver.try_recv().is_err());
    }
}

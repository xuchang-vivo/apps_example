// Copyright (c) 2025 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use embedded_websocket as ws;
use serde::Deserialize;
use std::{
    fmt::Write as _,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
};
use ws::framer::{Framer, FramerError, ReadResult};
use ws::{WebSocketContext, WebSocketSendMessageType, WebSocketServer};

use crate::AgentRuntime;

const WS_PORT: u16 = 9001;
const WS_PATH: &str = "/ws";
const MAX_HEADERS: usize = 16;
// The ESP32-C3 has a small, fragmented heap.  These buffers only need to
// cover the HTTP upgrade and the small device-control RPCs; keeping them
// bounded leaves room for the agent runtime and JSON serialization.
// A larger socket read buffer reduces short TCP reads on the ESP32-C3,
// lowering the chance of WebSocket frame headers being split across reads.
// TCP still does not preserve WebSocket frame boundaries, so this is only a
// mitigation; the parser must still handle incomplete frames correctly.
const READ_BUF_SIZE: usize = 2048;
const WRITE_BUF_SIZE: usize = 1024;
const FRAME_BUF_SIZE: usize = 2048;
const MAX_RPC_MESSAGE_BYTES: usize = FRAME_BUF_SIZE;
// A client that vanishes without a FIN/RST (WiFi loss, kill -9) leaves read()
// or write() blocked forever, pinning the single-connection accept loop on a
// dead socket. Bound both directions. The limit must sit above a normal agent
// turn (MAX_TURN_DURATION_MS = 120s is only the internal model-call ceiling;
// real prompts return faster), so 90s reclaims a dropped peer while staying
// clear of legitimate turns. On read expiry the kernel turns the read into a
// TimedOut io::Error, which TcpWsStream::read converts to an EOF so the
// framer exits via ReadResult::Closed and the accept loop continues. On write
// expiry (e.g. a Pong sent to a peer whose receive window has closed) the
// error propagates up through framer::send_back and write_text_message, so
// handle_client returns Err and the accept loop moves on. BlueOS does not
// implement SO_KEEPALIVE, so these timeouts are the only available probes for
// silently dropped peers.
const CLIENT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
const CLIENT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

#[derive(Deserialize)]
struct RpcRequest<'a> {
    #[serde(default, borrow)]
    jsonrpc: Option<&'a str>,
    #[serde(default, borrow)]
    method: Option<&'a str>,
    #[serde(default)]
    id: Option<RpcId<'a>>,
    #[serde(default, borrow)]
    params: Option<RpcParams<'a>>,
}

#[derive(Deserialize)]
struct RpcParams<'a> {
    #[serde(default, borrow)]
    message: Option<&'a str>,
}

// Keep the request id borrowed from the frame instead of materializing a
// serde_json::Value tree. JSON-RPC permits string and numeric ids; null and a
// missing id are represented by Option::None and both serialize as null.
#[derive(Deserialize)]
#[serde(untagged)]
enum RpcId<'a> {
    #[serde(borrow)]
    String(&'a str),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
}

struct TcpWsStream {
    inner: TcpStream,
    prefetched: Vec<u8>,
    prefetched_cursor: usize,
}

impl TcpWsStream {
    fn new(inner: TcpStream, prefetched: Vec<u8>) -> Self {
        Self {
            inner,
            prefetched,
            prefetched_cursor: 0,
        }
    }
}

impl ws::framer::Stream<std::io::Error> for TcpWsStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        println!("[ws] tcp read begin capacity={}", buf.len());
        if self.prefetched_cursor < self.prefetched.len() {
            let remaining = &self.prefetched[self.prefetched_cursor..];
            let size = remaining.len().min(buf.len());
            buf[..size].copy_from_slice(&remaining[..size]);
            self.prefetched_cursor += size;
            println!("[ws] tcp read complete buffered bytes={size}");
            return Ok(size);
        }
        let result = self.inner.read(buf);
        match &result {
            Ok(size) => println!("[ws] tcp read complete bytes={size}"),
            Err(error) => {
                // A read timeout means the peer vanished without closing the
                // socket. Surface it as a clean EOF so the framer returns
                // ReadResult::Closed and handle_client falls back to the
                // accept loop instead of hanging on a dead connection.
                if error.kind() == std::io::ErrorKind::TimedOut {
                    println!("[ws] client read timeout; treating as close");
                    return Ok(0);
                }
                eprintln!("[ws] tcp read failed: {error}");
            }
        }
        result
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        self.inner.write_all(buf)
    }
}

pub fn start_server(runtime: Arc<Mutex<AgentRuntime>>) {
    if let Err(error) = run_server(runtime) {
        eprintln!("[ws] server exited: {error}");
    }
}

fn run_server(runtime: Arc<Mutex<AgentRuntime>>) -> Result<(), String> {
    let bind_addr = ws_bind_addr()?;
    let listener = TcpListener::bind(bind_addr.as_str())
        .map_err(|error| format!("bind {bind_addr} failed: {error}"))?;
    // The socket is bound and the server is about to accept clients. Light
    // the red LED as a steady "ready" signal. The device handles are
    // released on return so capability invocations can still reopen the
    // device; the LED stays lit across client connections.
    if let Err(error) = crate::caps::led::turn_on("red") {
        eprintln!("[ws] ready LED on failed: {error}");
    }
    println!("[ws] listening on ws://{bind_addr}{WS_PATH}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let runtime = Arc::clone(&runtime);
                // There is only one supported WS connection. Handle it on the
                // main task so accepting a client does not require another
                // large heap-backed thread stack on ESP32-C3.
                if let Err(error) = handle_client(stream, runtime) {
                    eprintln!("[ws] client error: {error}");
                }
            }
            Err(error) => eprintln!("[ws] accept failed: {error}"),
        }
    }

    Ok(())
}

fn ws_bind_addr() -> Result<String, String> {
    let ip = core::str::from_utf8(blueos_kconfig::CONFIG_NET_STATIC_IP)
        .map_err(|_| String::from("CONFIG_NET_STATIC_IP is not valid UTF-8"))?
        .trim_matches(|character: char| character == '\0' || character.is_ascii_whitespace());
    if ip.is_empty() {
        return Err(String::from("CONFIG_NET_STATIC_IP is empty"));
    }
    Ok(format!("{ip}:{WS_PORT}"))
}

fn handle_client(mut stream: TcpStream, runtime: Arc<Mutex<AgentRuntime>>) -> Result<(), String> {
    // Bound the time a silently dropped client can pin the accept loop. Set
    // before any I/O so the upgrade-header read and every later frame read/
    // write are covered. Write timeout also covers framer send_back(Pong) on
    // a peer whose receive window has shut.
    if let Err(error) = stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT)) {
        eprintln!("[ws] set_read_timeout failed: {error}");
    }
    if let Err(error) = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT)) {
        eprintln!("[ws] set_write_timeout failed: {error}");
    }
    // Keep protocol buffers off the handler stack; a WS request can already
    // trigger serde and the agent loop to use substantial temporary storage.
    let mut read_buf = vec![0u8; READ_BUF_SIZE];
    let mut read_cursor = 0usize;

    let Some(websocket_context) =
        read_upgrade_header(&mut stream, &mut read_buf, &mut read_cursor)?
    else {
        return Ok(());
    };

    let mut write_buf = vec![0u8; WRITE_BUF_SIZE];
    let mut frame_buf = vec![0u8; FRAME_BUF_SIZE];
    let mut websocket = WebSocketServer::new_server();
    let initial_read_len = read_cursor;
    let prefetched = read_buf[..initial_read_len].to_vec();
    read_cursor = 0;
    println!("[ws] websocket buffered frame bytes={initial_read_len}");
    let mut ws_stream = TcpWsStream::new(stream, prefetched);
    let mut framer = Framer::new(
        &mut read_buf,
        &mut read_cursor,
        &mut write_buf,
        &mut websocket,
    );

    framer
        .accept(&mut ws_stream, &websocket_context)
        .map_err(format_framer_error)?;

    println!("[ws] connection opened");

    loop {
        println!("[ws] waiting for request");
        match framer
            .read(&mut ws_stream, &mut frame_buf)
            .map_err(format_framer_error)?
        {
            ReadResult::Text(text) => {
                println!("[ws] request received bytes={}", text.len());
                if text.len() > MAX_RPC_MESSAGE_BYTES {
                    return Err(format!(
                        "rpc message exceeds {} bytes",
                        MAX_RPC_MESSAGE_BYTES
                    ));
                }
                let response_text = process_rpc(runtime.as_ref(), text);
                write_text_message(&mut framer, &mut ws_stream, &response_text)?;
            }
            ReadResult::Binary(_) => {
                let response_text = rpc_error_null(
                    -32600,
                    "Invalid Request",
                    Some("only text JSON-RPC frame is supported"),
                );
                write_text_message(&mut framer, &mut ws_stream, &response_text)?;
            }
            ReadResult::Pong(_) => {}
            ReadResult::Closed => {
                println!("[ws] connection closed");
                break;
            }
        }
    }

    Ok(())
}

fn write_text_message(
    framer: &mut Framer<'_, ws::EmptyRng, ws::Server>,
    stream: &mut TcpWsStream,
    text: &str,
) -> Result<(), String> {
    // The websocket crate does not fragment automatically. Leave room for the
    // largest possible frame header and split only at UTF-8 boundaries.
    const MAX_PAYLOAD: usize = WRITE_BUF_SIZE - 14;
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return framer
            .write(stream, WebSocketSendMessageType::Text, true, bytes)
            .map_err(format_framer_error);
    }

    let mut offset = 0;
    while offset < bytes.len() {
        let mut end = (offset + MAX_PAYLOAD).min(bytes.len());
        while end > offset && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == offset {
            return Err(String::from(
                "unable to split websocket response at utf8 boundary",
            ));
        }
        framer
            .write(
                stream,
                WebSocketSendMessageType::Text,
                end == bytes.len(),
                &bytes[offset..end],
            )
            .map_err(format_framer_error)?;
        offset = end;
    }
    Ok(())
}

fn read_upgrade_header(
    stream: &mut TcpStream,
    read_buf: &mut [u8],
    read_cursor: &mut usize,
) -> Result<Option<WebSocketContext>, String> {
    loop {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut request = httparse::Request::new(&mut headers);

        let received_size = stream
            .read(&mut read_buf[*read_cursor..])
            .map_err(|error| format!("read header failed: {error}"))?;
        if received_size == 0 {
            return Ok(None);
        }

        let buffer_len = *read_cursor + received_size;
        let parsed = request
            .parse(&read_buf[..buffer_len])
            .map_err(|error| format!("parse header failed: {error}"))?;

        match parsed {
            httparse::Status::Complete(header_len) => {
                let request_path_is_rpc = request.path == Some(WS_PATH);
                let websocket_context = {
                    let headers = request
                        .headers
                        .iter()
                        .map(|header| (header.name, header.value));
                    ws::read_http_header(headers)
                        .map_err(|error| format!("websocket header invalid: {error}"))?
                };

                // Preserve bytes read past the HTTP header. They may already
                // contain the first masked WebSocket frame.
                let remaining = buffer_len - header_len;
                if remaining != 0 {
                    read_buf.copy_within(header_len..buffer_len, 0);
                }
                *read_cursor = remaining;
                println!(
                    "[ws] upgrade header bytes={header_len} total read bytes={buffer_len} buffered frame bytes={remaining}"
                );

                match websocket_context {
                    Some(context) => {
                        if !request_path_is_rpc {
                            return_404_not_found(stream)?;
                            return Ok(None);
                        }
                        return Ok(Some(context));
                    }
                    None => {
                        return_404_not_found(stream)?;
                        return Ok(None);
                    }
                }
            }
            httparse::Status::Partial => {
                *read_cursor += received_size;
                println!("[ws] upgrade partial bytes={read_cursor}");
                if *read_cursor == read_buf.len() {
                    return Err(String::from("request header too large"));
                }
            }
        }
    }
}

fn return_404_not_found(stream: &mut TcpStream) -> Result<(), String> {
    let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream
        .write_all(response.as_bytes())
        .map_err(|error| format!("write 404 failed: {error}"))
}

fn process_rpc(runtime: &Mutex<AgentRuntime>, text: &str) -> String {
    let request: RpcRequest<'_> = match serde_json::from_str(text) {
        Ok(request) => request,
        Err(_) => {
            return rpc_error_null(-32700, "Parse error", None);
        }
    };

    if request.jsonrpc != Some("2.0") {
        return rpc_error(
            request.id.as_ref(),
            -32600,
            "Invalid Request",
            Some("jsonrpc must be \"2.0\""),
        );
    }

    let Some(method) = request.method else {
        return rpc_error(
            request.id.as_ref(),
            -32600,
            "Invalid Request",
            Some("method is required"),
        );
    };

    if method != "device.control" {
        return rpc_error(request.id.as_ref(), -32601, "Method not found", None);
    }

    let Some(params) = request.params else {
        return rpc_error(
            request.id.as_ref(),
            -32602,
            "Invalid params",
            Some("params.message must be a string"),
        );
    };
    let Some(message) = params.message else {
        return rpc_error(
            request.id.as_ref(),
            -32602,
            "Invalid params",
            Some("params.message must be a string"),
        );
    };

    let reply = {
        let mut runtime = match runtime.lock() {
            Ok(runtime) => runtime,
            Err(_) => {
                return rpc_error(
                    request.id.as_ref(),
                    -32000,
                    "Server error",
                    Some("runtime lock poisoned"),
                );
            }
        };
        runtime.run_prompt(message)
    };

    match reply {
        Ok(reply) => rpc_success(request.id.as_ref(), &reply),
        Err(error) => rpc_error(
            request.id.as_ref(),
            -32000,
            "Server error",
            Some(error.as_str()),
        ),
    }
}

fn rpc_error_null(code: i32, message: &str, data: Option<&str>) -> String {
    rpc_error(None, code, message, data)
}

fn rpc_error(id: Option<&RpcId<'_>>, code: i32, message: &str, data: Option<&str>) -> String {
    // Construct the small error object directly. Building a serde_json::Value
    // map here used several temporary heap allocations and could exhaust the
    // ESP32-C3 heap even for a 58-byte request.
    let mut response = String::with_capacity(128);
    response.push_str("{\"jsonrpc\":\"2.0\",\"id\":");
    append_rpc_id(&mut response, id);
    response.push_str(",\"error\":{\"code\":");
    let _ = write!(response, "{code}");
    response.push_str(",\"message\":");
    append_json_string(&mut response, message);
    if let Some(data) = data {
        response.push_str(",\"data\":");
        append_json_string(&mut response, data);
    }
    response.push_str("}}");
    response
}

fn rpc_success(id: Option<&RpcId<'_>>, reply: &str) -> String {
    let mut response = String::with_capacity(reply.len().saturating_add(64));
    response.push_str("{\"jsonrpc\":\"2.0\",\"id\":");
    append_rpc_id(&mut response, id);
    response.push_str(",\"result\":{\"reply\":");
    append_json_string(&mut response, reply);
    response.push_str("}}");
    response
}

fn append_rpc_id(response: &mut String, id: Option<&RpcId<'_>>) {
    match id {
        None => response.push_str("null"),
        Some(RpcId::String(value)) => append_json_string(response, value),
        Some(RpcId::Integer(value)) => {
            let _ = write!(response, "{value}");
        }
        Some(RpcId::Unsigned(value)) => {
            let _ = write!(response, "{value}");
        }
        Some(RpcId::Float(value)) => {
            let _ = write!(response, "{value}");
        }
    }
}

fn append_json_string(response: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    response.push('"');
    for character in value.chars() {
        match character {
            '"' => response.push_str("\\\""),
            '\\' => response.push_str("\\\\"),
            '\u{08}' => response.push_str("\\b"),
            '\u{0c}' => response.push_str("\\f"),
            '\n' => response.push_str("\\n"),
            '\r' => response.push_str("\\r"),
            '\t' => response.push_str("\\t"),
            character if character <= '\u{1f}' => {
                let value = character as u32 as u8;
                response.push_str("\\u00");
                response.push(HEX[(value >> 4) as usize] as char);
                response.push(HEX[(value & 0x0f) as usize] as char);
            }
            character => response.push(character),
        }
    }
    response.push('"');
}

fn format_framer_error(error: FramerError<std::io::Error>) -> String {
    match error {
        FramerError::Io(err) => format!("I/O error: {err}"),
        FramerError::FrameTooLarge(size) => format!("frame too large for buffer: {size}"),
        FramerError::Utf8(err) => format!("utf8 error: {err}"),
        FramerError::HttpHeader(err) => format!("http header error: {err}"),
        FramerError::WebSocket(err) => format!("websocket error: {err}"),
    }
}

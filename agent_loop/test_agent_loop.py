#!/usr/bin/env python3
"""Minimal dependency-free WebSocket JSON-RPC client for agent_loop.

The ESP32 service listens on ws://<device>:9001/rpc and accepts a single
JSON-RPC method: device.control.  This script intentionally uses only Python's
standard library so it can be run directly from a development machine.
"""

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import time
from typing import Optional


DEFAULT_HOST = "10.255.117.24"
DEFAULT_PORT = 9001
DEFAULT_PATH = "/ws"
MAX_FRAME_BYTES = 128 * 1024


class WebSocketError(RuntimeError):
    pass


class WebSocket:
    def __init__(self, sock: socket.socket, verbose: bool = False) -> None:
        self.sock = sock
        self.verbose = verbose
        # TCP may deliver the HTTP 101 response and the first WebSocket data
        # frame in the same recv(). Keep bytes after the HTTP header for
        # recv_frame() instead of silently dropping them.
        self._receive_buffer = bytearray()

    @classmethod
    def connect(
        cls,
        host: str,
        port: int,
        path: str,
        timeout: float,
        verbose: bool,
        initial_frame: Optional[bytes] = None,
    ) -> "WebSocket":
        if verbose:
            print(f"[1/5] TCP connecting to {host}:{port}", flush=True)
        sock = socket.create_connection((host, port), timeout=timeout)
        sock.settimeout(timeout)
        if verbose:
            suffix = " with the first WebSocket frame" if initial_frame else ""
            print(f"[2/5] TCP connected; sending WebSocket upgrade{suffix}", flush=True)
        ws = cls(sock, verbose)
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        ).encode("ascii")
        sock.sendall(request if initial_frame is None else request + initial_frame)

        header = ws._read_until(b"\r\n\r\n", 4096)
        lines = header.decode("latin1").split("\r\n")
        if not lines or not lines[0].startswith("HTTP/1.1 101"):
            raise WebSocketError(f"websocket upgrade failed: {lines[0] if lines else header!r}")
        response_headers = {}
        for line in lines[1:]:
            if ":" in line:
                name, value = line.split(":", 1)
                response_headers[name.strip().lower()] = value.strip()
        expected = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()
        ).decode("ascii")
        if response_headers.get("sec-websocket-accept") != expected:
            raise WebSocketError("invalid Sec-WebSocket-Accept in upgrade response")
        if verbose:
            print("[3/5] WebSocket upgrade accepted", flush=True)
        return ws

    def _read_until(self, marker: bytes, limit: int) -> bytes:
        while marker not in self._receive_buffer:
            chunk = self.sock.recv(512)
            if not chunk:
                raise WebSocketError("connection closed during websocket handshake")
            self._receive_buffer.extend(chunk)
            if len(self._receive_buffer) > limit:
                raise WebSocketError("websocket handshake response is too large")
        header_end = self._receive_buffer.index(marker) + len(marker)
        header = bytes(self._receive_buffer[:header_end])
        del self._receive_buffer[:header_end]
        if self.verbose and self._receive_buffer:
            print(
                f"[3/5] preserved {len(self._receive_buffer)} post-handshake bytes",
                flush=True,
            )
        return header

    def _read_exact(self, size: int) -> bytes:
        data = bytearray()
        if self._receive_buffer:
            buffered_size = min(size, len(self._receive_buffer))
            data.extend(self._receive_buffer[:buffered_size])
            del self._receive_buffer[:buffered_size]
        while len(data) < size:
            chunk = self.sock.recv(size - len(data))
            if not chunk:
                raise WebSocketError("connection closed while reading websocket frame")
            data.extend(chunk)
        return bytes(data)

    @staticmethod
    def encode_frame(payload: bytes, opcode: int = 0x1, final: bool = True) -> bytes:
        if len(payload) > MAX_FRAME_BYTES:
            raise WebSocketError(f"payload exceeds local limit ({MAX_FRAME_BYTES} bytes)")
        first = (0x80 if final else 0) | opcode
        length = len(payload)
        mask_key = os.urandom(4)
        masked = bytes(value ^ mask_key[index % 4] for index, value in enumerate(payload))
        if length < 126:
            header = struct.pack("!BB", first, 0x80 | length)
        elif length <= 0xFFFF:
            header = struct.pack("!BBH", first, 0x80 | 126, length)
        else:
            header = struct.pack("!BBQ", first, 0x80 | 127, length)
        return header + mask_key + masked

    def send_frame(self, payload: bytes, opcode: int = 0x1, final: bool = True) -> None:
        frame = self.encode_frame(payload, opcode, final)
        if self.verbose:
            print(f"[4/6] sending masked WebSocket frame bytes={len(payload)}", flush=True)
        self.sock.sendall(frame)

    def recv_frame(self) -> tuple[bool, int, bytes]:
        first, second = struct.unpack("!BB", self._read_exact(2))
        final = bool(first & 0x80)
        opcode = first & 0x0F
        masked = bool(second & 0x80)
        length = second & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._read_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._read_exact(8))[0]
        if length > MAX_FRAME_BYTES:
            raise WebSocketError(f"received frame exceeds local limit ({length} bytes)")
        mask_key = self._read_exact(4) if masked else b""
        payload = bytearray(self._read_exact(length))
        if masked:
            for index in range(length):
                payload[index] ^= mask_key[index % 4]
        return final, opcode, bytes(payload)

    def recv_text(self) -> str:
        fragments = bytearray()
        expected_opcode: Optional[int] = None
        while True:
            final, opcode, payload = self.recv_frame()
            if opcode == 0x8:  # close
                raise WebSocketError("server closed websocket before sending a response")
            if opcode == 0x9:  # ping
                self.send_frame(payload, opcode=0xA)
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode in (0x1, 0x2):
                if expected_opcode is not None:
                    raise WebSocketError("received a new data frame before final fragment")
                expected_opcode = opcode
            elif opcode != 0x0:
                raise WebSocketError(f"unsupported websocket opcode 0x{opcode:x}")
            fragments.extend(payload)
            if len(fragments) > MAX_FRAME_BYTES:
                raise WebSocketError("assembled websocket message is too large")
            if final:
                if expected_opcode != 0x1:
                    raise WebSocketError("server response is not a text message")
                return fragments.decode("utf-8")

    def close(self) -> None:
        try:
            self.send_frame(struct.pack("!H", 1000), opcode=0x8)
        except (OSError, WebSocketError):
            pass
        self.sock.close()


def build_request(message: str, request_id: int) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "device.control",
        "params": {"message": message},
    }


def build_ws_probe(request_id: int) -> dict:
    """Exercise TCP, WebSocket framing and JSON-RPC without calling the LLM."""
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "link.probe",
        "params": {},
    }


def validate_response(response: object, request_id: int, mode: str) -> None:
    if not isinstance(response, dict):
        raise WebSocketError("response is not a JSON object")
    if response.get("jsonrpc") != "2.0":
        raise WebSocketError("response does not use JSON-RPC 2.0")
    if response.get("id") != request_id:
        raise WebSocketError("response id does not match request id")

    if mode == "ws":
        error = response.get("error")
        if not isinstance(error, dict) or error.get("code") != -32601:
            raise WebSocketError("probe did not receive the expected Method not found response")
        return

    error = response.get("error")
    if isinstance(error, dict):
        code = error.get("code")
        message = error.get("message", "unknown device error")
        data = error.get("data")
        detail = f"{message} (code={code})"
        if data is not None:
            detail += f": {data}"
        raise WebSocketError(f"device returned JSON-RPC error: {detail}")

    result = response.get("result")
    if not isinstance(result, dict) or not isinstance(result.get("reply"), str):
        raise WebSocketError("agent response has no result.reply")


def main() -> int:
    parser = argparse.ArgumentParser(description="Test the agent_loop WebSocket RPC service")
    parser.add_argument(
        "message",
        nargs="?",
        default="打开红灯，然后关闭红灯。",
        help="natural-language device command (default: end-to-end LED request)",
    )
    parser.add_argument("--host", default=DEFAULT_HOST, help=f"ESP32 IP (default: {DEFAULT_HOST})")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--path", default=DEFAULT_PATH)
    parser.add_argument("--timeout", type=float, default=75.0, help="socket timeout in seconds")
    parser.add_argument("--id", type=int, default=1, dest="request_id")
    parser.add_argument(
        "--repeat",
        type=int,
        default=1,
        help="number of serial, fresh WebSocket requests to run (default: 1)",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=1.0,
        help="seconds to wait between repeated requests (default: 1.0)",
    )
    parser.add_argument(
        "--mode",
        choices=("agent", "ws"),
        default="agent",
        help="agent tests the complete device-to-LLM path; ws tests local WS/JSON-RPC only",
    )
    parser.add_argument("--raw-json", help="send a complete JSON-RPC request instead of --message")
    parser.add_argument(
        "--pipeline",
        action="store_true",
        help="send the first WebSocket frame with the HTTP Upgrade request (diagnostic only)",
    )
    parser.add_argument("--quiet", action="store_true", help="only print the final JSON response")
    parser.add_argument("--verbose", action="store_true", help="print connection and send progress")
    args = parser.parse_args()
    verbose = args.verbose or not args.quiet
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")
    if args.interval < 0:
        parser.error("--interval must not be negative")

    if args.raw_json:
        try:
            request = json.loads(args.raw_json)
        except json.JSONDecodeError as error:
            parser.error(f"--raw-json is not valid JSON: {error}")
    elif args.mode == "ws":
        request = build_ws_probe(args.request_id)
    else:
        message = args.message.strip()
        if not message:
            parser.error("message must not be empty")
        request = build_request(message, args.request_id)

    payload = json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    for attempt in range(1, args.repeat + 1):
        ws: Optional[WebSocket] = None
        started_at = time.monotonic()
        try:
            if args.repeat > 1:
                print(f"[{attempt}/{args.repeat}] starting serial request", flush=True)
            initial_frame = WebSocket.encode_frame(payload) if args.pipeline else None
            ws = WebSocket.connect(
                args.host, args.port, args.path, args.timeout, verbose, initial_frame
            )
            if args.pipeline:
                if verbose:
                    print(
                        f"[4/6] masked WebSocket frame was sent with the upgrade bytes={len(payload)}",
                        flush=True,
                    )
            else:
                ws.send_frame(payload)
            if verbose:
                print(f"[5/6] JSON-RPC frame sent bytes={len(payload)}", flush=True)
                print("[6/6] waiting for JSON-RPC response", flush=True)
            response_text = ws.recv_text()
            response = json.loads(response_text)
            print(json.dumps(response, ensure_ascii=False, indent=2))
            validate_response(response, args.request_id, args.mode)
            elapsed = time.monotonic() - started_at
            print(f"PASS: {args.mode} path completed in {elapsed:.2f}s", flush=True)
        except (OSError, WebSocketError, UnicodeError, json.JSONDecodeError) as error:
            print(f"test failed on request {attempt}/{args.repeat}: {error}", file=sys.stderr)
            return 1
        finally:
            if ws is not None:
                ws.close()

        if attempt < args.repeat and args.interval:
            time.sleep(args.interval)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

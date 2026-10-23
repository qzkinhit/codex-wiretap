#!/usr/bin/env python3
"""Local Codex HTTP/SSE/WebSocket metadata observer. Python 3.11+."""
from __future__ import annotations

import argparse
import asyncio
import codecs
from contextlib import closing
import json
import os
from pathlib import Path
import re
import shlex
import ssl
import sqlite3
import sys
import time
import tomllib
import uuid
import zlib
from urllib.parse import urlsplit

import aiohttp
from aiohttp import web, WSMsgType
from multidict import CIMultiDict
from yarl import URL
from conversations import ConversationIndex
try:
    from compression import zstd
except ImportError:
    from backports import zstd

ROOT = Path(__file__).resolve().parent
DEFAULT_CONFIG = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "config.toml"
LIMIT = 4 * 1024 * 1024
HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
       "te", "trailer", "transfer-encoding", "upgrade", "host"}
TERMINAL = {"response.completed", "response.failed", "response.incomplete", "response.done"}


class ProviderCredentialsError(Exception):
    """No valid credential for the fixed upstream; never include the secret."""


class CCKeyLoader:
    def __init__(self, provider_id, upstream, db_path=None):
        self.provider_id, self.upstream, self.db_path = provider_id, URL(upstream), db_path

    def __call__(self):
        try:
            upstream, key = cc_provider(self.provider_id, self.db_path)
            if URL(upstream) != self.upstream:
                raise ValueError('upstream_changed')
            return key
        except (OSError, ValueError, TypeError, KeyError, sqlite3.Error):
            raise ProviderCredentialsError() from None


def stamp():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def obj(value):
    return value if isinstance(value, dict) else {}


def label(value):
    # Never persist arbitrary text, headers, prompts, outputs, or error messages.
    if isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_.:/@+\-]{1,180}", value):
        return value
    return None


def number(value):
    return value if type(value) is int and value >= 0 else None


def parse_json(data):
    try:
        return obj(json.loads(data))
    except (ValueError, UnicodeError, RecursionError):
        return {}


def request_fields(data):
    data = obj(data)
    if isinstance(data.get("response"), dict):
        data = data["response"]
    return {
        "model": label(data.get("model")),
        "reasoning_effort": label(obj(data.get("reasoning")).get("effort", data.get("reasoning_effort"))),
        "service_tier": label(data.get("service_tier")),
    }


def response_fields(data):
    data = obj(data)
    if isinstance(data.get("response"), dict):
        data = data["response"]
    usage = obj(data.get("usage"))
    details = obj(usage.get("output_tokens_details", usage.get("completion_tokens_details")))
    return {
        "id": label(data.get("id")),
        "model": label(data.get("model")),
        "reasoning_effort": label(obj(data.get("reasoning")).get("effort", data.get("reasoning_effort"))),
        "service_tier": label(data.get("service_tier")),
        "status": label(data.get("status")),
        "input_tokens": number(usage.get("input_tokens", usage.get("prompt_tokens"))),
        "output_tokens": number(usage.get("output_tokens", usage.get("completion_tokens"))),
        "reasoning_tokens": number(details.get("reasoning_tokens")),
        "error_type": label(obj(data.get("error")).get("type")),
        "error_code": label(obj(data.get("error")).get("code")),
    }


def compare(a, b):
    if a is None or b is None:
        return "unknown"
    return "same" if a == b else "different"


class ZstdStream:
    """Accept concatenated zstd frames without unbounded decompression."""
    def __init__(self):
        self.decoder = zstd.ZstdDecompressor()

    def decompress(self, data, limit):
        output = bytearray()
        while data and len(output) < limit:
            if self.decoder.eof:
                self.decoder = zstd.ZstdDecompressor()
            output.extend(self.decoder.decompress(data, limit - len(output)))
            data = self.decoder.unused_data if self.decoder.eof else b""
        return bytes(output)


class Journal:
    """Append only whitelisted snapshots; keep the latest 300 rows in memory."""
    def __init__(self, path, conversations=None):
        self.conversations = conversations or ConversationIndex()
        self.path = Path(path)
        self.control_path = self.path.with_suffix('.control.json')
        self.enabled = True
        try:
            state = json.loads(self.control_path.read_text())
            if type(state.get('enabled')) is bool:
                self.enabled = state['enabled']
        except (OSError, ValueError, AttributeError):
            pass
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd = os.open(self.path, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        os.fchmod(fd, 0o600)
        self.file = os.fdopen(fd, "a", encoding="utf-8")
        self.rows = {}

    def save(self, row):
        if not row.get('_capture', True):
            return
        row["model_comparison"] = compare(row["request"].get("model"), row["response"].get("model"))
        row["effort_comparison"] = compare(row["request"].get("reasoning_effort"), row["response"].get("reasoning_effort"))
        row["actual_execution"] = "unverifiable_from_client_capture"
        self.rows[row["id"]] = row
        while len(self.rows) > 300:
            del self.rows[next(iter(self.rows))]
        self.file.write(json.dumps({k:v for k,v in row.items() if not k.startswith('_')}, ensure_ascii=False) + "\n")
        self.file.flush()

    def new(self, data, transport, endpoint, headers=None):
        row = {"id": uuid.uuid4().hex[:16], "time": stamp(), "transport": transport,
               "_capture": self.enabled,
               "endpoint": endpoint, "phase": "request", "request": request_fields(data),
               "response": {}, "notes": [], "http_status": None}
        row['conversation'] = self.conversations.identify(data, headers) if self.enabled else {'id': None, 'status': 'unidentified', 'source': None}
        self.save(row)
        return row

    def observe(self, row, data):
        if not row.get('_capture', True):
            return
        kind = data.get("type")
        if kind in TERMINAL:
            row["terminal_event"] = kind
        fields = response_fields(data)
        if not any(v is not None for v in fields.values()) and kind not in TERMINAL:
            return
        row["response"].update({k: v for k, v in fields.items() if v is not None})
        row["phase"] = "response"
        self.save(row)

    def finish(self, row, phase="finished"):
        if row.get("terminal_event"):
            phase = "error" if row["terminal_event"] == "response.failed" else "finished"
        row["phase"] = phase
        self.save(row)

    def close(self):
        self.file.close()

    def set_enabled(self, enabled):
        tmp = self.control_path.with_name(self.control_path.name + '.' + uuid.uuid4().hex)
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as f:
            json.dump({'enabled': enabled}, f)
        os.replace(tmp, self.control_path)
        self.enabled = enabled


class SSE:
    """Incremental UTF-8/event parsing with bounded memory; supports CR/LF/CRLF."""
    def __init__(self, emit):
        self.emit = emit
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.line = []
        self.line_size = 0
        self.lines = []
        self.size = 0
        self.skip = False
        self.cr = False
        self.truncated = False

    def end_line(self):
        line, self.line = "".join(self.line), []
        size, self.line_size = self.line_size, 0
        if not size:
            if not self.skip and self.lines:
                data = parse_json("\n".join(self.lines))
                if data:
                    self.emit(data)
            self.lines, self.size, self.skip = [], 0, False
        elif not self.skip and line.startswith("data:"):
            self.lines.append(line[5:].removeprefix(" "))

    def feed(self, chunk):
        text = self.decoder.decode(chunk)
        if not text:
            return
        if self.cr and text.startswith("\n"):
            text = text[1:]
        self.cr = text.endswith("\r")
        parts = re.split(r"\r\n|\r|\n", text)
        for i, part in enumerate(parts):
            self.line_size += len(part)
            self.size += len(part)
            if self.size > LIMIT:
                self.line, self.lines, self.skip, self.truncated = [], [], True, True
            elif not self.skip:
                self.line.append(part)
            if i < len(parts) - 1:
                self.end_line()

    def finish(self):
        # SSE dispatch requires a blank line; an interrupted event is not evidence.
        if self.line_size or self.lines or self.skip:
            self.truncated = True


class Observer:
    """Decode a bounded *copy*. The forwarded body remains byte-for-byte intact."""
    def __init__(self, journal, row, content_type, encoding):
        self.journal, self.row = journal, row
        self.sse = SSE(lambda d: journal.observe(row, d)) if "text/event-stream" in content_type else None
        self.data = bytearray()
        self.disabled = False
        self.decoder = None
        if encoding == "gzip":
            self.decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
        elif encoding == "deflate":
            self.decoder = zlib.decompressobj()
        elif encoding == "zstd":
            self.decoder = ZstdStream()
        elif encoding not in ("", "identity"):
            self.disabled = True
            row["notes"].append("unsupported_content_encoding")

    def feed(self, chunk):
        if self.disabled:
            return
        try:
            if self.decoder:
                chunk = self.decoder.decompress(chunk, LIMIT + 1)
                if len(chunk) > LIMIT or getattr(self.decoder, "unconsumed_tail", b""):
                    self.row["notes"].append("observation_limit_exceeded")
                    self.disabled = True
                    return
        except (zlib.error, zstd.ZstdError):
            self.row["notes"].append("invalid_compressed_body")
            self.disabled = True
            return
        if self.sse:
            self.sse.feed(chunk)
        elif len(self.data) + len(chunk) <= LIMIT:
            self.data.extend(chunk)
        else:
            self.data.clear()
            self.disabled = True
            self.row["notes"].append("observation_limit_exceeded")

    def finish(self):
        if self.disabled:
            return
        if self.sse:
            self.sse.finish()
            if self.sse.truncated:
                self.row["notes"].append("incomplete_or_oversized_sse_event")
        else:
            self.journal.observe(self.row, parse_json(self.data))


def clean_headers(headers, websocket=False):
    blocked = HOP | {v.strip().lower() for v in headers.get("Connection", "").split(",")}
    if websocket:
        blocked |= {"content-length", "sec-websocket-key", "sec-websocket-version",
                    "sec-websocket-extensions", "sec-websocket-protocol", "sec-websocket-accept"}
    return CIMultiDict((k, v) for k, v in headers.items() if k.lower() not in blocked)


class WSObserver:
    def __init__(self, journal, endpoint, headers=None):
        self.journal, self.endpoint = journal, endpoint
        self.headers = headers
        self.pending = []
        self.active = {}

    def client(self, data):
        if data.get("type") == "response.create" or ("model" in data and not data.get("type")):
            self.pending.append(self.journal.new(data, "WebSocket", self.endpoint, self.headers))
        # Do not guess inherited configuration from absent request fields.

    def server(self, data):
        kind = data.get("type")
        response = obj(data.get("response"))
        rid = label(response.get("id") or data.get("response_id"))
        row = self.active.get(rid) if rid else None
        if row is None and (kind == "response.created" or kind in TERMINAL):
            if len(self.pending) == 1 and not self.active:
                row = self.pending.pop()
            else:
                row = self.journal.new({}, "WebSocket", self.endpoint)
                row["notes"].append("unpaired_response")
                for pending in self.pending:
                    pending["notes"].append("ambiguous_websocket_pairing")
                    self.journal.finish(pending, "unpaired")
                self.pending.clear()
            if rid:
                self.active[rid] = row
        if kind == "error" and row is None and len(self.pending) == 1 and not self.active:
            row = self.pending.pop()
        if row is not None:
            self.journal.observe(row, data)
            if kind in TERMINAL or kind == "error":
                self.journal.finish(row, "error" if kind in {"error", "response.failed"} else "finished")
                if rid:
                    self.active.pop(rid, None)

    def close(self):
        for row in [*self.pending, *self.active.values()]:
            row["notes"].append("connection_closed_before_terminal_event")
            self.journal.finish(row, "disconnected")


class Proxy:
    def __init__(self, upstream, journal, config_path=DEFAULT_CONFIG, profile=None, api_key=None, outbound_proxy=None, key_loader=None):
        self.key_loader = key_loader
        self.api_key, self.outbound_proxy = api_key, outbound_proxy
        self.config_path, self.profile = Path(config_path), profile
        self.upstream = URL(upstream)
        if self.upstream.scheme not in {"http", "https"} or not self.upstream.host:
            raise ValueError("上游必须是完整的 HTTP(S) base_url")
        if self.upstream.user or self.upstream.password or self.upstream.query_string or self.upstream.fragment:
            raise ValueError("上游 URL 不得包含凭证、查询串或片段")
        self.journal = journal
        self.base = self.upstream.raw_path.rstrip("/")
        self.started = stamp()

    async def lifecycle(self, app):
        # TLS verification stays enabled; SSL_CERT_FILE may select a custom CA bundle.
        tls = ssl.create_default_context()
        connector = aiohttp.TCPConnector(ssl=tls, limit=0)
        async with aiohttp.ClientSession(connector=connector, auto_decompress=False,
                                         timeout=aiohttp.ClientTimeout(total=None, sock_connect=20),
                                         cookie_jar=aiohttp.DummyCookieJar(), trust_env=False,
                                         skip_auto_headers={"User-Agent", "Content-Type", "Accept-Encoding"}) as session:
            self.session = session
            yield

    def app(self):
        @web.middleware
        async def local_guard(request, handler):
            if request.remote not in {"127.0.0.1", "::1"} or request.url.host not in {"localhost", "127.0.0.1", "::1"}:
                raise web.HTTPForbidden(text="仅允许本机访问")
            origin = request.headers.get("Origin")
            if origin and origin != str(request.url.origin()):
                raise web.HTTPForbidden(text="不允许跨站请求")
            return await handler(request)

        app = web.Application(client_max_size=64 * 1024 * 1024, middlewares=[local_guard])
        app.cleanup_ctx.append(self.lifecycle)
        app.router.add_get("/__wiretap__/", self.dashboard)
        app.router.add_get("/__wiretap__/api/records", self.records)
        app.router.add_post("/__wiretap__/api/recording", self.recording)
        app.router.add_get("/favicon.ico", self.favicon)
        app.router.add_route("*", "/{tail:.*}", self.forward)
        return app

    async def dashboard(self, request):
        return web.Response(text=(ROOT / "dashboard.html").read_text(), content_type="text/html",
                            headers={"Cache-Control": "no-store", "X-Frame-Options": "DENY",
                                     "Content-Security-Policy": "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'"})

    async def favicon(self, request):
        return web.Response(status=204)

    async def records(self, request):
        expected = str(request.url.origin()) + self.base
        connection = connection_status(self.config_path, expected, self.profile)
        return web.json_response({"started": self.started, "upstream": str(self.upstream),
                                  "recording": self.journal.enabled,
                                  "connection": connection,
                                  "records": self.journal.conversations.enrich(list(reversed(self.journal.rows.values())))},
                                 headers={"Cache-Control": "no-store"})

    async def recording(self, request):
        if request.headers.get('X-Wiretap-Control') != '1' or request.content_type != 'application/json':
            raise web.HTTPForbidden(text='必须通过本地面板控制')
        try:
            data = await request.json()
        except (ValueError, UnicodeError):
            raise web.HTTPBadRequest(text='无效 JSON')
        if not isinstance(data, dict) or type(data.get('enabled')) is not bool:
            raise web.HTTPBadRequest(text='enabled 必须为布尔值')
        self.journal.set_enabled(data['enabled'])
        return web.json_response({'recording': self.journal.enabled, 'forwarding': True})

    def destination(self, request):
        path = request.rel_url.raw_path
        if self.base and path != self.base and not path.startswith(self.base + "/"):
            raise web.HTTPNotFound(text="请求路径不在配置的上游 base_url 下")
        # Fixed origin: never interpret client path/Host/query as another upstream.
        return URL(str(self.upstream.origin()) + request.raw_path, encoded=True)

    def upstream_headers(self, request, websocket=False):
        headers = clean_headers(request.headers, websocket)
        key = self.key_loader() if self.key_loader else self.api_key
        if key:
            headers['Authorization'] = 'Bearer ' + key
            for name in ['Cookie', 'ChatGPT-Account-ID', 'OpenAI-Organization', 'OpenAI-Project']:
                headers.popall(name, None)
        return headers

    async def forward(self, request):
        target = self.destination(request)
        if request.headers.get("Upgrade", "").lower() == "websocket":
            return await self.websocket(request, target)
        endpoint = "responses" if request.path.rstrip("/").endswith("/responses") else "chat.completions" if request.path.rstrip("/").endswith("/chat/completions") else None
        body = await request.read()
        data = body
        encoding = request.headers.get("Content-Encoding", "")
        parse_note = None
        if encoding in {"gzip", "deflate", "zstd"}:
            try:
                dec = ZstdStream() if encoding == "zstd" else zlib.decompressobj(16 + zlib.MAX_WBITS if encoding == "gzip" else zlib.MAX_WBITS)
                data = dec.decompress(body, LIMIT + 1)
            except (zlib.error, zstd.ZstdError):
                parse_note = "invalid_compressed_request"
                data = b""
        elif encoding not in {"", "identity"}:
            data = b""
            parse_note = "unsupported_request_encoding"
        row = self.journal.new(parse_json(data) if len(data) <= LIMIT else {}, "HTTP", endpoint, request.headers) if endpoint and request.method == "POST" else None
        if row and (parse_note or len(data) > LIMIT):
            row["notes"].append(parse_note or "request_observation_limit_exceeded")
            self.journal.save(row)
        start = time.monotonic()
        downstream = None
        try:
            async with self.session.request(request.method, target, data=body,
                                            headers=self.upstream_headers(request), proxy=self.outbound_proxy, allow_redirects=False) as upstream:
                downstream = web.StreamResponse(status=upstream.status, reason=upstream.reason,
                                                headers=clean_headers(upstream.headers))
                observer = None
                if row:
                    row["http_status"] = upstream.status
                    row["transport"] = "SSE" if "text/event-stream" in upstream.headers.get("Content-Type", "") else "HTTP"
                    self.journal.save(row)
                    observer = Observer(self.journal, row, upstream.headers.get("Content-Type", ""), upstream.headers.get("Content-Encoding", ""))
                await downstream.prepare(request)
                async for chunk in upstream.content.iter_any():
                    await downstream.write(chunk)
                    if observer:
                        observer.feed(chunk)
                if observer:
                    observer.finish()
                await downstream.write_eof()
                if row:
                    row["duration_ms"] = round((time.monotonic() - start) * 1000)
                    self.journal.finish(row, "error" if upstream.status >= 400 else "finished")
                return downstream
        except ProviderCredentialsError:
            if row:
                row['notes'].append('cc_credentials_unavailable_or_upstream_changed')
                self.journal.finish(row, 'error')
            return web.json_response({'error': {'type': 'wiretap_credentials_error', 'message': '无法读取原供应商有效凭据，或上游地址已改变。检查 CC Switch 原供应商；更换上游地址后需重新加载服务。'}}, status=502)
        except (aiohttp.ClientError, OSError, asyncio.TimeoutError):
            if row:
                row["notes"].append("transport_error")
                self.journal.finish(row, "error")
            if downstream and downstream.prepared:
                if request.transport:
                    request.transport.close()
                return downstream
            return web.json_response({"error": {"type": "wiretap_upstream_error", "message": "无法连接上游，检查上游地址和 TLS 配置。"}}, status=502)
        except asyncio.CancelledError:
            if row:
                if not row.get("terminal_event"):
                    row["notes"].append("client_disconnected")
                self.journal.finish(row, "disconnected")
            raise

    async def websocket(self, request, target):
        endpoint = "responses" if request.path.rstrip("/").endswith("/responses") else "other"
        observer = WSObserver(self.journal, endpoint, request.headers)
        protocols = [p.strip() for p in request.headers.get("Sec-WebSocket-Protocol", "").split(",") if p.strip()]
        try:
            upstream = await self.session.ws_connect(target, headers=self.upstream_headers(request, True), proxy=self.outbound_proxy,
                                                     protocols=protocols, compress=0, autoping=False, max_msg_size=64 * 1024 * 1024)
        except ProviderCredentialsError:
            row = self.journal.new({}, 'WebSocket', endpoint, request.headers)
            row['notes'].append('cc_credentials_unavailable_or_upstream_changed')
            self.journal.finish(row, 'error')
            return web.Response(status=502, text='无法读取原供应商有效凭据，或上游地址已改变')
        except aiohttp.WSServerHandshakeError as error:
            # Preserve status (e.g. 426) so Codex can fall back to HTTP.
            row = self.journal.new({}, "WebSocket", endpoint, request.headers)
            row["http_status"] = error.status
            row["notes"].append("websocket_handshake_rejected")
            self.journal.finish(row, "error")
            return web.Response(status=error.status, text="WebSocket 上游握手被拒绝")
        except (aiohttp.ClientError, OSError, asyncio.TimeoutError):
            return web.Response(status=502, text="WebSocket 上游连接失败")
        downstream = web.WebSocketResponse(protocols=[upstream.protocol] if upstream.protocol else (),
                                           compress=False, autoping=False, max_msg_size=64 * 1024 * 1024)

        async def relay(source, dest, observe):
            async for message in source:
                if message.type == WSMsgType.TEXT:
                    if len(message.data) <= LIMIT:
                        observe(parse_json(message.data))
                    await dest.send_str(message.data)
                elif message.type == WSMsgType.BINARY:
                    if len(message.data) <= LIMIT:
                        observe(parse_json(message.data))
                    await dest.send_bytes(message.data)
                elif message.type == WSMsgType.PING:
                    await dest.ping(message.data)
                elif message.type == WSMsgType.PONG:
                    await dest.pong(message.data)
                elif message.type == WSMsgType.ERROR:
                    break

        tasks = []
        try:
            await downstream.prepare(request)
            tasks = [asyncio.create_task(relay(downstream, upstream, observer.client)),
                     asyncio.create_task(relay(upstream, downstream, observer.server))]
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            observer.close()
            await upstream.close()
            await downstream.close()
        return downstream


def configuration(path, profile=None):
    d = tomllib.loads(Path(path).read_text())
    profile = profile or d.get("profile")
    selected = d
    if profile:
        if not re.fullmatch(r"[A-Za-z0-9_-]+", profile):
            raise ValueError("无效的 profile 名称")
        overlay_path = Path(path).parent / f"{profile}.config.toml"
        overlay = tomllib.loads(overlay_path.read_text()) if overlay_path.exists() else obj(d.get("profiles", {}).get(profile))
        if not overlay:
            raise ValueError("找不到指定的 profile")
        def merge(base, extra):
            result = dict(base)
            for key, value in extra.items():
                result[key] = merge(result[key], value) if isinstance(result.get(key), dict) and isinstance(value, dict) else value
            return result
        selected = merge(d, overlay)
    provider = selected.get("model_provider", "openai")
    info = obj(selected.get("model_providers", {}).get(provider))
    upstream = info.get("base_url") or selected.get("openai_base_url")
    if provider == "openai" and not upstream:
        # ChatGPT login endpoints differ. Never silently route credentials to a guessed URL.
        upstream = os.environ.get("OPENAI_BASE_URL")
    provider_key = provider if re.fullmatch(r"[A-Za-z0-9_-]+", provider) else json.dumps(provider)
    key = "openai_base_url" if provider == "openai" else "model_providers." + provider_key + ".base_url"
    return {"model": selected.get("model"), "reasoning_effort": selected.get("model_reasoning_effort"),
            "provider": provider, "profile": profile, "upstream": upstream, "override_key": key}


def read_report(path):
    rows = {}
    with Path(path).open() as f:
        for line in f:
            try:
                row = json.loads(line)
                rows[row["id"]] = row
            except (ValueError, KeyError, TypeError):
                continue
    return list(rows.values())


def connection_status(path, expected, profile=None):
    result = {"config_file": str(path), "expected_base_url": expected, "configured": None}
    try:
        config = configuration(path, profile)
        current = urlsplit(config.get("upstream") or "")
        target = urlsplit(expected)
        host = current.hostname
        same_host = host == target.hostname or host in {"localhost", "127.0.0.1"} and target.hostname in {"localhost", "127.0.0.1"}
        result.update({"provider": label(config.get("provider")), "configured_host": host,
                       "configured_port": current.port,
                       "configured": bool(same_host and current.scheme == target.scheme and current.port == target.port and current.path.rstrip("/") == target.path.rstrip("/"))})
    except (OSError, ValueError, TypeError, KeyError):
        result["error"] = "config_unreadable"
    return result


def settings(args):
    try:
        config = configuration(args.config, args.profile)
    except FileNotFoundError:
        config = {"upstream": None, "override_key": "openai_base_url"}
    upstream = args.upstream or config["upstream"]
    if not upstream:
        raise ValueError("未找到明确的上游地址，请用 --upstream 指定当前 provider 的 base_url")
    parsed = urlsplit(upstream)
    if parsed.hostname in {"127.0.0.1", "localhost", "::1"} and parsed.port == args.port:
        raise ValueError("代理和上游端口相同，会产生循环")
    base = f"http://127.0.0.1:{args.port}" + parsed.path.rstrip("/")
    return config, upstream, base


def cc_provider(provider_id, db_path=None):
    db_path = Path(db_path or Path.home() / '.cc-switch/cc-switch.db')
    with closing(sqlite3.connect(db_path.resolve().as_uri() + '?mode=ro', uri=True, timeout=.1)) as db:
        db.execute('PRAGMA query_only=ON')
        row = db.execute("SELECT settings_config FROM providers WHERE id=? AND app_type='codex'", (provider_id,)).fetchone()
        if not row:
            raise ValueError('CC Switch 供应商不存在')
        data = json.loads(row[0])
        config = tomllib.loads(data['config'])
        upstream = config['model_providers'][config['model_provider']]['base_url']
        key = data.get('auth', {}).get('OPENAI_API_KEY')
        parsed = urlsplit(upstream)
        if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment or not isinstance(key, str) or not key:
            raise ValueError('原供应商必须具有 HTTPS 上游和有效的 API Key')
        return upstream, key


async def run(args):
    key = None
    if args.cc_provider_id:
        args.upstream, key = cc_provider(args.cc_provider_id)
    config, upstream, base = settings(args)
    journal = Journal(args.log, ConversationIndex(args.codex_home, titles=not args.no_conversation_titles))
    key_loader = CCKeyLoader(args.cc_provider_id, upstream) if args.cc_provider_id else None
    proxy = Proxy(upstream, journal, args.config, args.profile, api_key=key if not key_loader else None, outbound_proxy=args.outbound_proxy, key_loader=key_loader)
    runner = web.AppRunner(proxy.app(), access_log=None, auto_decompress=False, handler_cancellation=True)
    await runner.setup()
    try:
        await web.TCPSite(runner, "127.0.0.1", args.port).start()
        print(f"实时页面  http://127.0.0.1:{args.port}/__wiretap__/", flush=True)
        print(f"转发上游  {upstream}", flush=True)
        print(f"监测地址  {base}", flush=True)
        override = config["override_key"] + "=" + json.dumps(base)
        print(f"临时接入  codex -c {shlex.quote(override)}", flush=True)
        print("仅保存模型、强度、用量等白名单字段；实际后端执行不可由客户端独立验证。", flush=True)
        if args.action == "run":
            command = args.command
            if command and command[0] == "--":
                command = command[1:]
            if not command:
                candidates = [Path('/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex'),
                              Path('/Applications/Codex.app/Contents/Resources/codex')]
                command = [str(next((p for p in candidates if p.is_file()), 'codex'))]
            profile_args = ["-p", args.profile] if args.profile else []
            proc = await asyncio.create_subprocess_exec(command[0], *profile_args, "-c", override, *command[1:])
            try:
                return await proc.wait()
            finally:
                if proc.returncode is None:
                    proc.terminate()
                    await proc.wait()
        await asyncio.Event().wait()
    finally:
        await runner.cleanup()
        journal.close()


def main():
    parser = argparse.ArgumentParser(description="Codex 请求/响应模型与推理强度监测（HTTP、SSE、WebSocket）")
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--profile", help="读取 Codex 指定 profile 的配置")
    parser.add_argument("--upstream", help="真实上游 base_url；默认读取 Codex 配置")
    parser.add_argument("--cc-provider-id", help="只读加载 CC Switch 原供应商的 HTTPS 上游与 API Key，不在日志中输出 Key")
    parser.add_argument("--outbound-proxy", help="显式指定连接外部上游的 HTTP 代理，与模型网关不同")
    parser.add_argument("--port", type=int, default=10812)
    parser.add_argument("--codex-home", type=Path, help="用于只读查询对话标题的 Codex 数据目录，默认 CODEX_HOME 或 ~/.codex")
    parser.add_argument("--no-conversation-titles", action='store_true', help="仅展示对话 ID 和链接，不读取本地标题或项目名")
    parser.add_argument("--log", type=Path, default=ROOT / "data" / "capture.jsonl")
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("serve", help="启动代理和实时页面，Ctrl+C 停止")
    sub.add_parser("run", help="临时覆盖 provider 地址并运行 Codex，不修改配置文件").add_argument("command", nargs=argparse.REMAINDER)
    sub.add_parser("inspect", help="只显示配置，不代表已发出的请求")
    sub.add_parser("report", help="汇总捕获记录，每个请求一行")
    sub.add_parser("connection", help="显示桌面客户端接入设置")
    args = parser.parse_args()
    try:
        if args.action == "inspect":
            c = configuration(args.config, args.profile)
            # Never print URL credentials or query parameters from user configuration.
            u = urlsplit(c.get("upstream") or "")
            c["upstream"] = f"{u.scheme}://{u.hostname}:{u.port or (443 if u.scheme == 'https' else 80)}{u.path}" if u.hostname else None
            print(json.dumps(c, ensure_ascii=False, indent=2))
            print("这是静态配置；会话和命令行参数可能覆盖它。")
        elif args.action == "report":
            for row in read_report(args.log):
                req, res = row["request"], row["response"]
                print(f"{row['time']} {row['transport']} {row['phase']}\n"
                      f"  请求模型 {req.get('model') or '未知'} / 强度 {req.get('reasoning_effort') or '未发送'}\n"
                      f"  响应模型 {res.get('model') or '未知'} / 强度 {res.get('reasoning_effort') or '未返回'} / 推理 tokens {res.get('reasoning_tokens', '未知')}")
            print("响应字段是上游声明；实际后端模型和执行强度无法独立验证。")
        elif args.action == "connection":
            c, upstream, base = settings(args)
            print(f"配置文件  {args.config}\n当前上游  {upstream}\n临时 base_url  {base}\n覆盖键  {c['override_key']}")
            print("先启动 serve，再修改当前 provider 的 base_url 并重新加载客户端。停止代理前恢复原地址。")
            print("本命令不会修改配置。已经启动的客户端可能仍使用旧配置。")
        else:
            return asyncio.run(run(args)) or 0
    except KeyboardInterrupt:
        return 130
    except (ValueError, OSError) as error:
        # Avoid printing exception strings that may contain upstream URLs or headers.
        print(f"启动失败（{type(error).__name__}）。检查端口、路径、配置格式和 --upstream。", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

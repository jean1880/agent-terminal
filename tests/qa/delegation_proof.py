#!/usr/bin/env python3
"""Opt-in installed-provider delegation proofs, with a deterministic loopback model.

Run through ``make delegation-proof PROOF_ARGS=--dry-run`` first. No cloud model,
credentials or live user configuration is used. The real installed CLIs enforce their
permissions, launch real fixture MCP subprocesses and persist their normal session files.
This proves those CLI mechanisms; it is not a cloud-model or full GUI route test.
"""

import argparse
import json
import os
from pathlib import Path
import queue
import re
import secrets
import shlex
import shutil
import signal
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parent))
import record_frames as rec


HERE = Path(__file__).resolve()
NATIVE_TOOLS = {"spawn_agent", "send_input", "wait", "close_agent", "resume_agent"}


def export_proof_fixtures(source, destination):
    """Publish gate projections, never provider transcripts or incidental model prose."""
    names = {"claude", "codex", "codex-parent", "codex-parent-resume", "codex-native-control",
             "agy", "agy-hook", "agy-mcp"}
    recordings = {path.stem: [json.loads(line) for line in path.read_text().splitlines()]
                  for path in source.glob("*.jsonl") if path.stem in names}
    times = [entry["at"] for entries in recordings.values() for entry in entries
             if isinstance(entry.get("at"), (int, float))]
    origin = min(times, default=0)
    identifiers = {}

    def identifier(value):
        if not isinstance(value, str) or not value:
            return None
        if value not in identifiers:
            identifiers[value] = f"proof-id-{len(identifiers) + 1}"
        return identifiers[value]

    def arguments(value):
        # Every retained string was supplied by this harness's own fixed test request.
        if value in ({}, {"message": "OK"}, {"command": "touch proof-denied"}):
            return value
        if isinstance(value, dict) and value.get("command") == "touch proof-denied":
            return {"command": "touch proof-denied"}
        return None

    def parameters(value):
        result = {}
        if value.get("ServerName") in {"agent_terminal", "other"}:
            result["ServerName"] = value["ServerName"]
        if value.get("ToolName") == "echo":
            result["ToolName"] = "echo"
        if "Arguments" in value:
            safe = arguments(value["Arguments"])
            if safe is not None:
                result["Arguments"] = safe
        return result

    def project(entry, name):
        out = {}
        if "at" in entry:
            out["at"] = round(entry["at"] - origin, 6)
        if name == "agy-mcp":
            if entry.get("event") != "call":
                return None
            meta = entry.get("meta", {})
            safe = arguments(entry.get("arguments"))
            if entry.get("tool") != "echo" or safe is None:
                return None
            progress = str(meta.get("progressToken", ""))
            prefix, _, index = progress.rpartition(":")
            if not prefix or not index.isdigit():
                return None
            out.update(event="call", tool="echo", arguments=safe, meta={
                "antigravity.google/conversation_id": identifier(meta.get("antigravity.google/conversation_id")),
                "progressToken": f"{identifier(prefix)}:{index}"})
            return out
        if name == "agy-hook":
            payload = entry.get("payload", {})
            tool = payload.get("toolCall", {})
            if tool.get("name") not in {"call_mcp_tool", "invoke_subagent", "define_subagent"}:
                return None
            out["payload"] = {"conversationId": identifier(payload.get("conversationId")),
                              "stepIdx": payload.get("stepIdx"),
                              "toolCall": {"name": tool["name"], "args": parameters(tool.get("args", {}))}}
            return out
        frame = entry.get("frame", {})
        if entry.get("dir") == "stdin" and name == "claude" and frame.get("type") == "control_response":
            behaviour = frame.get("response", {}).get("response", {}).get("behavior")
            if behaviour in {"allow", "deny"}:
                out.update(dir="stdin", frame={"type": "control_response", "response": {"behavior": behaviour}})
                return out
        if entry.get("dir") == "stdin" and name.startswith("codex") and frame.get("method") in {"thread/start", "thread/resume"}:
            params = frame.get("params", {})
            config = params.get("config", {})
            safe_config = {key: value for key, value in config.items()
                           if (key.startswith("mcp_servers.") and key.endswith(".enabled")
                               or key in {"features.multi_agent", "features.multi_agent_v2"}) and isinstance(value, bool)}
            safe_params = {"config": safe_config}
            if params.get("approvalPolicy") in {"never", "untrusted", "on-request", "on-failure"}:
                safe_params["approvalPolicy"] = params["approvalPolicy"]
            if isinstance(params.get("sandbox"), str) and params["sandbox"] in {"read-only", "workspace-write", "danger-full-access"}:
                safe_params["sandbox"] = params["sandbox"]
            out.update(dir="stdin", frame={"method": frame["method"], "params": safe_params})
            return out
        if entry.get("dir") != "stdout":
            return None  # Never export stdin prompts, stderr, paths or incidental diagnostics.
        safe = None
        if name == "agy" and frame.get("event") == "step_update":
            step = frame.get("step_update", {})
            if step.get("tool_name") in {"call_mcp_tool", "invoke_subagent", "define_subagent"}:
                safe = {"event": "step_update", "conversation_id": identifier(frame.get("conversation_id") or step.get("conversation_id")),
                        "step_update": {key: step[key] for key in ("step_index", "state", "tool_name") if key in step}}
                safe["step_update"]["tool_info"] = {"parameters": parameters(step.get("tool_info", {}).get("parameters", {}))}
        elif name.startswith("codex"):
            config = frame.get("result", {}).get("config")
            if isinstance(config, dict):
                safe = {"result": {"config": {"features": {key: value for key, value in config.get("features", {}).items()
                         if key in {"multi_agent", "multi_agent_v2"} and isinstance(value, bool)},
                         "mcp_servers": {key: {"enabled": value.get("enabled")} for key, value in config.get("mcp_servers", {}).items()
                         if key in {"ambient", "agent_terminal", "codex_apps"} and isinstance(value, dict)}}}}
            elif "error" in frame and "invalid transport" in str(frame["error"]):
                safe = {"error": {"code": frame["error"].get("code"), "invalid_transport": True}}
            elif isinstance(frame.get("result", {}).get("thread"), dict):
                result = frame["result"]
                sandbox = result.get("sandbox", {})
                safe = {"result": {"thread": {"id": identifier(result["thread"].get("id"))},
                        "approvalPolicy": result.get("approvalPolicy"),
                        "sandbox": {key: sandbox[key] for key in ("type", "networkAccess") if key in sandbox}}}
            elif frame.get("method") == "mcpServer/startupStatus/updated":
                params = frame.get("params", {})
                if params.get("name") in {"agent_terminal", "ambient", "codex_apps"}:
                    safe = {"method": frame["method"], "params": {key: params[key] for key in ("name", "status") if key in params}}
            elif frame.get("method") in {"item/started", "item/completed"}:
                item = frame.get("params", {}).get("item", {})
                if item.get("type") == "subAgentActivity":
                    safe = {"method": frame["method"], "params": {"item": {"type": "subAgentActivity", "kind": item.get("kind"),
                            "agentThreadId": identifier(item.get("agentThreadId"))}}}
            elif frame.get("method") == "turn/completed":
                safe = {"method": "turn/completed", "completed": True}
        elif name == "claude":
            if frame.get("type") == "system" and frame.get("subtype") == "init":
                safe = {"type": "system", "subtype": "init", "tools": [tool for tool in frame.get("tools", [])
                        if tool in {"Read", "Glob", "Grep", "Edit", "Write", "NotebookEdit", "Bash"}]}
            elif frame.get("type") == "control_request" and frame.get("request", {}).get("tool_name") == "Bash":
                safe = {"type": "control_request", "request": {"subtype": "can_use_tool", "tool_name": "Bash",
                        "input": arguments(frame["request"].get("input"))}}
            elif frame.get("type") == "control_response":
                response = frame.get("response", {})
                behaviour = response.get("response", {}).get("behavior")
                if behaviour in {"allow", "deny"}:
                    safe = {"type": "control_response", "response": {"behavior": behaviour}}
            elif frame.get("type") == "result":
                safe = {"type": "result", "is_error": bool(frame.get("is_error"))}
        if safe is None:
            return None
        out.update(dir="stdout", frame=safe)
        return out

    destination.mkdir(parents=True, exist_ok=True)
    for name, entries in recordings.items():
        frames = [value for entry in entries if (value := project(entry, name)) is not None]
        payload = {"fixture_version": 1, "provider": name.split("-", 1)[0],
                   "evidence_kind": name, "frames": frames}
        (destination / f"{name}.proof.json").write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    for path in source.glob("*-results.json"):
        if path.resolve() != (destination / path.name).resolve():
            shutil.copyfile(path, destination / path.name)
    aggregate = source / "results.json"
    if aggregate.exists() and aggregate.resolve() != (destination / aggregate.name).resolve():
        shutil.copyfile(aggregate, destination / aggregate.name)


def mcp_child():
    """A real child transport; its log contains no environment values or call arguments."""
    log = Path(os.environ["PROOF_LOG"])
    with log.open("a") as stream:
        stream.write(json.dumps({"event": "started", "pid": os.getpid(),
                                 "cap_present": bool(os.environ.get("PROOF_CAP"))}) + "\n")
    for line in sys.stdin:
        frame = json.loads(line)
        method, rid = frame.get("method"), frame.get("id")
        if method == "initialize":
            result = {"protocolVersion": frame.get("params", {}).get("protocolVersion", "2025-06-18"),
                      "capabilities": {"tools": {}},
                      "serverInfo": {"name": "delegation-proof", "version": "1"}}
        elif method == "tools/list":
            result = {"tools": [{"name": "echo", "description": "Return the fixed word OK.",
                                  "inputSchema": {"type": "object", "properties": {}}}]}
        elif method == "tools/call":
            with log.open("a") as stream:
                stream.write(json.dumps({"event": "call", "tool": frame["params"].get("name"),
                                         "at": time.time(), "meta": frame["params"].get("_meta", {}),
                                         "arguments":frame["params"].get("arguments",{})}) + "\n")
            result = {"content": [{"type": "text", "text": "OK"}]}
        elif rid is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": rid,
                              "error": {"code": -32601, "message": "not implemented"}}), flush=True)
            continue
        else:
            continue
        print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}), flush=True)
    return 0


def agy_hook():
    raw = sys.stdin.read()
    path = Path(os.environ["PROOF_HOOK_LOG"])
    with path.open("a") as stream:
        stream.write(json.dumps({"at": time.time(), "payload": json.loads(rec.redact(raw))}) + "\n")
    answer = subprocess.run([os.environ["PROOF_APP"], "--approval-hook"], input=raw,
                            text=True, capture_output=True, timeout=10, check=False)
    if answer.returncode == 0 and answer.stdout.strip():
        print(answer.stdout.strip(), flush=True)
    else:
        print(json.dumps({"decision": "deny", "reason": "fixture approval client failed"}), flush=True)
    return 0


class ModelFixture:
    """Captures tool NAMES only; never stores prompts, headers or credentials."""
    def __init__(self):
        self.tools = []
        self.requests = []
        self.shapes = []
        self.native_probe = False
        self.native_sent = False
        self.native_outputs = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"data":[]}')

            def do_POST(self):
                if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
                    chunks = []
                    total = 0
                    while True:
                        size = int(self.rfile.readline().strip().split(b";", 1)[0], 16)
                        if size == 0:
                            self.rfile.readline()
                            break
                        total += size
                        if total > 8 * 1024 * 1024:
                            self.send_error(413)
                            return
                        chunks.append(self.rfile.read(size))
                        self.rfile.read(2)
                    raw = b"".join(chunks)
                else:
                    raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body = json.loads(raw or b"{}")
                owner.native_outputs.extend(str(item.get("output", ""))[:2048] for item in body.get("input", [])
                                            if item.get("type") == "function_call_output")
                owner.shapes.append({"path":self.path,"bytes":len(raw),"keys":sorted(body),"tools_type":type(body.get("tools")).__name__,
                    "metadata_keys":sorted(body.get("client_metadata",{})),"has_spawn_agent":"spawn_agent" in raw.decode(),
                    "input_shapes":[{"keys":sorted(i),"type":i.get("type"),"role":i.get("role")} for i in body.get("input",[])]})
                names = []
                tools = list(body.get("tools", []))
                for item in body.get("input", []):
                    if item.get("type") == "additional_tools":
                        tools.extend(item.get("tools", []))
                for tool in tools:
                    if tool.get("type") == "namespace":
                        names.extend(t.get("name", "") for t in tool.get("tools", []))
                    else:
                        names.append(tool.get("name") or tool.get("function", {}).get("name", ""))
                owner.tools.append(sorted(names))
                owner.requests.append(self.path)
                if "count_tokens" in self.path:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(b'{"input_tokens":1}')
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.end_headers()
                try:
                    if "messages" in self.path:
                        self.anthropic(body)
                    else:
                        self.responses_stream(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def event(self, kind, body):
                self.wfile.write(f"event: {kind}\ndata: {json.dumps(body)}\n\n".encode())
                self.wfile.flush()

            def anthropic(self, body):
                replied = any(part.get("type") == "tool_result"
                              for msg in body.get("messages", [])
                              for part in (msg.get("content", []) if isinstance(msg.get("content"), list) else []))
                tool = not replied
                self.event("message_start", {"type": "message_start", "message": {
                    "id": "msg_proof", "type": "message", "role": "assistant", "model": body.get("model"),
                    "content": [], "stop_reason": None, "stop_sequence": None,
                    "usage": {"input_tokens": 1, "output_tokens": 0}}})
                block = {"type": "tool_use", "id": "tool_proof", "name": "Bash", "input": {}} if tool else {"type": "text", "text": ""}
                self.event("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block})
                delta = {"type": "input_json_delta", "partial_json": json.dumps({"command": "touch proof-denied", "description": "Permission proof"})} if tool else {"type": "text_delta", "text": "OK"}
                self.event("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta})
                self.event("content_block_stop", {"type": "content_block_stop", "index": 0})
                self.event("message_delta", {"type": "message_delta", "delta": {
                    "stop_reason": "tool_use" if tool else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}})
                self.event("message_stop", {"type": "message_stop"})

            def responses_stream(self, body):
                message = {"id": "msg_proof", "type": "message", "role": "assistant", "status": "completed",
                           "content": [{"type": "output_text", "text": "OK", "annotations": []}]}
                native = owner.native_probe and not owner.native_sent
                if native:
                    owner.native_sent = True
                    message = {"id":"fc_proof", "type":"function_call", "call_id":"call_proof", "namespace":"collaboration",
                               "name":"spawn_agent", "arguments":json.dumps({"task_name":"proof_child", "message":"Reply OK."})}
                response = {"id": "resp_proof", "object": "response", "created_at": int(time.time()),
                            "status": "completed", "output": [message],
                            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
                                      "input_tokens_details": {"cached_tokens": 0},
                                      "output_tokens_details": {"reasoning_tokens": 0}}}
                self.event("response.created", {"type": "response.created", "response": {**response, "status": "in_progress", "output": []}})
                self.event("response.output_item.added", {"type": "response.output_item.added", "output_index": 0,
                                                          "item": {**message, "status": "in_progress", "content": []}})
                if not native:
                    self.event("response.output_text.delta", {"type": "response.output_text.delta", "item_id": "msg_proof",
                                                              "output_index": 0, "content_index": 0, "delta": "OK"})
                self.event("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": message})
                self.event("response.completed", {"type": "response.completed", "response": response})

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class Wire:
    def __init__(self, argv, cwd, env, output, canary):
        self.canary = canary
        self.log = output.open("w")
        self.log_lock = threading.Lock()
        self.frames = []
        self.timed_frames = []
        self.queue = queue.Queue()
        self.next_id = 0
        self.process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                        text=True, bufsize=1, start_new_session=True)
        self.threads = []
        for channel in ["stdout", "stderr"]:
            thread = threading.Thread(target=self.pump, args=(channel,), daemon=True)
            thread.start()
            self.threads.append(thread)

    def record(self, direction, frame):
        safe = rec.redact(json.dumps(frame).replace(self.canary, "<CAPABILITY_CANARY>"))
        safe = re.sub(r"atdc_[A-Za-z0-9_-]+", "<CAPABILITY_CANARY>", safe)
        safe = re.sub(r"eyJ[A-Za-z0-9_-]{12,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+", "<AUTH_REDACTED>", safe)
        safe = re.sub(r"ya29\.[A-Za-z0-9._-]+", "<AUTH_REDACTED>", safe)
        with self.log_lock:
            self.log.write(json.dumps({"at":time.time(),"dir": direction, "frame": json.loads(safe)}) + "\n")
            self.log.flush()

    def pump(self, channel):
        for line in getattr(self.process, channel):
            try:
                frame = json.loads(line)
            except ValueError:
                frame = {"raw": line.rstrip()[:1024]}
            self.record(channel, frame)
            if channel == "stdout":
                self.frames.append(frame)
                self.timed_frames.append((time.time(),frame))
                self.queue.put(frame)

    def send(self, frame):
        self.record("stdin", frame)
        self.process.stdin.write(json.dumps(frame) + "\n")
        self.process.stdin.flush()

    def wait(self, predicate, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                frame = self.queue.get(timeout=min(0.2, deadline - time.monotonic()))
            except queue.Empty:
                if self.process.poll() is not None:
                    return None
                continue
            if predicate(frame):
                return frame
        return None

    def call(self, method, params):
        self.next_id += 1
        rid = self.next_id
        self.send({"id": rid, "method": method, "params": params})
        return self.wait(lambda frame: frame.get("id") == rid and "method" not in frame)

    def close(self):
        try:
            self.process.stdin.close()
        except (OSError, ValueError):
            pass
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=2)
        # A wrapper can exit while a descendant still holds the stream. The group belongs
        # exclusively to this fixture; never leave its MCP/model children behind.
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        for thread in self.threads:
            thread.join(timeout=2)
        self.process.stdout.close()
        self.process.stderr.close()
        self.log.close()


def environment(home):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
           "LANG": "C.UTF-8", "TERM": "dumb", "NO_COLOR": "1",
           "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
    for key, folder in [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                        ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"),
                        ("XDG_RUNTIME_DIR", "runtime")]:
        path = home / folder
        path.mkdir(mode=0o700)
        env[key] = str(path)
    return env


def scan_canary(root, canary, excluded=()):
    """Inspect only disposable output, never symlinks or credentials; report relative names."""
    found = []
    for path in root.rglob("*"):
        if path.is_symlink() or not path.is_file() or path in excluded:
            continue
        if path.name in {"auth.json", ".credentials.json"}:
            continue
        needle = canary.encode()
        tail = b""
        with path.open("rb") as stream:
            while chunk := stream.read(65536):
                part = tail + chunk
                if needle in part:
                    found.append(str(path.relative_to(root)))
                    break
                tail = part[-max(0,len(needle)-1):] if len(needle)>1 else b""
    return found


def codex_proof(base, output, parent_only=False):
    canary = "atdc_" + secrets.token_hex(32)
    home, work = base / "home", base / "work"
    home.mkdir(mode=0o700)
    work.mkdir(mode=0o700)
    codex_home = home / ".codex"
    codex_home.mkdir(mode=0o700)
    env = environment(home)
    env["CODEX_HOME"] = str(codex_home)
    log = base / "mcp-starts.jsonl"
    ambient_log = base / "ambient-starts.jsonl"
    with ModelFixture() as model:
        config_path = codex_home / "config.toml"
        config_path.write_text(f'''model = "gpt-6.1-sol"
model_provider = "proof"
[model_providers.proof]
name = "Local deterministic proof"
base_url = "{model.url}/v1"
wire_api = "responses"
requires_openai_auth = false
[features]
enable_request_compression = false
multi_agent = true
multi_agent_v2 = false
code_mode_host = false
[mcp_servers.ambient]
command = {json.dumps(sys.executable)}
args = [{json.dumps(str(HERE))}, "--mcp-child"]
env = {{ PROOF_LOG = {json.dumps(str(ambient_log))} }}
''')
        launch = [shutil.which("codex")]
        if not parent_only:
            launch.extend(["--disable", "multi_agent", "--disable", "multi_agent_v2"])
        process = Wire([*launch, "app-server"], work, env, output / ("codex-parent.jsonl" if parent_only else "codex.jsonl"), canary)
        try:
            initialized = process.call("initialize", {"clientInfo": {"name": "delegation-proof", "version": "1"}})
            assert initialized and "result" in initialized, "Codex initialize failed"
            process.send({"method": "initialized"})
            effective = process.call("config/read", {"includeLayers": False, "cwd": str(work)})
            assert effective and "result" in effective, "effective config unavailable"
            names = list(effective["result"]["config"].get("mcp_servers", {}))
            assert "ambient" in names, "ambient canary server missing from effective config"
            baseline = process.call("thread/start", {"cwd":str(work),"config":{"mcp_servers.codex_apps.enabled":False}})
            transport_baseline = bool(baseline and "invalid transport" in baseline.get("error",{}).get("message",""))
            overlay = {"mcp_servers.agent_terminal.command": sys.executable,
                       "mcp_servers.agent_terminal.args": [str(HERE), "--mcp-child"],
                       "mcp_servers.agent_terminal.env": {"PROOF_LOG": str(log), "PROOF_CAP": canary},
                       "mcp_servers.agent_terminal.default_tools_approval_mode": "approve",
                       "features.multi_agent": False, "features.multi_agent_v2": False}
            overlay["agents.max_depth"] = 0
            overlay["agents.max_threads"] = 1
            for name in [*names, "codex_apps"]:
                if name != "agent_terminal":
                    overlay[f"mcp_servers.{name}.enabled"] = False
                    if name not in names:
                        # Codex validates transport even for a disabled synthetic entry.
                        overlay[f"mcp_servers.{name}.command"] = sys.executable
                        overlay[f"mcp_servers.{name}.args"] = [str(HERE), "--mcp-child"]
            start = process.call("thread/start", {"cwd": str(work), "sandbox": "read-only",
                                                  "approvalPolicy": "never", "config": overlay})
            assert start and "result" in start, "Codex thread start failed"
            result = start["result"]
            assert result["sandbox"]["type"] == "readOnly" and result["approvalPolicy"] == "never", "sandbox mismatch"
            tid = result["thread"]["id"]
            model.native_probe = not parent_only
            turn = process.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": "Reply OK."}],
                                               "approvalPolicy": "never", "sandboxPolicy": {"type": "readOnly"}})
            assert turn and "result" in turn, "turn start failed"
            completed = process.wait(lambda frame: frame.get("method") == "turn/completed", timeout=45)
            assert completed and completed["params"]["turn"]["status"] == "completed", "local model turn failed"
            statuses = [frame["params"].get("name") for frame in process.frames
                        if frame.get("method") == "mcpServer/startupStatus/updated"]
            assert statuses and set(statuses) == {"agent_terminal"}, f"unexpected MCP statuses: {statuses}"
            assert log.exists() and not ambient_log.exists(), "ambient server actually started"
            if parent_only:
                process.close()
                process = Wire([*launch, "app-server"], work, env, output / "codex-parent-resume.jsonl", canary)
                assert process.call("initialize", {"clientInfo":{"name":"delegation-proof","version":"1"}})
                process.send({"method":"initialized"})
                resumed = process.call("thread/resume", {"threadId":tid,"cwd":str(work),"excludeTurns":True,"config":overlay})
                assert resumed and "result" in resumed, "fresh-process resume failed"
                assert resumed["result"]["thread"]["id"] == tid, "resume changed native thread"
                process.call("turn/start", {"threadId":tid,"input":[{"type":"text","text":"Reply OK again."}],
                                              "approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"}})
                assert process.wait(lambda frame: frame.get("method") == "turn/completed", timeout=45), "resumed turn did not complete"
                assert len(log.read_text().splitlines()) >= 2, "resume did not launch a fresh bridge"
                process.close()
                leaks = scan_canary(home, canary)
                return {"G-X3":not leaks,"overlay_start_and_fresh_resume":True,"token_leak_paths":leaks,
                        "provider":"codex-parent","model_transport":"local fixture; no cloud inference"}
            assert model.tools, "no model request captured"
            assert model.native_outputs, f"no native call output: {model.shapes}"
            native_started = [frame for frame in process.frames if frame.get("method") == "item/started"
                              and frame.get("params",{}).get("item",{}).get("type") == "subAgentActivity"
                              and frame["params"]["item"].get("kind") == "started"]
            native_disabled = not native_started and any("disabled" in item.lower() or "not available" in item.lower()
                                                         for item in model.native_outputs)
            tools_off = model.tools[-1]
            # A control turn proves that the capture sees native tooling when it is enabled.
            if process.process.poll() is None:
                process.close()
            process = Wire([shutil.which("codex"), "app-server"], work, env, output / "codex-native-control.jsonl", canary)
            model.native_sent = False
            assert process.call("initialize", {"clientInfo":{"name":"delegation-proof","version":"1"}})
            process.send({"method":"initialized"})
            control = process.call("thread/start", {"cwd": str(work), "sandbox": "read-only", "approvalPolicy": "never",
                                                    "config": {**overlay, "features.multi_agent": True}})
            assert control and "result" in control, "control start failed"
            control_id = control["result"]["thread"]["id"]
            process.call("turn/start", {"threadId": control_id, "input": [{"type": "text", "text": "Reply OK."}],
                                         "approvalPolicy": "never", "sandboxPolicy": {"type": "readOnly"}})
            assert process.wait(lambda frame: frame.get("method") == "turn/completed", timeout=45), "control turn missing"
            tools_on = model.tools[-1]
            assert "spawn_agent" in tools_on, "positive control did not expose spawn_agent"
            control_started = [frame for frame in process.frames if frame.get("method") == "item/started"
                               and frame.get("params",{}).get("item",{}).get("type") == "subAgentActivity"]
            assert control_started, "positive control did not execute a valid native spawn"
        finally:
            process.close()
    leaks = scan_canary(home, canary)
    return {"G-X1": True, "G-X2": native_disabled, "G-X3": not leaks, "token_leak_paths": leaks,
            "native_started_while_disabled": len(native_started), "native_positive_control_started": len(control_started),
            "disabled_builtin_without_transport_rejected":transport_baseline,
            "disabled_tools": tools_off, "enabled_native_tools": sorted(set(tools_on) & NATIVE_TOOLS),
            "provider": "codex", "model_transport": "local fixture; no cloud inference"}


def claude_proof(base, output):
    canary = "atdc_" + secrets.token_hex(32)
    home, work, runtime = base / "home", base / "work", base / "runtime"
    for path in [home, work, runtime]:
        path.mkdir(mode=0o700)
    env = environment(home)
    env.update({"CLAUDE_CONFIG_DIR": str(home / ".claude"), "ANTHROPIC_API_KEY": "local-proof-placeholder",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_TELEMETRY": "1",
                "DISABLE_ERROR_REPORTING": "1", "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1"})
    overlay = runtime / "overlay.json"
    log = runtime / "mcp-starts.jsonl"
    overlay.write_text(json.dumps({"mcpServers": {"agent_terminal": {"command": sys.executable,
                       "args": [str(HERE), "--mcp-child"], "env": {"PROOF_LOG": str(log), "PROOF_CAP": canary}}}}))
    overlay.chmod(0o600)
    tools = ["Read", "Glob", "Grep", "Edit", "Write", "NotebookEdit", "Bash"]
    with ModelFixture() as model:
        env["ANTHROPIC_BASE_URL"] = model.url
        argv = [shutil.which("claude"), "--output-format", "stream-json", "--verbose", "--input-format", "stream-json",
                "--permission-prompt-tool", "stdio", "--include-partial-messages", "--restricted", "--strict-mcp-config",
                "--permission-mode", "default", "--mcp-config", str(overlay), "--tools", ",".join(tools), "--model", "haiku",
                "--replay-user-messages"]
        process = Wire(argv, work, env, output / "claude.jsonl", canary)
        approval = False
        try:
            process.send({"type": "control_request", "request_id": "init-proof", "request": {"subtype": "initialize"}})
            process.send({"type": "user", "message": {"role": "user", "content": "Run the permission proof, then finish."}, "parent_tool_use_id": None})
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                frame = process.wait(lambda _frame: True, timeout=max(0.1, deadline - time.monotonic()))
                assert frame is not None, "Claude exited or timed out"
                request = frame.get("request", {})
                if frame.get("type") == "control_request" and request.get("subtype") == "can_use_tool":
                    approval |= request.get("tool_name") == "Bash"
                    process.send({"type": "control_response", "response": {"subtype": "success", "request_id": frame["request_id"],
                                  "response": {"behavior": "deny", "message": "Denied by permission proof"}}})
                if frame.get("type") == "result":
                    assert not frame.get("is_error"), "Claude local-model turn failed"
                    break
            else:
                raise AssertionError("Claude no result before deadline")
            assert approval, "restricted Bash did not request host permission"
            assert not (work / "proof-denied").exists(), "denied Bash executed"
            init = next(frame for frame in process.frames if frame.get("type") == "system" and frame.get("subtype") == "init")
            assert set(init["tools"]) == set(tools) | {"mcp__agent_terminal__echo"}, "restricted tools mismatch"
            assert [server["name"] for server in init["mcp_servers"]] == ["agent_terminal"], "strict MCP mismatch"
        finally:
            process.close()
    leaks = scan_canary(home, canary)
    return {"G-C1": approval, "G-C3": not leaks, "token_leak_paths": leaks,
            "provider": "claude", "model_transport": "local fixture; no cloud inference"}


def agy_attested(calls, timed_frames):
    """Every observed invocation must match exact original arguments within two seconds."""
    def attests(call):
        meta = call.get("meta",{})
        progress = str(meta.get("progressToken",""))
        index = progress.rsplit(":",1)[-1]
        conversation = meta.get("antigravity.google/conversation_id")
        for arrived,frame in timed_frames:
            step = frame.get("step_update",{})
            parameters = step.get("tool_info",{}).get("parameters",{})
            if (frame.get("event")=="step_update" and abs(arrived-call["at"])<=2
                and str(step.get("step_index"))==index
                and conversation and conversation==(frame.get("conversation_id") or step.get("conversation_id"))
                and parameters.get("ServerName")=="agent_terminal" and parameters.get("ToolName")=="echo"
                and parameters.get("Arguments")==call.get("arguments")):
                return True
        return False
    return bool(calls) and all(attests(call) for call in calls)


def agy_proof(base, output, app, auth_file=None):
    """Opt-in CLOUD test. No auth is copied; an unavailable account is a failed proof."""
    assert app.is_file() and os.access(app,os.X_OK), "build agent-terminal before the agy approval-hook proof"
    canary = "atdc_" + secrets.token_hex(32)
    home, work = base / "home", base / "work"
    home.mkdir(mode=0o700)
    work.mkdir(mode=0o700)
    env = environment(home)
    temporary = home/"tmp"
    temporary.mkdir(mode=0o700)
    env["TMPDIR"] = str(temporary)
    config = home / ".gemini" / "config"
    config.mkdir(parents=True, mode=0o700)
    mcp_log, hook_log = base / "mcp.jsonl", base / "hook.jsonl"
    socket_path = base / "approval.sock"
    env.update({"PROOF_APP":str(app),"PROOF_CAP":canary,"PROOF_HOOK_LOG":str(hook_log),
                "AGENT_TERMINAL_APPROVAL_SOCKET":str(socket_path),"AGENT_TERMINAL_DELEGATION_CAP":canary})
    server = {"command":sys.executable,"args":[str(HERE),"--mcp-child"],"env":{"PROOF_LOG":str(mcp_log)}}
    (config / "mcp_config.json").write_text(json.dumps({"mcpServers":{"agent_terminal":server,"other":server}}))
    command = shlex.join([sys.executable,str(HERE),"--agy-hook"])
    (config / "hooks.json").write_text(json.dumps({"delegation-proof":{"PreToolUse":[{
        "matcher":".*","hooks":[{"type":"command","command":command,"timeout":10}]}]}}))
    queries = []

    class Approval(socketserver.StreamRequestHandler):
        def handle(self):
            query = json.loads(self.rfile.readline(65536))
            queries.append(query)
            allowed = query.get("tool") == "call_mcp_tool" and query.get("args",{}).get("ServerName") == "agent_terminal"
            self.wfile.write((json.dumps({"decision":"allow" if allowed else "deny","reason":"delegation proof"})+"\n").encode())

    listener = socketserver.UnixStreamServer(str(socket_path),Approval)
    listener_thread = threading.Thread(target=listener.serve_forever,daemon=True)
    listener_thread.start()
    prompt = ("Perform these permission probes in order; do not use any other tools. "
              "Call agent_terminal's echo MCP tool. Then call other's echo MCP tool. "
              "Then try to invoke a subagent to reply OK. Report each outcome briefly.")
    argv = [shutil.which("agy"),"-p",prompt,"--output-format","stream-json","--mode","plan",
            "--dangerously-skip-permissions"]
    if auth_file:
        assert shutil.which("bwrap"), "bwrap is required for read-only auth binding"
        assert auth_file.is_file() and not auth_file.is_symlink(), "auth source must be one regular file"
        destination = home/".gemini"/"antigravity-cli"/"antigravity-oauth-token"
        destination.parent.mkdir(mode=0o700)
        destination.touch(mode=0o600)
        argv = [shutil.which("bwrap"),"--ro-bind","/","/","--bind",str(base),str(base),
                "--ro-bind",str(auth_file),str(destination),"--proc","/proc","--dev","/dev",
                "--unshare-pid","--die-with-parent","--",*argv]
    process = Wire(argv,work,env,output/"agy.jsonl",canary)
    try:
        result = process.wait(lambda frame: frame.get("type") == "result" or frame.get("event") == "result",timeout=120)
        assert result, "agy timed out or could not authenticate with its isolated home"
        assert result.get("result",{}).get("status") != "ERROR", rec.redact(result.get("result",{}).get("error","agy failed"))
    finally:
        process.close()
        listener.shutdown()
        listener.server_close()
        listener_thread.join(timeout=2)
    hooks = [json.loads(line) for line in hook_log.read_text().splitlines()] if hook_log.exists() else []
    calls = [json.loads(line) for line in mcp_log.read_text().splitlines()] if mcp_log.exists() else []
    own = [query for query in queries if query.get("tool")=="call_mcp_tool" and query.get("args",{}).get("ServerName")=="agent_terminal"]
    foreign = [query for query in queries if query.get("tool")=="call_mcp_tool" and query.get("args",{}).get("ServerName")=="other"]
    native = [query for query in queries if query.get("tool") in {"invoke_subagent","define_subagent","manage_subagents","browser_subagent"}]
    actual = [entry for entry in calls if entry.get("event")=="call"]
    # Exact arrival correlation requires the provider's step arguments within two seconds,
    # not a final summary or a loose name match. Missing fields never count as proof.
    attested = agy_attested(actual,process.timed_frames)
    leaks = scan_canary(home,canary)
    (output/"agy-hook.jsonl").write_text("\n".join(json.dumps(entry) for entry in hooks)+"\n")
    (output/"agy-mcp.jsonl").write_text("\n".join(json.dumps(entry) for entry in calls)+"\n")
    return {"provider":"agy","G-A1":bool(own and foreign and native and len(actual)==1),
            "G-A3":not leaks,"G-A5":attested,"token_leak_paths":leaks,
            "hook_own_queries":len(own),"hook_foreign_queries":len(foreign),"hook_native_queries":len(native),
            "model_transport":"cloud model; isolated home; no credentials copied"}


def main():
    if sys.argv[1:] == ["--mcp-child"]:
        return mcp_child()
    if sys.argv[1:] == ["--agy-hook"]:
        return agy_hook()
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--provider", choices=["codex", "codex-parent", "claude", "agy", "all"], default="all")
    parser.add_argument("--cloud",action="store_true",help="explicitly enable the agy cloud-model probe")
    parser.add_argument("--app",type=Path,default=Path("target/debug/agent-terminal"))
    parser.add_argument("--agy-auth-file",type=Path,help="one existing auth source, bound read-only with bwrap; never read or copied by the harness")
    parser.add_argument("--export-fixtures",type=Path)
    parser.add_argument("--out", type=Path, default=Path("target/qa-artifacts/delegation-proof"))
    args = parser.parse_args()
    if args.provider == "agy" and not args.cloud and not args.dry_run:
        parser.error("agy uses its cloud backend; pass --cloud explicitly")
    providers = ["codex", "codex-parent", "claude"] if args.provider == "all" else [args.provider]
    if args.dry_run:
        cloud = "agy" in providers
        print(json.dumps({"mode": "dry-run", "providers": providers,
                          "cloud_calls": 1 if cloud else 0, "user_config_changes": 0,
                          "plan": ["Create disposable home/workspace/runtime", "Install isolated agy hook and start fixture Unix approval socket" if cloud else "Start loopback model fixture",
                                   "Launch installed CLIs with fixture MCP and fresh canary", "Assert enforcement and scan disposable provider outputs",
                                   "Kill complete process groups; remove disposable files" ]}, indent=2))
        return 0
    args.out.mkdir(parents=True, exist_ok=True)
    results = []
    for provider in providers:
        with tempfile.TemporaryDirectory(prefix="delegation-proof-") as temporary:
            base = Path(temporary)
            try:
                if provider.startswith("codex"):
                    result = codex_proof(base, args.out, parent_only=provider == "codex-parent")
                elif provider == "agy":
                    result = agy_proof(base,args.out,args.app.resolve(),args.agy_auth_file)
                else:
                    result = claude_proof(base, args.out)
                result["version"] = subprocess.check_output([shutil.which(provider.split("-",1)[0]), "--version"], text=True, timeout=10).strip()
                result["passed"] = all(value for key, value in result.items() if key.startswith("G-"))
            except (AssertionError, OSError, KeyError, TypeError, ValueError) as error:
                result = {"provider": provider, "passed": False, "error": str(error)}
            results.append(result)
            (args.out/f"{provider}-results.json").write_text(json.dumps(result,indent=2)+"\n")
            print(json.dumps(result))
    (args.out / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    if args.export_fixtures:
        export_proof_fixtures(args.out, args.export_fixtures)
    return 0 if all(result["passed"] for result in results) else 1


if __name__ == "__main__":
    sys.exit(main())

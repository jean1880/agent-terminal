"""Prompt-less MCP smoke of a built or installed binary, without a display or credentials."""

import argparse
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile

import qa_env


def smoke(binary):
    """Exercise the real entry point and SDK; an unbound bridge must fail closed."""
    with tempfile.TemporaryDirectory(prefix="agent-terminal-mcp-smoke-") as temporary:
        env = qa_env.isolated_env(Path(temporary))
        # No DISPLAY, WAYLAND_DISPLAY, session bus or delegation credential is inherited.
        process = subprocess.Popen(
            [str(Path(binary).resolve()), "--cross-agent-mcp"], env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        pending = bytearray()

        def send(value):
            process.stdin.write(json.dumps(value).encode() + b"\n")
            process.stdin.flush()

        def receive():
            while b"\n" not in pending:
                if not selector.select(timeout=5):
                    raise ValueError("MCP response deadline exceeded")
                chunk = os.read(process.stdout.fileno(), 4096)
                if not chunk:
                    raise ValueError("MCP exited before replying")
                pending.extend(chunk)
                if len(pending) > 1024 * 1024:
                    raise ValueError("MCP response exceeded transport bound")
            line, _, remaining = pending.partition(b"\n")
            pending[:] = remaining
            value = json.loads(line)
            if value.get("jsonrpc") != "2.0":
                raise ValueError("stdout did not contain a JSON-RPC response")
            return value

        try:
            send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "package-smoke", "version": "1"},
            }})
            initialised = receive()
            if initialised.get("result", {}).get("serverInfo", {}).get("name") != "agent-terminal":
                raise ValueError("unexpected MCP server identity")
            send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
            listed = receive()
            names = [tool["name"] for tool in listed.get("result", {}).get("tools", [])]
            if listed.get("id") != 2 or names != ["cross_agent_capabilities"]:
                raise ValueError("unbound bridge advertised privileged tools")
            send({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "cross_agent_capabilities", "arguments": {},
            }})
            reply = receive()
            result = reply.get("result", {})
            if (reply.get("id") != 3 or result.get("isError") is not True
                    or result.get("structuredContent", {}).get("error", {}).get("code") != "runtime_unavailable"):
                raise ValueError("unbound bridge did not refuse access")
            process.stdin.close()
            process.wait(timeout=5)
            if process.returncode:
                raise ValueError("MCP entry point exited unsuccessfully")
        finally:
            selector.close()
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)
            process.stdout.close()
            process.stderr.close()
    return {"passed": True, "display": False, "mode": "unbound/degraded"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    print(json.dumps(smoke(args.binary), sort_keys=True))


if __name__ == "__main__":
    main()

"""Deterministic Claude stream-json process for delegation tests; no tools or network."""
import json
from pathlib import Path
import sys
import time

root = Path(sys.argv[1])
turn = 0
tools = sys.argv[sys.argv.index("--tools") + 1].split(",") if "--tools" in sys.argv else []
model = (root / "reported-model").read_text() if (root / "reported-model").exists() else "synthetic-model"


def record(direction, frame):
    with (root / "frames.ndjson").open("a") as log:
        log.write(json.dumps({"dir": direction, "frame": frame}) + "\n")


def emit(frame):
    record("out", frame)
    print(json.dumps(frame), flush=True)


for line in sys.stdin:
    frame = json.loads(line)
    record("in", frame)
    if frame.get("type") == "control_request":
        request = frame.get("request", {})
        emit({"type": "control_response", "response": {
            "subtype": "success", "request_id": frame["request_id"],
            "response": {"models": [{"value": "synthetic-model",
                                       "displayName": "Synthetic"}]}}})
        if request.get("subtype") == "interrupt" and not (root / "ignore-interrupt").exists():
            emit({"type": "result", "subtype": "error_during_execution",
                  "is_error": True, "result": "interrupted"})
    elif frame.get("type") == "user":
        turn += 1
        text = frame.get("message", {}).get("content", "")
        bootstrap = "startup check" in text
        if not bootstrap and (root / "silent-task").exists():
            # The task was written but its turn never opens: a stop now finds no turn to end.
            continue
        init_tools = tools + (["Agent"] if (root / "bad-init").exists() else [])
        emit({"type": "system", "subtype": "init", "session_id": "synthetic-session",
              "model": model, "cwd": str(root), "tools": init_tools,
              "mcp_servers": [{"name": "agent_terminal", "status": "connected"}]})
        emit({"type": "user", "isReplay": True, "message": {"role": "user", "content": text}})
        if bootstrap and (root / "bootstrap-tool").exists():
            emit({"type": "assistant", "message": {"id": f"a-{turn}", "role": "assistant",
                  "model": model, "content": [{"type": "tool_use", "id": "illegal",
                  "name": "Read", "input": {"file_path": "file"}}]}})
        else:
            emit({"type": "assistant", "message": {"id": f"a-{turn}", "role": "assistant",
                  "model": model, "content": [{"type": "text",
                  "text": "OK" if bootstrap else "Synthetic delegated result."}]}})
        # A test holds the startup check open to change the world before the task is sent.
        while bootstrap and (root / "hold-bootstrap").exists():
            time.sleep(0.01)
        if "hold" not in text or bootstrap:
            emit({"type": "result", "subtype": "success", "is_error": False,
                  "session_id": "synthetic-session", "result": "OK" if bootstrap else "Synthetic delegated result."})

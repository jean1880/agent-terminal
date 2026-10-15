"""Synthetic Codex app-server for session tests; stdlib only, no tools or network.

Each prompt selects a deterministic scenario. Server requests block progress until
the exact typed ID is answered; stdin/stdout are the real application transport.
All frames are saved in the test's private directory for failure diagnostics.

Delegation (P4) controls, as files in the test's directory:
- `refuse-steer`: every `turn/steer` is refused (a strict steer that lost its turn);
- `readback.json`: merged into the `thread/start`/`thread/resume` result, which otherwise
  reads back the `approvalPolicy` and `sandbox` the request asked for;
- `mcp-servers`: one name per line, each reported ready by `mcpServer/startupStatus/updated`
  right after the thread starts.
A delegated turn's last line selects `hold` (the turn stays open until interrupted) or
`mcp:<name>` (that server reports in mid-turn, then the turn stays open).
"""

import json
from pathlib import Path
import sys


root = Path(sys.argv[1])
counter = root / "connections"
connection = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(connection))
thread = "fixture-thread"
turn = None
scenario = None
pending = None
ordinal = 0
turn_number = 0
# The current turn already ended (an interrupt after a Stop turn answer only acknowledges).
ended = False


def record(direction, frame):
    with (root / "frames.ndjson").open("a") as log:
        log.write(json.dumps({"connection": connection, "dir": direction,
                              "frame": frame}) + "\n")


def emit(frame):
    record("out", frame)
    print(json.dumps(frame), flush=True)


def notify(method, **params):
    emit({"method": method, "params": {"threadId": thread, **params}})


SANDBOX_TYPES = {"read-only": "readOnly", "workspace-write": "workspaceWrite",
                 "danger-full-access": "dangerFullAccess"}


def server_status(name):
    emit({"method": "mcpServer/startupStatus/updated",
          "params": {"name": name, "status": "ready"}})


def complete(status="completed", error=None):
    global pending, ended
    pending = None
    ended = True
    notify("turn/completed", turn={"id": turn, "status": status,
                                    "error": {"message": error} if error else None})


def request():
    global pending
    wire_id = str(ordinal) if scenario == "strings" else ordinal
    params = {"threadId": thread, "turnId": turn,
              "itemId": f"{turn}-item-{ordinal}"}
    if scenario == "question":
        method = "item/tool/requestUserInput"
        params["questions"] = [{"id": "choice", "header": "Choice",
                                "question": "Continue?",
                                "options": [{"label": "Yes"}, {"label": "No"}]}]
    elif scenario.startswith("permissions:"):
        # "permissions:<RequestPermissionProfile JSON>", asked from the test's directory.
        method = "item/permissions/requestApproval"
        params.update(cwd=str(root), startedAtMs=1, reason="Synthetic permissions",
                      permissions=json.loads(scenario.split(":", 1)[1]))
    elif scenario.startswith("form:"):
        # "form:<mode fields JSON>" from a synthetic tool server.
        method = "mcpServer/elicitation/request"
        params = {"threadId": thread, "turnId": turn, "serverName": "synthetic",
                  "message": "Synthetic form", **json.loads(scenario.split(":", 1)[1])}
    elif scenario == "file-change":
        # A patch inside the test's directory: the item names its changes, the request none.
        notify("item/started", turnId=turn, item={
            "id": params["itemId"], "type": "fileChange", "status": "inProgress",
            "changes": [{"path": str(root / "synthetic.txt"), "kind": {"type": "add"},
                         "diff": "synthetic\n"}]})
        method = "item/fileChange/requestApproval"
        params.update(reason="Synthetic patch")
    else:
        method = "item/commandExecution/requestApproval"
        params.update(command="printf synthetic", reason="Synthetic approval")
    pending = wire_id
    frame = {"id": wire_id, "method": method, "params": params}
    emit(frame)
    # The same pending frame must not materialise a second card.
    emit(frame)
    if scenario == "disconnect-approval":
        sys.exit(23)


for line in sys.stdin:
    frame = json.loads(line)
    record("in", frame)
    # The switch regression uses a minimal alternate stream-json provider too.
    if "--claude" in sys.argv:
        if frame.get("type") == "control_request":
            emit({"type": "control_response", "response": {
                "subtype": "success", "request_id": frame["request_id"], "response": {}}})
            if frame["request"]["subtype"] == "initialize":
                emit({"type": "system", "subtype": "init", "session_id": "fixture-claude",
                      "model": "synthetic-claude"})
        elif frame.get("type") == "user":
            emit({"type": "result", "subtype": "success", "is_error": False})
        else:
            raise AssertionError(f"unexpected alternate-provider frame: {frame}")
        continue
    method = frame.get("method")
    if method == "initialize":
        emit({"id": frame["id"], "result": {"userAgent": "synthetic-fixture"}})
    elif method == "initialized":
        pass
    elif method == "config/read":
        config_file = root / "effective-config.json"
        effective = json.loads(config_file.read_text()) if config_file.exists() else {"mcp_servers": {}}
        emit({"id": frame["id"], "result": {"config": effective}})
    elif method == "thread/resume" and frame["params"]["threadId"] == "stale-thread":
        # A thread that never completed a turn: Codex has no rollout to resume.
        emit({"id": frame["id"], "error": {
            "code": -32600, "message": "no rollout found for thread id stale-thread"}})
    elif method in ("thread/start", "thread/resume"):
        if method == "thread/resume":
            assert frame["params"]["threadId"] == thread
        result = {"thread": {"id": thread}}
        startup_model = root / "startup-model"
        if startup_model.exists():
            result["model"] = startup_model.read_text()
        # Read back the policy asked for, as codex-cli 0.161.0 does (d6-sandbox-policy).
        params = frame["params"]
        if "approvalPolicy" in params:
            result["approvalPolicy"] = params["approvalPolicy"]
        if "sandbox" in params:
            result["sandbox"] = {"type": SANDBOX_TYPES.get(params["sandbox"], params["sandbox"])}
        readback = root / "readback.json"
        if readback.exists():
            result.update(json.loads(readback.read_text()))
        emit({"id": frame["id"], "result": result})
        servers = root / "mcp-servers"
        if servers.exists():
            for name in servers.read_text().split():
                server_status(name)
    elif method == "turn/start":
        turn_number += 1
        turn = f"connection-{connection}-turn-{turn_number}"
        scenario = frame["params"]["input"][0]["text"]
        ordinal = 0
        ended = False
        emit({"id": frame["id"], "result": {"turn": {"id": turn}}})
        notify("turn/started", turn={"id": turn, "status": "inProgress"})
        command = scenario.rsplit("\n", 1)[-1]
        if command == "hold":
            pass  # open until interrupted
        elif command.startswith("mcp:"):
            server_status(command.split(":", 1)[1])
        elif scenario in ("denied", "ssh-error"):
            error = ("Permission denied: synthetic command" if scenario == "denied"
                     else "Bad owner or permissions on synthetic SSH configuration")
            complete("failed", error)
        elif scenario == "disconnect-command":
            notify("item/started", item={"id": f"{turn}-command", "type": "commandExecution",
                                         "command": "printf synthetic", "status": "inProgress"})
            sys.exit(24)
        elif (scenario == "success" or scenario.endswith("\nsuccess")  # after a handoff
              or "Continue the unfinished task after the model switch." in scenario):
            complete()
        else:
            request()
    elif method == "turn/steer":
        # Accepted only into the active turn it names (codex-cli 0.161.0's precondition).
        params = frame["params"]
        refused = (root / "refuse-steer").exists()
        if not refused and turn is not None and not ended and params["expectedTurnId"] == turn:
            with (root / "steered").open("a") as log:
                log.write(params["input"][0]["text"] + "\n")
            emit({"id": frame["id"], "result": {"turnId": turn}})
        else:
            emit({"id": frame["id"], "error": {
                "code": -32600, "message": "expected turn mismatch"}})
    elif method == "turn/interrupt":
        emit({"id": frame["id"], "result": {}})
        if not ended:
            complete("interrupted")
    elif method is None and "result" in frame:
        assert pending is not None, "unsolicited response"
        assert type(frame["id"]) is type(pending) and frame["id"] == pending, "wrong request ID"
        if scenario == "question":
            assert frame["result"] == {"answers": {"choice": {"answers": ["Yes"]}}}
        elif scenario.startswith("permissions:"):
            assert set(frame["result"]) <= {"permissions", "scope"}
        elif scenario.startswith("form:"):
            assert frame["result"]["action"] in ("accept", "decline", "cancel")
        else:
            assert frame["result"]["decision"] in ("accept", "decline", "cancel")
        pending = None
        ordinal += 1
        if scenario in ("approvals", "strings") and ordinal < 4:
            request()
        else:
            complete()
    else:
        raise AssertionError(f"unexpected fixture frame: {frame}")

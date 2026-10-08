"""Synthetic Codex app-server for session tests; stdlib only, no tools or network.

Each prompt selects a deterministic scenario. Server requests block progress until
the exact typed ID is answered; stdin/stdout are the real application transport.
All frames are saved in the test's private directory for failure diagnostics.
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


def record(direction, frame):
    with (root / "frames.ndjson").open("a") as log:
        log.write(json.dumps({"connection": connection, "dir": direction,
                              "frame": frame}) + "\n")


def emit(frame):
    record("out", frame)
    print(json.dumps(frame), flush=True)


def notify(method, **params):
    emit({"method": method, "params": {"threadId": thread, **params}})


def complete(status="completed", error=None):
    global pending
    pending = None
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
    elif method in ("thread/start", "thread/resume"):
        if method == "thread/resume":
            assert frame["params"]["threadId"] == thread
        emit({"id": frame["id"], "result": {"thread": {"id": thread}}})
    elif method == "turn/start":
        turn_number += 1
        turn = f"connection-{connection}-turn-{turn_number}"
        scenario = frame["params"]["input"][0]["text"]
        ordinal = 0
        emit({"id": frame["id"], "result": {"turn": {"id": turn}}})
        notify("turn/started", turn={"id": turn, "status": "inProgress"})
        if scenario in ("denied", "ssh-error"):
            error = ("Permission denied: synthetic command" if scenario == "denied"
                     else "Bad owner or permissions on synthetic SSH configuration")
            complete("failed", error)
        elif scenario == "disconnect-command":
            notify("item/started", item={"id": f"{turn}-command", "type": "commandExecution",
                                         "command": "printf synthetic", "status": "inProgress"})
            sys.exit(24)
        elif scenario == "success":
            complete()
        else:
            request()
    elif method == "turn/interrupt":
        emit({"id": frame["id"], "result": {}})
        complete("interrupted")
    elif method is None and "result" in frame:
        assert pending is not None, "unsolicited response"
        assert type(frame["id"]) is type(pending) and frame["id"] == pending, "wrong request ID"
        if scenario == "question":
            assert frame["result"] == {"answers": {"choice": {"answers": ["Yes"]}}}
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

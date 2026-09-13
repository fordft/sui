#!/usr/bin/env python3
"""Scenario-driven mock ACP agent for Sui's backend tests.

Speaks ACP/JSON-RPC over newline-delimited JSON on stdio — the same
framing the Rust SDK uses. Behavior is selected by MOCK_ACP_SCENARIO:

  happy          stream chunks + tool_call + plan + usage, then end_turn
  worker         run MOCK_ACP_CMD in cwd, report it as a tool_call, end_turn
  permission     send session/request_permission, obey the decision
  deny           same, but asserts the client denies/cancels it
  hang           hold the prompt until session/cancel, then stop cancelled
  crash          die mid-prompt with a stderr tail
  artifact       submit MOCK_ACP_ARTIFACT (JSON) via the MCP bridge
  bad_artifact   submit an invalid payload via the MCP bridge
  quota          fail session/prompt with a quota error
  no_model_opts  like happy but advertises no configOptions

Optional env:
  MOCK_ACP_CMD        shell command run in the session cwd (worker)
  MOCK_ACP_ARTIFACT   JSON payload for submit_result (artifact)
  MOCK_ACP_NOOPTS     if set, session/new returns no configOptions
"""

import json
import os
import subprocess
import sys

SCEN = os.environ.get("MOCK_ACP_SCENARIO", "happy")
SESSION = "mock-sess-1"
_pending_prompt = None  # id of a prompt held open until cancel


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def respond(i, result):
    send({"jsonrpc": "2.0", "id": i, "result": result})


def respond_err(i, code, msg):
    send({"jsonrpc": "2.0", "id": i, "error": {"code": code, "message": msg}})


def notify_update(update):
    send({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": SESSION, "update": update},
    })


def text_chunk(kind, text):
    notify_update({
        "sessionUpdate": kind,
        "content": {"type": "text", "text": text},
    })


def run_bridge(artifact):
    """Spawn the sui-artifacts MCP server the client advertised, submit
    `artifact`, return (ok, message)."""
    global BRIDGE_SERVERS
    srv = next((s for s in BRIDGE_SERVERS if s.get("name") == "sui-artifacts"), None)
    if srv is None:
        return False, "no sui-artifacts mcp server advertised"
    env = {e["name"]: e["value"] for e in srv.get("env", [])}
    env["PATH"] = os.environ.get("PATH", "/usr/bin:/bin")
    env["HOME"] = os.environ.get("HOME", "/tmp")
    p = subprocess.Popen(
        [srv["command"]] + srv.get("args", []),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        text=True, env=env,
    )
    def rpc(method, params, i):
        p.stdin.write(json.dumps(
            {"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n")
        p.stdin.flush()
        return json.loads(p.stdout.readline())
    rpc("initialize", {"protocolVersion": "2024-11-05",
                       "capabilities": {}, "clientInfo": {"name": "mock"}}, 1)
    p.stdin.write(json.dumps({"jsonrpc": "2.0",
                              "method": "notifications/initialized"}) + "\n")
    p.stdin.flush()
    r = rpc("tools/call", {"name": "submit_result",
                         "arguments": {"payload": artifact}}, 2)
    p.kill()
    res = r.get("result", {})
    txt = "".join(c.get("text", "") for c in res.get("content", []))
    return not res.get("isError", False), txt


BRIDGE_SERVERS = []


def do_prompt(i, params):
    global _pending_prompt
    text = "".join(
        c.get("text", "") for c in params.get("prompt", []) if c.get("type") == "text"
    )
    if SCEN == "crash":
        sys.stderr.write("mock-acp: fatal internal error\n" + "x" * 100 + "\n")
        sys.stderr.flush()
        os._exit(3)
    if SCEN == "hang":
        _pending_prompt = i
        return  # held until session/cancel
    if SCEN == "quota":
        respond_err(i, -32000, "quota exceeded: model rate-limited")
        return
    if SCEN == "permission" or SCEN == "deny":
        # agent → client request; our loop reads the response below
        send({
            "jsonrpc": "2.0", "id": 9001,
            "method": "session/request_permission",
            "params": {
                "sessionId": SESSION,
                "toolCall": {
                    "toolCallId": "tc-perm", "title": "rm -rf build/",
                    "kind": "execute", "status": "pending",
                },
                "options": [
                    {"optionId": "ao", "name": "allow once", "kind": "allow_once"},
                    {"optionId": "aa", "name": "always", "kind": "allow_always"},
                    {"optionId": "ro", "name": "reject", "kind": "reject_once"},
                ],
            },
        })
        # the response arrives as a normal message; handled in main loop,
        # which then finishes the prompt
        _pending_prompt = i
        return
    if SCEN == "artifact" or SCEN == "bad_artifact":
        payload = os.environ.get("MOCK_ACP_ARTIFACT")
        if SCEN == "bad_artifact":
            artifact = {"definitely": "not valid"}
        else:
            artifact = json.loads(payload) if payload else {"verdict": "PASS",
                                                            "findings": [],
                                                            "required_fixes": []}
        ok, msg = run_bridge(artifact)
        text_chunk("agent_message_chunk", f"submit_result → {msg}")
        respond(i, {"stopReason": "end_turn"})
        return
    if SCEN == "worker":
        cmd = os.environ.get("MOCK_ACP_CMD", "true")
        text_chunk("agent_thought_chunk", "planning the edit")
        notify_update({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-1", "title": cmd, "kind": "execute",
            "status": "in_progress",
        })
        out = subprocess.run(cmd, shell=True, capture_output=True, text=True)
        notify_update({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-1",
            "status": "completed" if out.returncode == 0 else "failed",
            "content": [{"type": "content",
                         "content": {"type": "text",
                                     "text": out.stdout + out.stderr}}],
        })
        text_chunk("agent_message_chunk", "done.")
        notify_update({"sessionUpdate": "plan", "entries": [
            {"content": "implement", "status": "completed", "priority": "high"}]})
        notify_update({"sessionUpdate": "usage_update", "used": 12000,
                       "size": 200000})
        respond(i, {"stopReason": "end_turn"})
        return
    # happy / no_model_opts default
    text_chunk("agent_thought_chunk", "thinking aloud")
    text_chunk("agent_message_chunk", "hello from mock-acp")
    respond(i, {"stopReason": "end_turn"})


def main():
    global _pending_prompt, BRIDGE_SERVERS
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        # response to our own outgoing request (permission)
        if "id" in msg and "method" not in msg:
            if msg.get("id") == 9001 and _pending_prompt is not None:
                outc = (msg.get("result") or {}).get("outcome", {})
                oid = str(outc.get("optionId", ""))
                denied = outc.get("outcome") == "cancelled" or \
                    oid.startswith("ro") or oid.startswith("ra") or "reject" in oid
                if SCEN == "deny" and not denied:
                    text_chunk("agent_message_chunk",
                               "ERROR: permission was not denied")
                    respond(_pending_prompt, {"stopReason": "refusal"})
                else:
                    text_chunk("agent_message_chunk",
                               "perm: " + ("denied" if denied else "granted"))
                    respond(_pending_prompt, {"stopReason": "end_turn"})
                _pending_prompt = None
            continue
        mid, method = msg.get("id"), msg.get("method", "")
        params = msg.get("params") or {}
        if method == "initialize":
            respond(mid, {
                "protocolVersion": params.get("protocolVersion", 1),
                "agentCapabilities": {
                    "loadSession": False,
                    "promptCapabilities": {"image": False, "audio": False,
                                           "embeddedContext": False},
                    "mcpCapabilities": {"http": False, "sse": False},
                },
                "agentInfo": {"name": "mock-acp", "version": "0.0.1"},
                "authMethods": [],
            })
        elif method == "session/new":
            BRIDGE_SERVERS = params.get("mcpServers", [])
            resp = {"sessionId": SESSION}
            if SCEN != "no_model_opts" and not os.environ.get("MOCK_ACP_NOOPTS"):
                resp["configOptions"] = [{
                    "id": "model", "name": "Model", "category": "model",
                    "type": "select", "currentValue": "mock-1",
                    "options": [
                        {"value": "mock-1", "name": "mock-1"},
                        {"value": "mock-2", "name": "mock-2"}],
                }]
            respond(mid, resp)
        elif method == "session/load":
            respond_err(mid, -32601, "load_session unsupported")
        elif method == "session/prompt":
            do_prompt(mid, params)
        elif method == "session/set_config_option":
            respond(mid, {"configOptions": []})
        elif method == "session/cancel":
            if _pending_prompt is not None:
                respond(_pending_prompt, {"stopReason": "cancelled"})
                _pending_prompt = None
        elif method == "authenticate" or method == "session/set_mode":
            respond(mid, {})


if __name__ == "__main__":
    main()

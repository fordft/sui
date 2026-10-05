"""Deterministic stdio LSP peer for production code_intel integration tests.

This is a protocol fixture, not a Rust analyzer. It provides independently
chosen semantic locations for a tiny ambiguous Rust document and records the
client's framing, document versions, Unicode coordinates and lifecycle.
"""
import argparse
import json
import os
from pathlib import Path
import re
import sys
import time


parser = argparse.ArgumentParser()
parser.add_argument("--trace", required=True)
parser.add_argument("--mode", default="normal")
parser.add_argument("--outside-file")
options = parser.parse_args()
documents = {}
query_count = 0
large_response_pending = False


def trace(event, **fields):
    with open(options.trace, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"event": event, **fields}, ensure_ascii=False) + "\n")


def send(message):
    payload = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(payload)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()


def response(request, result):
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})


def error(request, message):
    send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": message}})


def query_error(request, code, retrigger=None):
    payload = {"code": code, "message": "dummy fixture analysis transition"}
    if retrigger is not None:
        payload["data"] = {"retriggerRequest": retrigger}
    send({"jsonrpc": "2.0", "id": request["id"], "error": payload})


def read_message():
    size = None
    while True:
        line = sys.stdin.buffer.readline(8193)
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if len(line) > 8192:
            raise ValueError("oversize request header")
        key, value = line.decode("ascii").split(":", 1)
        if key.lower() == "content-length":
            size = int(value)
    if size is None or not 0 <= size <= 2 * 1024 * 1024:
        raise ValueError("invalid request length")
    payload = sys.stdin.buffer.read(size)
    if len(payload) != size:
        raise ValueError("incomplete request")
    return json.loads(payload)


def utf16_prefix(line, index):
    return len(line[:index].encode("utf-16-le")) // 2


def range_for(line, number, start, length=4):
    return {
        "start": {"line": number, "character": utf16_prefix(line, start)},
        "end": {"line": number, "character": utf16_prefix(line, start + length)},
    }


def definition_location(uri, source):
    for number, line in enumerate(source.splitlines()):
        if "mod second {" in line and "fn same" in line:
            return {"uri": uri, "range": range_for(line, number, line.index("same"))}
    raise ValueError("fixture has no second::same definition")


def reference_locations(uri, source, include_declaration):
    result = [definition_location(uri, source)] if include_declaration else []
    for number, line in enumerate(source.splitlines()):
        if line.lstrip().startswith("//"):
            continue
        for found in re.finditer(r"\bsecond::same\b", line):
            result.append({"uri": uri, "range": range_for(line, number, found.start() + len("second::"))})
    return result


def position_targets_same(source, position):
    lines = source.splitlines()
    number = position["line"]
    if not 0 <= number < len(lines):
        return False
    line = lines[number]
    character = position["character"]
    for index in range(len(line) + 1):
        if utf16_prefix(line, index) == character:
            return line[index:index + 4] == "same"
    return False


def diagnostics(source):
    items = []
    for number, line in enumerate(source.splitlines()):
        if line.lstrip().startswith("//"):
            continue
        if "missing" in line:
            items.append({
                "range": range_for(line, number, line.index("missing"), len("missing")),
                "severity": 1,
                "code": "E0425",
                "source": "fixture-analyzer",
                "message": "cannot find value `missing` in this scope",
            })
    return items


trace("started", pid=os.getpid())
while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    params = message.get("params", {})
    trace("received", method=method, params=params, id=message.get("id"))
    if method == "initialize":
        response(message, {
            "capabilities": {
                "positionEncoding": "utf-16",
                "textDocumentSync": {"openClose": True, "change": 1},
                "definitionProvider": True,
                "referencesProvider": True,
                "diagnosticProvider": {"interFileDependencies": False, "workspaceDiagnostics": False},
            },
            "serverInfo": {"name": "code-intel-fixture", "version": "1"},
        })
    elif method == "initialized":
        if options.mode == "outside_symlink_after_scan":
            alias = Path(options.trace).parent / "alias.rs"
            alias.symlink_to(Path(options.outside_file).absolute())
            trace("alias_created_after_launch", path=str(alias))
        send({
            "jsonrpc": "2.0",
            "method": "experimental/serverStatus",
            "params": {"health": "ok", "quiescent": True},
        })
    elif method == "textDocument/didOpen":
        document = params["textDocument"]
        documents[document["uri"]] = {"text": document["text"], "version": document["version"]}
    elif method == "textDocument/didChange":
        document = params["textDocument"]
        documents[document["uri"]] = {
            "text": params["contentChanges"][-1]["text"],
            "version": document["version"],
        }
    elif method == "experimental/openCargoToml":
        if options.mode == "loading_then_ready":
            send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                  "params": {"health": "ok", "quiescent": True}})
        cargo = Path(options.trace).parent / "Cargo.toml"
        if options.mode == "membership_unsupported":
            send({"jsonrpc": "2.0", "id": message["id"], "error": {
                "code": -32601, "message": "fixture has no membership extension",
            }})
        elif options.mode == "orphan" or not cargo.is_file():
            response(message, None)
        else:
            response(message, {
                "uri": cargo.absolute().as_uri(),
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            })
        if large_response_pending:
            # Mark the semantic reply only after the subsequent membership
            # reply is flushed, so cancellation can reach wrapper rendering.
            trace("response_sent", pid=os.getpid(), count=5000)
            large_response_pending = False
    elif method in ("textDocument/definition", "textDocument/references", "textDocument/diagnostic"):
        query_count += 1
        uri = params["textDocument"]["uri"]
        document = documents.get(uri)
        if document is None:
            error(message, "document was not opened")
            continue
        if options.mode == "loading_then_ready":
            send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                  "params": {"health": "ok", "quiescent": False}})
        if options.mode == "transient" and query_count <= 2:
            if query_count == 1:
                query_error(message, -32801)
            else:
                query_error(message, -32802, True)
            continue
        if options.mode == "permanent_transient":
            query_error(message, -32801)
            continue
        if options.mode == "cancel_no_retrigger":
            query_error(message, -32802, False)
            continue
        if options.mode == "invalid_query":
            query_error(message, -32602)
            continue
        if options.mode == "hang_once" and not Path(options.trace + ".hung").exists():
            Path(options.trace + ".hung").write_text("dummy fixture marker", encoding="utf-8")
            trace("query_hanging", pid=os.getpid())
            while True:
                time.sleep(0.1)
        if options.mode == "malformed":
            sys.stdout.buffer.write(b"Content-Length: nope\r\n\r\n")
            sys.stdout.buffer.flush()
            while True:
                time.sleep(0.1)
        if options.mode == "oversize_frame":
            sys.stdout.buffer.write(b"Content-Length: 4194305\r\n\r\n")
            sys.stdout.buffer.flush()
            while True:
                time.sleep(0.1)
        if options.mode.startswith("semantic_bad_") and not Path(options.trace + ".bad").exists():
            Path(options.trace + ".bad").write_text("one malformed semantic reply", encoding="utf-8")
            location = definition_location(uri, document["text"])
            selection = location["range"]
            link = {"targetUri": uri, "targetRange": selection, "targetSelectionRange": selection}
            if options.mode == "semantic_bad_unchanged":
                result = {"kind": "unchanged", "resultId": "never supplied", "items": []}
            elif options.mode == "semantic_bad_message":
                result = {"kind": "full", "items": [{"range": selection, "severity": 1}]}
            elif options.mode == "semantic_bad_scalar":
                result = 42
            elif options.mode == "semantic_bad_references_object":
                result = location
            elif options.mode == "semantic_bad_link_range":
                del link["targetRange"]
                result = [link]
            elif options.mode == "semantic_bad_link_selection":
                del link["targetSelectionRange"]
                result = [link]
            elif options.mode == "semantic_bad_link_outside":
                link["targetRange"] = {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
                result = [link]
            elif options.mode == "semantic_bad_after_limit":
                del link["targetRange"]
                result = [location, link]
            else:
                raise ValueError("unknown malformed semantic fixture")
            response(message, result)
            continue
        if method == "textDocument/diagnostic":
            response(message, {"kind": "full", "resultId": str(document["version"]), "items": diagnostics(document["text"])})
            continue
        if not position_targets_same(document["text"], params["position"]):
            error(message, "query was not the UTF-16 position of same")
            continue
        if options.mode in ("outside", "outside_symlink_after_scan"):
            location = definition_location(uri, document["text"])
            # Keep symlinks in the reported URI so the wrapper must guard them.
            target = (Path(options.trace).parent / "alias.rs"
                      if options.mode == "outside_symlink_after_scan"
                      else Path(options.outside_file))
            location["uri"] = target.absolute().as_uri()
            response(message, location if method.endswith("definition") else [location])
        elif options.mode == "location_link":
            location = definition_location(uri, document["text"])
            number = location["range"]["start"]["line"]
            line = document["text"].splitlines()[number]
            response(message, [{
                "targetUri": uri,
                "targetRange": range_for(line, number, 0, len(line)),
                "targetSelectionRange": location["range"],
            }])
        elif options.mode == "large_refs":
            line = document["text"].splitlines()[0]
            selected = range_for(line, 0, line.rindex("same"))
            response(message, [{"uri": uri, "range": selected}] * 5000)
            large_response_pending = True
        elif method == "textDocument/definition":
            response(message, definition_location(uri, document["text"]))
        else:
            response(message, reference_locations(uri, document["text"], params.get("context", {}).get("includeDeclaration", False)))
    elif method == "shutdown":
        response(message, None)
    elif method == "exit":
        break
    elif "id" in message:
        error(message, "unsupported fixture request")
trace("finished", pid=os.getpid())

#!/usr/bin/env python3
"""Mock LLM server for local Patchbay testing.

Speaks OpenAI Chat Completions, OpenAI Responses, Anthropic Messages and Gemini
(streaming) at http://127.0.0.1:PORT/v1 (Gemini: /v1/models/<id>:streamGenerateContent). Replies echo what the model "saw" (text, image count,
attached file names). When the request carries tools, a user message containing
`!run` triggers a shell tool call, `!edit` a file-creating edit call and `!tools` lists
the offered tool names, `!ask` asks a multiple-choice question and `!task` starts a subagent, so the Code tab's permission prompts, diff view and MCP wiring can
be exercised.

Usage: scripts/mock-llm.py [--port 8765]
"""

import argparse
import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODELS = ["mock-echo", "mock-agent"]


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def openai_user_text(content):
    """Return (text, image_count) for an OpenAI message content."""
    if isinstance(content, str):
        return content, 0
    text, images = [], 0
    for part in content or []:
        if part.get("type") == "text":
            text.append(part.get("text", ""))
        elif part.get("type") in ("image_url", "input_image"):
            images += 1
    return "\n".join(text), images


def anthropic_user_text(content):
    if isinstance(content, str):
        return content, 0, False
    text, images, tool_result = [], 0, False
    for part in content or []:
        t = part.get("type")
        if t == "text":
            text.append(part.get("text", ""))
        elif t == "image":
            images += 1
        elif t == "document":
            src = part.get("source") or {}
            text.append(f"[document {src.get('media_type', '?')}, {len(src.get('data', ''))} b64 chars]")
        elif t == "tool_result":
            tool_result = True
            c = part.get("content")
            text.append(c if isinstance(c, str) else json.dumps(c)[:400])
    return "\n".join(text), images, tool_result


def find_tool(tools, want):
    """Pick a tool by its parameter names (works for OpenAI and Anthropic tool shapes)."""
    for t in tools or []:
        fn = t.get("function", t)
        props = (fn.get("parameters") or fn.get("input_schema") or fn.get("parametersJsonSchema") or {}).get("properties", {})
        if want <= set(props):
            return fn.get("name")
    return None


def fill_required(tools, name, args):
    """Add placeholder values for any required parameters the scripted args don't cover."""
    for t in tools or []:
        fn = t.get("function", t)
        if fn.get("name") != name:
            continue
        schema = fn.get("parameters") or fn.get("input_schema") or fn.get("parametersJsonSchema") or {}
        props = schema.get("properties", {})
        for key in schema.get("required", []):
            if key in args:
                continue
            typ = (props.get(key) or {}).get("type")
            args[key] = {"boolean": False, "integer": 0, "number": 0, "array": [], "object": {}}.get(
                typ if isinstance(typ, str) else "", f"mock {key}"
            )
    return args


RULE_MARKER = "PB-RULE-MARKER"
LAST_BODY = {}


def decide(user_text, images, after_tool, tools):
    plan = _decide(user_text, images, after_tool, tools)
    if plan[0] == "text" and RULE_MARKER in json.dumps(LAST_BODY.get("body", {})):
        plan = ("text", plan[1] + "\n\n- custom instructions: seen")
    if plan[0] == "tool":
        return ("tool", plan[1], fill_required(tools, plan[1], plan[2]))
    if plan[0] == "tools":
        return ("tools", [(n, fill_required(tools, n, a)) for n, a in plan[1]])
    return plan


def calls_of(plan):
    """The tool calls of a plan, as [(name, args)]."""
    return [(plan[1], plan[2])] if plan[0] == "tool" else list(plan[1])


def _decide(user_text, images, after_tool, tools):
    """Return ("text", str) or ("tool", name, args_dict)."""
    if after_tool:
        snippet = user_text.strip().replace("\n", " ")[:300]
        return ("text", f"Tool finished. I saw this output: `{snippet}`")
    if "!tools" in user_text:
        names = sorted((t.get("function", t) or {}).get("name", "?") for t in tools or [])
        return ("text", "tool names: " + ", ".join(names))
    if "!run" in user_text:
        name = find_tool(tools, {"command"})
        if name:
            return ("tool", name, {"command": "echo patchbay-ok && ls"})
    if "!size" in user_text:
        return ("text", f"request bytes: {len(json.dumps(LAST_BODY.get('body', {})))}")
    if "!search" in user_text:
        name = find_tool(tools, {"query"})
        if name:
            return ("tool", name, {"query": "patchbay search test"})
    task = find_tool(tools, {"subagent_type", "prompt"})
    if task and "!par" in user_text:
        job = lambda tag: {"description": f"job {tag}", "prompt": f"Report PAR-{tag}", "subagent_type": "explore"}
        return ("tools", [(task, job("A")), (task, job("B"))])
    if task and "!bg" in user_text:
        return ("tool", task, {"description": "slow job", "prompt": "Report BG-DONE", "subagent_type": "explore", "run_in_background": True, "name": "scout"})
    if task and "!fork" in user_text:
        return ("tool", task, {"description": "branch off", "prompt": "Report FORK-OK", "subagent_type": "fork"})
    if task and "!named" in user_text:
        return ("tool", task, {"description": "named job", "prompt": "Report NAMED-OK", "subagent_type": "general-purpose", "name": "worker"})
    if task and "!wt" in user_text:
        return ("tool", task, {"description": "isolated edit", "prompt": "!edit please", "subagent_type": "general-purpose", "isolation": "worktree"})
    if "!msg" in user_text:
        name = find_tool(tools, {"to", "message"})
        if name:
            return ("tool", name, {"to": "worker", "message": "Report MSG-OK"})
    if "!ask" in user_text:
        name = find_tool(tools, {"questions"})
        if name:
            return ("tool", name, {"questions": [{"question": "Which one?", "options": [{"label": "Alpha"}, {"label": "Beta"}]}]})
    if "!task" in user_text:
        name = find_tool(tools, {"subagent_type", "prompt"})
        if name:
            return ("tool", name, {"description": "look around", "prompt": "Report the word SUBAGENT-OK", "subagent_type": "explore"})
    if "!edit" in user_text:
        name = find_tool(tools, {"file_path", "old_string", "new_string"})
        if name:
            return (
                "tool",
                name,
                {"file_path": "patchbay-test.txt", "old_string": "", "new_string": "hello from patchbay\n"},
            )
    files = re.findall(r'<file name="([^"]+)"', user_text)
    question = re.sub(r"<file[^>]*>.*?</file>", "", user_text, flags=re.S).strip()
    chars = sum(len(m) for m in re.findall(r"<file[^>]*>(.*?)</file>", user_text, flags=re.S))
    lines = [
        f"**Echo:** {question or '(no text)'}",
        "",
        f"- images seen: {images}",
        f"- attached files: {', '.join(files) if files else 'none'} ({chars} chars)",
        f"- tools offered: {len(tools or [])}",
        "",
        "```python\nprint('hello from the mock model')\n```",
    ]
    return ("text", "\n".join(lines))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse_start(self):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

    def _send(self, data, event=None):
        if event:
            self.wfile.write(f"event: {event}\n".encode())
        self.wfile.write(f"data: {data}\n\n".encode())
        self.wfile.flush()

    def _auth_summary(self):
        auth = self.headers.get("authorization")
        xkey = self.headers.get("x-api-key")
        ver = self.headers.get("anthropic-version")
        return f"authorization={'yes' if auth else 'no'} x-api-key={'yes' if xkey else 'no'} anthropic-version={ver}"

    def do_GET(self):
        if self.path.split("?")[0].endswith("/res/v1/web/search"):
            # Brave Search: needs the subscription token, answers with a tiny result set.
            if self.headers.get("x-subscription-token") != "brave-test-key":
                return self._json(403, {"error": {"detail": "Subscription token invalid"}})
            return self._json(200, {"web": {"results": [
                {"title": "Mock <strong>result</strong>", "url": "https://example.com/mock", "description": "BRAVE-MOCK-HIT &amp; more", "age": "1 day ago"},
            ]}})
        if self.path.rstrip("/").endswith("/models"):
            log("GET", self.path, self._auth_summary())
            return self._json(200, {"object": "list", "data": [{"id": m, "object": "model"} for m in MODELS]})
        self._json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")
        LAST_BODY["body"] = body
        path = self.path.split("?")[0].rstrip("/")
        log("POST", path, self._auth_summary(), f"model={body.get('model')} tools={len(body.get('tools') or [])}",
            "params=" + json.dumps({k: body[k] for k in ("temperature", "top_p", "max_tokens", "max_completion_tokens", "reasoning_effort") if k in body}))
        if path.endswith("/chat/completions"):
            return self.chat(body)
        if path.endswith("/messages"):
            return self.messages(body)
        if path.endswith("/responses"):
            return self.openai_responses(body)
        if ":streamGenerateContent" in path:
            return self.gemini(body)
        self._json(404, {"error": f"unsupported path {path}"})

    # ---- OpenAI Chat Completions ----
    def chat(self, body):
        msgs = body.get("messages") or []
        # Be as strict as e.g. Fireworks: unknown per-message fields are a 400.
        allowed = {"role", "content", "name", "tool_calls", "tool_call_id", "reasoning_content"}
        for i, m in enumerate(msgs):
            extra = sorted(set(m) - allowed)
            if extra:
                log(f"REJECT 400: messages[{i}] has non-standard fields {extra}")
                return self._json(400, {"error": {"type": "invalid_request_error",
                                                  "message": f"Extra inputs are not permitted, field: 'messages[{i}].{extra[0]}'"}})
        last = msgs[-1] if msgs else {}
        after_tool = last.get("role") == "tool"
        # Harnesses may append reminder messages after the prompt; read the whole trailing run.
        texts, images = [], 0
        for m in reversed(msgs):
            if m.get("role") != last.get("role"):
                break
            t, n = openai_user_text(m.get("content"))
            texts.insert(0, t)
            images += n
        text = "\n".join(texts)
        plan = decide(text, images, after_tool, body.get("tools"))
        model = body.get("model", "mock")
        if not body.get("stream"):
            content = plan[1] if plan[0] == "text" else ""
            return self._json(
                200,
                {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion",
                    "model": model,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20},
                },
            )
        self._sse_start()

        def chunk(delta, finish=None):
            return json.dumps(
                {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "created": int(time.time()),
                    "model": model,
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                }
            )

        self._send(chunk({"role": "assistant", "content": ""}))
        if plan[0] == "text":
            for piece in re.findall(r"\S+\s*|\s+", plan[1]):
                self._send(chunk({"content": piece}))
                time.sleep(0.01)
            finish = "stop"
        else:
            calls = calls_of(plan)
            self._send(chunk({"content": f"Calling `{calls[0][0]}`.\n"}))
            for i, (name, args) in enumerate(calls):
                self._send(
                    chunk(
                        {
                            "tool_calls": [
                                {"index": i, "id": f"call_{int(time.time()*1000)}_{i}", "type": "function",
                                 "function": {"name": name, "arguments": json.dumps(args)}}
                            ]
                        }
                    )
                )
            finish = "tool_calls"
        self._send(chunk({}, finish))
        self._send(
            json.dumps(
                {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [],
                 "usage": {"prompt_tokens": 42, "completion_tokens": 17, "total_tokens": 59,
                           # Some hosts (e.g. Fireworks) send explicit nulls here.
                           "prompt_tokens_details": {"cached_tokens": None},
                           "completion_tokens_details": {"reasoning_tokens": None}}}
            )
        )
        self._send("[DONE]")

    # ---- Anthropic Messages ----
    def messages(self, body):
        if not self.headers.get("x-api-key"):
            return self._json(401, {"type": "error", "error": {"type": "authentication_error", "message": "x-api-key header is required"}})
        msgs = body.get("messages") or []
        last = msgs[-1] if msgs else {}
        texts, images, after_tool = [], 0, False
        for m in reversed(msgs):
            if m.get("role") != "user":
                break
            t, n, tr = anthropic_user_text(m.get("content"))
            texts.insert(0, t)
            images += n
            after_tool = after_tool or tr
        text = "\n".join(texts)
        plan = decide(text, images, after_tool, body.get("tools"))
        model = body.get("model", "mock")
        self._sse_start()
        ev = lambda name, obj: self._send(json.dumps({"type": name, **obj}), event=name)
        ev("message_start", {"message": {"id": "msg_mock", "type": "message", "role": "assistant", "model": model,
                                          "content": [], "stop_reason": None, "stop_sequence": None,
                                          "usage": {"input_tokens": 42, "output_tokens": 0}}})
        ev("content_block_start", {"index": 0, "content_block": {"type": "text", "text": ""}})
        if plan[0] == "text":
            for piece in re.findall(r"\S+\s*|\s+", plan[1]):
                ev("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": piece}})
                time.sleep(0.01)
            ev("content_block_stop", {"index": 0})
            stop = "end_turn"
        else:
            calls = calls_of(plan)
            ev("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": f"Calling `{calls[0][0]}`.\n"}})
            ev("content_block_stop", {"index": 0})
            for i, (name, args) in enumerate(calls):
                ev("content_block_start", {"index": 1 + i, "content_block": {"type": "tool_use", "id": f"toolu_{int(time.time()*1000)}_{i}", "name": name, "input": {}}})
                ev("content_block_delta", {"index": 1 + i, "delta": {"type": "input_json_delta", "partial_json": json.dumps(args)}})
                ev("content_block_stop", {"index": 1 + i})
            stop = "tool_use"
        ev("message_delta", {"delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 17}})
        ev("message_stop", {})


    # ---- OpenAI Responses ----
    def openai_responses(self, body):
        items = body.get("input") or []
        texts, images, after_tool = [], 0, False
        for it in reversed(items):
            if it.get("type") == "function_call_output":
                after_tool = True
                texts.insert(0, str(it.get("output", "")))
                continue
            if it.get("role") != "user":
                break
            for p in it.get("content") or []:
                if p.get("type") == "input_text":
                    texts.insert(0, p.get("text", ""))
                elif p.get("type") == "input_image":
                    images += 1
                elif p.get("type") == "input_file":
                    texts.insert(0, f"[document {len(p.get('file_data', ''))} chars]")
        plan = decide("\n".join(texts), images, after_tool, body.get("tools"))
        model = body.get("model", "mock")
        self._sse_start()
        ev = lambda name, obj: self._send(json.dumps({"type": name, **obj}), event=name)
        ev("response.created", {"response": {"id": "resp_mock", "status": "in_progress"}})
        # A reasoning item first, so the encrypted round trip is exercised.
        ev("response.output_item.done", {"output_index": 0, "item": {"type": "reasoning", "id": "rs_mock", "summary": [{"type": "summary_text", "text": "thinking"}], "encrypted_content": "mock-encrypted"}})
        if plan[0] == "text":
            for piece in re.findall(r"\S+\s*|\s+", plan[1]):
                ev("response.output_text.delta", {"output_index": 1, "delta": piece})
                time.sleep(0.01)
            ev("response.output_item.done", {"output_index": 1, "item": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": plan[1]}]}})
        else:
            for i, (name, args) in enumerate(calls_of(plan)):
                ev("response.output_item.done", {"output_index": 1 + i, "item": {"type": "function_call", "call_id": f"call_{int(time.time()*1000)}_{i}", "name": name, "arguments": json.dumps(args)}})
        ev("response.completed", {"response": {"id": "resp_mock", "status": "completed", "model": model,
                                                "usage": {"input_tokens": 42, "output_tokens": 17,
                                                          "input_tokens_details": {"cached_tokens": 0},
                                                          "output_tokens_details": {"reasoning_tokens": 0}}}})

    # ---- Gemini generateContent ----
    def gemini(self, body):
        if not (self.headers.get("x-goog-api-key") or self.headers.get("authorization")):
            return self._json(403, {"error": {"code": 403, "message": "API key missing", "status": "PERMISSION_DENIED"}})
        contents = body.get("contents") or []
        texts, images, after_tool = [], 0, False
        for c in reversed(contents):
            if c.get("role") != "user":
                break
            for p in c.get("parts") or []:
                if "text" in p:
                    texts.insert(0, p["text"])
                elif "inlineData" in p:
                    mime = p["inlineData"].get("mimeType", "")
                    if mime == "application/pdf":
                        texts.insert(0, f"[document {mime}]")
                    else:
                        images += 1
                elif "functionResponse" in p:
                    after_tool = True
                    texts.insert(0, json.dumps(p["functionResponse"].get("response")))
        tools = [d for t in body.get("tools") or [] for d in t.get("functionDeclarations") or []]
        plan = decide("\n".join(texts), images, after_tool, tools)
        self._sse_start()

        def chunk(parts, finish=None, usage=False):
            cand = {"content": {"role": "model", "parts": parts}}
            if finish:
                cand["finishReason"] = finish
            out = {"candidates": [cand], "modelVersion": "mock"}
            if usage:
                out["usageMetadata"] = {"promptTokenCount": 42, "candidatesTokenCount": 17, "totalTokenCount": 59}
            self._send(json.dumps(out))

        chunk([{"text": "thinking", "thought": True}])
        if plan[0] == "text":
            pieces = re.findall(r"\S+\s*|\s+", plan[1])
            for piece in pieces[:-1]:
                chunk([{"text": piece}])
                time.sleep(0.01)
            chunk([{"text": pieces[-1]}], "STOP", True)
        else:
            chunk([{"functionCall": {"name": name, "args": args}, "thoughtSignature": "mock-sig"} for name, args in calls_of(plan)], "STOP", True)


def _plan_for_rounds(text, images, after_tool, tools):
    return decide(text, images, after_tool, tools)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8765)
    args = ap.parse_args()
    srv = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    log(f"mock LLM on http://127.0.0.1:{args.port}/v1 (OpenAI chat + Anthropic messages)")
    srv.serve_forever()


if __name__ == "__main__":
    main()

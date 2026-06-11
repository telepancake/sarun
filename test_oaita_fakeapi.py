#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "openai>=1.30",
#   "pytest>=8",
# ]
# ///
"""
test_oaita_fakeapi — round-trip tests for oaita_fakeclient against oaita_fakeserver.

Covers:
  1. GET /v1/models via client.models.list()
  2. Non-stream chat completion + request capture assertions
  3. Streaming chat — assembled StreamResult content + finish_reason
  4. Tool call, non-stream — function name, arguments, finish_reason
  5. Tool call, streaming — StreamResult.tool_calls reconstruction
  6. Error injection — BadRequestError (400) + RateLimitError (429)
  7. Empty queue → InternalServerError (500)
  8. Extra kwargs passthrough (temperature)
  9. Expect-mode: match-and-respond rules (string/callable matchers, once
     consumption, templated responses, queue fallthrough, reset)

Run standalone:
    ./test_oaita_fakeapi.py

Run under pytest:
    uv run --with "openai>=1.30" --with "pytest>=8" pytest -q test_oaita_fakeapi.py
"""

from __future__ import annotations

import importlib.machinery
import json
import sys
from pathlib import Path

import openai

# ── load both app modules via SourceFileLoader ────────────────────────────────
_HERE = Path(__file__).resolve().parent

_srv_loader = importlib.machinery.SourceFileLoader(
    "oaita_fakeserver", str(_HERE / "oaita_fakeserver"))
_srv = _srv_loader.load_module()

_cli_loader = importlib.machinery.SourceFileLoader(
    "oaita_fakeclient", str(_HERE / "oaita_fakeclient"))
_cli = _cli_loader.load_module()

FakeOpenAIServer = _srv.FakeOpenAIServer
CannedChat       = _srv.CannedChat
CannedError      = _srv.CannedError
CannedRaw        = _srv.CannedRaw

make_client      = _cli.make_client
CannedPrompt     = _cli.CannedPrompt
send             = _cli.send
assemble_stream  = _cli.assemble_stream
run_prompts      = _cli.run_prompts
StreamResult     = _cli.StreamResult

# ── check / _fails pattern ────────────────────────────────────────────────────
_fails: list[str] = []


def check(msg, cond):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


# ════════════════════════════════════════════════════════════════════════════
#  1 · GET /v1/models
# ════════════════════════════════════════════════════════════════════════════
def test_models_list():
    """client.models.list() should include 'test-model'."""
    with FakeOpenAIServer() as srv:
        client = make_client(srv.base_url)
        models = client.models.list()
        ids = [m.id for m in models.data]
        check("test-model in models.list()", "test-model" in ids)
        check("models.list() captured as GET /v1/models",
              any(r.path == "/v1/models" and r.method == "GET"
                  for r in srv.requests))


# ════════════════════════════════════════════════════════════════════════════
#  2 · Non-stream chat + request capture
# ════════════════════════════════════════════════════════════════════════════
def test_non_stream_chat():
    """Non-streaming chat returns the correct content; request is captured."""
    with FakeOpenAIServer() as srv:
        srv.enqueue(CannedChat(content="hi there"))
        client = make_client(srv.base_url)
        result = send(client, CannedPrompt(user="hello"))

        # Response assertions.
        msg = result.choices[0].message
        check("non-stream content == 'hi there'",
              msg.content == "hi there")
        check("non-stream model == 'test-model'",
              result.model == "test-model")
        check("non-stream finish_reason == 'stop'",
              result.choices[0].finish_reason == "stop")

        # Captured request assertions.
        req = srv.requests[-1]
        msgs = req.json["messages"]
        check("captured messages contains user 'hello'",
              any(m.get("role") == "user" and m.get("content") == "hello"
                  for m in msgs))
        check("captured model == 'test-model'",
              req.model == "test-model")
        check("authorization header carries bearer token",
              req.authorization is not None
              and req.authorization.startswith("Bearer "))


# ════════════════════════════════════════════════════════════════════════════
#  3 · Streaming chat
# ════════════════════════════════════════════════════════════════════════════
def test_streaming_chat():
    """Streaming chat assembles content and finish_reason correctly."""
    with FakeOpenAIServer() as srv:
        srv.enqueue(CannedChat(content="streamed words here", n_content_chunks=3))
        client = make_client(srv.base_url)
        result = send(client, CannedPrompt(user="stream please", stream=True))

        check("stream result is StreamResult",
              isinstance(result, StreamResult))
        check("stream content == 'streamed words here'",
              result.content == "streamed words here")
        check("stream finish_reason == 'stop'",
              result.finish_reason == "stop")
        check("stream saw multiple chunks",
              len(result.chunks) >= 3)
        check("server saw stream:true",
              srv.requests[-1].json.get("stream") is True)


# ════════════════════════════════════════════════════════════════════════════
#  4 · Tool call, non-stream
# ════════════════════════════════════════════════════════════════════════════
def test_tool_call_non_stream():
    """Non-streaming tool call: name, arguments, finish_reason are correct."""
    tools_def = [{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            },
        },
    }]
    with FakeOpenAIServer() as srv:
        srv.enqueue(CannedChat(tool_calls=[("get_weather", '{"city":"sf"}', "call_1")]))
        client = make_client(srv.base_url)
        result = send(client, CannedPrompt(
            user="What's the weather in SF?",
            tools=tools_def,
        ))

        tc = result.choices[0].message.tool_calls[0]
        check("tool call name == 'get_weather'",
              tc.function.name == "get_weather")
        check("tool call arguments parses to {city: sf}",
              json.loads(tc.function.arguments) == {"city": "sf"})
        check("tool call finish_reason == 'tool_calls'",
              result.choices[0].finish_reason == "tool_calls")

        # Verify tools were captured in request body.
        req = srv.requests[-1]
        check("server captured tools in request",
              req.tools is not None and len(req.tools) == 1)
        check("server captured tool name 'get_weather'",
              req.tools[0]["function"]["name"] == "get_weather")


# ════════════════════════════════════════════════════════════════════════════
#  5 · Tool call, streaming
# ════════════════════════════════════════════════════════════════════════════
def test_tool_call_streaming():
    """Streaming tool call: StreamResult.tool_calls reconstructs correctly."""
    tools_def = [{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            },
        },
    }]
    with FakeOpenAIServer() as srv:
        srv.enqueue(CannedChat(tool_calls=[("get_weather", '{"city":"sf"}', "call_1")]))
        client = make_client(srv.base_url)
        result = send(client, CannedPrompt(
            user="What's the weather in SF?",
            tools=tools_def,
            stream=True,
        ))

        check("streaming tool result is StreamResult",
              isinstance(result, StreamResult))
        check("streaming tool_calls list is non-empty",
              len(result.tool_calls) == 1)
        tc = result.tool_calls[0] if result.tool_calls else {}
        check("streaming tool name == 'get_weather'",
              tc.get("name") == "get_weather")
        check("streaming tool arguments parses to {city: sf}",
              json.loads(tc.get("arguments", "{}")) == {"city": "sf"})


# ════════════════════════════════════════════════════════════════════════════
#  6 · Error injection: 400 and 429
# ════════════════════════════════════════════════════════════════════════════
def test_error_injection():
    """CannedError(400) → BadRequestError; CannedError(429) → RateLimitError."""
    with FakeOpenAIServer() as srv:
        client = make_client(srv.base_url)  # max_retries=0 by default

        # 400 Bad Request.
        srv.enqueue(CannedError(status_code=400, message="bad input"))
        raised_400 = None
        try:
            send(client, CannedPrompt(user="trigger 400"))
        except openai.BadRequestError as exc:
            raised_400 = exc
        except Exception as exc:
            raised_400 = exc
        check("400 raises BadRequestError",
              isinstance(raised_400, openai.BadRequestError))
        check("400 status code == 400",
              getattr(raised_400, "status_code", None) == 400)

        # 429 Rate Limit.
        srv.enqueue(CannedError(status_code=429, message="too many requests"))
        raised_429 = None
        try:
            send(client, CannedPrompt(user="trigger 429"))
        except openai.RateLimitError as exc:
            raised_429 = exc
        except Exception as exc:
            raised_429 = exc
        check("429 raises RateLimitError",
              isinstance(raised_429, openai.RateLimitError))
        check("429 status code == 429",
              getattr(raised_429, "status_code", None) == 429)


# ════════════════════════════════════════════════════════════════════════════
#  7 · Empty queue → 500
# ════════════════════════════════════════════════════════════════════════════
def test_empty_queue():
    """Sending with nothing enqueued raises an APIStatusError (HTTP 500)."""
    with FakeOpenAIServer() as srv:
        client = make_client(srv.base_url)
        raised = None
        try:
            send(client, CannedPrompt(user="nothing enqueued"))
        except openai.APIStatusError as exc:
            raised = exc
        except Exception as exc:
            raised = exc
        check("empty queue raises APIStatusError",
              isinstance(raised, openai.APIStatusError))
        check("empty queue status_code == 500",
              getattr(raised, "status_code", None) == 500)


# ════════════════════════════════════════════════════════════════════════════
#  8 · Extra kwargs passthrough (temperature)
# ════════════════════════════════════════════════════════════════════════════
def test_extra_passthrough():
    """extra={temperature:0.2} is forwarded verbatim in the request body."""
    with FakeOpenAIServer() as srv:
        srv.enqueue(CannedChat(content="cool"))
        client = make_client(srv.base_url)
        send(client, CannedPrompt(user="x", extra={"temperature": 0.2}))
        req = srv.requests[-1]
        check("temperature 0.2 captured in request body",
              req.json.get("temperature") == 0.2)


# ════════════════════════════════════════════════════════════════════════════
#  9 · Expect-mode: match-and-respond rules
# ════════════════════════════════════════════════════════════════════════════
def test_expect_mode():
    """expect() rules answer by match, not arrival order; once-consumption;
    callable responses template off the request; fallthrough to the queue."""
    with FakeOpenAIServer() as srv:
        client = make_client(srv.base_url)
        # Registration order ≠ arrival order: rules answer whichever request
        # MATCHES, so a scenario script needn't predict the exact call order.
        srv.expect("alpha topic", CannedChat(content="ALPHA"))
        srv.expect("beta topic", CannedChat(content="BETA"))
        r1 = send(client, CannedPrompt(user="please cover beta topic"))
        r2 = send(client, CannedPrompt(user="now the alpha topic"))
        check("string matcher answers by content, not order",
              r1.choices[0].message.content == "BETA"
              and r2.choices[0].message.content == "ALPHA")

        # once=True rules are consumed: a repeat falls through (here: queue).
        srv.enqueue(CannedChat(content="FROM QUEUE"))
        r3 = send(client, CannedPrompt(user="alpha topic again"))
        check("a consumed once-rule falls through to the queue",
              r3.choices[0].message.content == "FROM QUEUE")

        # Persistent rule (once=False) keeps answering.
        srv.expect("evergreen", CannedChat(content="STILL HERE"), once=False)
        a = send(client, CannedPrompt(user="evergreen 1"))
        b = send(client, CannedPrompt(user="evergreen 2"))
        check("once=False rule answers repeatedly",
              a.choices[0].message.content == "STILL HERE"
              and b.choices[0].message.content == "STILL HERE")

        # Callable matcher + callable response (request-derived templating).
        srv.expect(lambda req: req.json.get("temperature") == 0.7,
                   lambda req: CannedChat(
                       content=f"temp was {req.json['temperature']}"))
        c = send(client, CannedPrompt(user="anything",
                                      extra={"temperature": 0.7}))
        check("callable matcher + templated response work",
              c.choices[0].message.content == "temp was 0.7")

        # No match, empty queue → the loud 500 (exact-control contract kept).
        raised = None
        try:
            send(client, CannedPrompt(user="nothing matches this"))
        except Exception as exc:
            raised = exc
        check("unmatched request still raises the loud 500",
              getattr(raised, "status_code", None) == 500)

        # reset() clears the rules too.
        srv.reset()
        raised2 = None
        try:
            send(client, CannedPrompt(user="evergreen 3"))
        except Exception as exc:
            raised2 = exc
        check("reset() clears expectations",
              getattr(raised2, "status_code", None) == 500)


# ════════════════════════════════════════════════════════════════════════════
#  __main__
# ════════════════════════════════════════════════════════════════════════════
if __name__ == "__main__":
    tests = [
        test_models_list,
        test_non_stream_chat,
        test_streaming_chat,
        test_tool_call_non_stream,
        test_tool_call_streaming,
        test_error_injection,
        test_empty_queue,
        test_extra_passthrough,
        test_expect_mode,
    ]
    for t in tests:
        print(f"\n── {t.__name__} ──")
        try:
            t()
        except Exception:
            import traceback
            traceback.print_exc()
            _fails.append(t.__name__)

    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)

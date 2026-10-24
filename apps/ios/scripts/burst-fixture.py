#!/usr/bin/env python3
"""Writes apps/ios/AppTests/burst.stream, the stream StallBudgetTests replays
(R-PERF-7, ticket 127): a turn as the fake fleet's host streams it to a
phone (the lines `decodeStreamLine` takes, in the fleet's shapes: its
`read` before the long answer, then numbered lines at 200 tokens a second,
about eleven tokens a line), stretched to 20 s of answer in bursts of 20
tokens every 100 ms, with two 1 MiB tool outputs logged on the way.

Each line is `<ms from the start>\t<stream line>`. A tool output reads
`@1MiB@`, which the test fills with 1 MiB of text before decoding: the
file stays small. Deterministic: run it again and nothing changes.

    apps/ios/scripts/burst-fixture.py > apps/ios/AppTests/burst.stream
"""
import json

SESSION = "0192f0a0-0000-7000-8000-000000000127"
USAGE = {"inputTokens": 1, "outputTokens": 2, "cacheReadTokens": 0, "cacheWriteTokens": 0, "reasoningTokens": 0}
out = []
n = 0


def emit(at, body, logged=False):
    global n
    line = {"sessionId": SESSION, **body}
    if logged:
        n += 1
        line = {"id": f"0192f0a0-0000-7000-8000-{n:012x}", "sessionId": SESSION, "timeMs": at, **body}
    out.append(f"{at}\t{json.dumps(line, separators=(',', ':'))}")


def tokens(first, count):
    """The fleet's numbered lines, a token at a time, from line `first`."""
    words = []
    line = first
    while len(words) < count:
        text = f"line {line:05d}: the quick brown fox jumps over the lazy dog again\n"
        parts = text.split(" ")
        words += [p if i == 0 else " " + p for i, p in enumerate(parts)]
        line += 1
    return words[:count]


at = 0
emit(at, {"type": "turn.started", "turnId": "t1", "model": {"instance": "anthropic", "model": "claude-sonnet-4-6"},
          "provider": "anthropic", "wireApi": "anthropic-messages", "permissionMode": "default"}, logged=True)
emit(at, {"type": "item.completed", "turnId": "t1", "itemId": "u1", "item": {"kind": "userText", "text": "Please stream a long answer"}}, logged=True)
line = 1
for part in (1, 2):
    call = f"toolu_01Stream{part}"
    at += 100
    emit(at, {"type": "item.started", "turnId": "t1", "itemId": f"c{part}", "item": {"kind": "toolCall", "callId": call, "name": "read"}})
    emit(at, {"type": "item.completed", "turnId": "t1", "itemId": f"c{part}", "item": {"kind": "toolCall", "callId": call, "name": "read", "input": {"path": f"big-{part}.log"}}}, logged=True)
    at += 100
    emit(at, {"type": "item.completed", "turnId": "t1", "itemId": f"o{part}", "item": {"kind": "toolResult", "callId": call, "output": "@1MiB@", "isError": False}}, logged=True)
    at += 100
    emit(at, {"type": "item.started", "turnId": "t1", "itemId": f"a{part}", "item": {"kind": "assistantText"}})
    # 10 s at 200 tokens a second: 20 tokens every 100 ms.
    words = tokens(line, 2000)
    line += 2000 // 11 + 1
    for burst in range(100):
        at += 100
        for word in words[burst * 20:(burst + 1) * 20]:
            emit(at, {"type": "item.delta", "turnId": "t1", "itemId": f"a{part}", "delta": {"kind": "text", "text": word}})
    emit(at, {"type": "item.completed", "turnId": "t1", "itemId": f"a{part}", "item": {"kind": "assistantText", "text": "".join(words)}}, logged=True)
at += 100
emit(at, {"type": "turn.completed", "turnId": "t1", "status": "completed", "usage": USAGE, "durationMs": at}, logged=True)
print("\n".join(out))

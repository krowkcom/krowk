#!/usr/bin/env python3
"""Phase 1 exit: an independent lossless spot-check of the krowk session store.

For each provider (claude, cursor, opencode) five sessions are picked from the
store and `krowk sessions show <id> --json` is compared against counts this
script derives from the SOURCE itself (Claude/Cursor JSONL, opencode.db) —
never from krowk or krowk.db. The source-side rules mirror the importers in
crates/gruntas-import/src/{claude,cursor,opencode}.rs and turn.rs; the few places
where the store is deliberately not mirrored are labelled in the output.

krowk always runs with XDG_DATA_HOME=--data and XDG_CACHE_HOME=a scratch copy
of ~/.cache/krowk, so the real store and cache are never written. --rebuild
runs `krowk sessions rebuild --yes` into the scratch store, hashing every
source file before and after.

Stdlib only. Usage:
    scripts/phase1_spotcheck.py --krowk target/release/krowk --data /tmp/p1 --rebuild
"""

import argparse
import hashlib
import json
import os
import shutil
import sqlite3
import statistics
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

HOME = Path.home()
CLAUDE_ROOT = HOME / ".claude/projects"
CURSOR_ROOT = HOME / ".cursor/projects"
OPENCODE_DB = HOME / ".local/share/opencode/opencode.db"
PROVIDERS = ("claude", "cursor", "opencode")

# Mirrored importer constants (file:line in crates/gruntas-import/src).
MAX_LINE_BYTES = 16 << 20        # jsonl.rs:9
MAX_FILE_BYTES = 64 << 20        # home.rs:11 (open_home default cap)
INTERRUPT_PREFIX = "[Request interrupted"   # claude.rs:44
INJECTED_TAGS = ("<local-command-stdout>", "<bash-stdout>", "<task-notification>", "<system-reminder>")  # claude.rs:643
KNOWN_PARTS = {"text", "thinking", "tool_call", "tool_result", "image", "file", "patch", "step", "unknown"}  # part.rs:15
OC_MESSAGE_RAW_LIMIT = 200_000   # opencode.rs:44
OC_PART_RAW_LIMIT = 5_000_000    # opencode.rs:48


def die(msg):
    print(f"phase1_spotcheck: {msg}", file=sys.stderr)
    sys.exit(2)


# ---------------------------------------------------------------- krowk runs

class Krowk:
    def __init__(self, binary, data, cache):
        self.binary = binary
        # Scratch data, cache and config, and no update check: nothing a run
        # does lands in the real home.
        self.env = dict(os.environ, XDG_DATA_HOME=str(data), XDG_CACHE_HOME=str(cache),
                        XDG_CONFIG_HOME=str(cache / "config"), KROWK_NO_UPDATE_CHECK="1")

    def run(self, *args, check=True):
        t0 = time.perf_counter()
        p = subprocess.run([self.binary, *args], env=self.env, capture_output=True, text=True)
        ms = (time.perf_counter() - t0) * 1000
        if check and p.returncode != 0:
            die(f"krowk {' '.join(args)} exited {p.returncode}: {p.stderr.strip() or p.stdout[:500]}")
        return p, ms

    def json(self, *args, check=True):
        p, ms = self.run(*args, "--json", check=check)
        try:
            return json.loads(p.stdout), ms
        except json.JSONDecodeError:
            die(f"krowk {' '.join(args)} --json printed no JSON: {p.stdout[:300]!r} {p.stderr[:300]!r}")


# ------------------------------------------------------------ source hashing

def source_files():
    files = sorted(str(p) for p in CLAUDE_ROOT.rglob("*.jsonl")) if CLAUDE_ROOT.is_dir() else []
    files += sorted(str(p) for p in CURSOR_ROOT.glob("*/agent-transcripts/*/*.jsonl")) if CURSOR_ROOT.is_dir() else []
    # The -shm index is rewritten by every reader, opencode's own included; it
    # holds no transcript, so it is not part of the source.
    for p in (OPENCODE_DB, Path(str(OPENCODE_DB) + "-wal")):
        if p.exists():
            files.append(str(p))
    return files


def sha256(path, limit=None):
    h = hashlib.sha256()
    left = limit
    with open(path, "rb") as f:
        while True:
            n = 1 << 20 if left is None else min(1 << 20, left)
            if n == 0:
                break
            b = f.read(n)
            if not b:
                break
            h.update(b)
            if left is not None:
                left -= len(b)
    return h.hexdigest()


def snapshot():
    out = {}
    for f in source_files():
        try:
            st = os.stat(f)
            out[f] = {"size": st.st_size, "mtime_ns": st.st_mtime_ns, "sha256": sha256(f)}
        except OSError as e:
            out[f] = {"error": str(e)}
    return out


def compare_snapshots(before, after):
    """Unchanged, appended (old bytes intact: a live writer), or rewritten."""
    res = {"files": len(before), "unchanged": 0, "appended": [], "rewritten": [], "new": [], "gone": [], "live_db": []}
    for f, b in before.items():
        a = after.get(f)
        if a is not None and a != b and f.startswith(str(OPENCODE_DB)):
            # opencode rewrites its own database and WAL while it runs; krowk
            # opens it read-only (the importer's reads_are_read_only_under_wal
            # test), so a change here is opencode's, not the import's.
            res["live_db"].append(f)
            continue
        if a is None:
            res["gone"].append(f)
        elif a == b:
            res["unchanged"] += 1
        elif "size" in a and "size" in b and a["size"] >= b["size"] and sha256(f, b["size"]) == b["sha256"]:
            res["appended"].append(f)
        else:
            res["rewritten"].append(f)
    res["new"] = [f for f in after if f not in before]
    return res


# ------------------------------------------------------- shared source logic

def is_str(v):
    return v is None or isinstance(v, str)


def is_bool(v):
    return v is None or isinstance(v, bool)


def as_int(v):
    return v if isinstance(v, int) and not isinstance(v, bool) else 0


# path -> byte size the import saw (from the post-import hash), so a file a
# live agent appended to since is parsed only as far as krowk read it.
SIZE_PIN = {}


def jsonl_payloads(path):
    """jsonl.rs read_jsonl: terminated lines only (an unterminated tail is a
    partial write, not read), >16MB skipped, CR/LF trimmed, blanks dropped,
    undecodable JSON skipped. Yields (line_no, value)."""
    with open(path, "rb") as f:
        data = f.read(SIZE_PIN[path]) if path in SIZE_PIN else f.read()
    line_no = 0
    start = 0
    while True:
        nl = data.find(b"\n", start)
        if nl < 0:
            break
        raw = data[start:nl + 1]
        start = nl + 1
        line_no += 1
        if len(raw) > MAX_LINE_BYTES:
            continue
        payload = raw.rstrip(b"\r\n")
        if not payload:
            continue
        try:
            v = json.loads(payload.decode("utf-8", errors="replace"))
        except ValueError:
            continue
        yield line_no, v


def spans(candidates, starts):
    """turn.rs split_turns: the first candidate always opens a turn."""
    out = []
    for i, c in enumerate(candidates):
        if i == 0 or starts(c):
            out.append(i)
    return [(s, (out[k + 1] if k + 1 < len(out) else len(candidates))) for k, s in enumerate(out)]


def new_result():
    return {
        "messages": 0, "turns": 0, "parts": Counter(), "tok_uniq": [0, 0, 0, 0, 0], "tok_mirror": [0, 0, 0, 0, 0],
        "first_user": None, "last_user": None, "tool_results": 0, "unlinked": 0, "children": 0, "notes": [],
    }


def user_texts(messages):
    """First text part of every user message that has one — the same rule the
    store side uses, applied to either."""
    out = []
    for role, parts in messages:
        if role != "user":
            continue
        for kind, text in parts:
            if kind == "text" and text:
                out.append(text)
                break
    return out


def finish(res, messages, calls, results):
    res["messages"] = len(messages)
    for _, parts in messages:
        for kind, _ in parts:
            res["parts"][kind] += 1
    t = user_texts(messages)
    res["first_user"], res["last_user"] = (t[0], t[-1]) if t else (None, None)
    res["tool_results"] = len(results)
    res["unlinked"] = sum(1 for r in results if not r or r not in calls)
    # Which call every result links to, in transcript order — not how many
    # link, nor how many point at each call: two results that swapped calls
    # must not compare equal. Both sides list results in message order.
    res["result_links"] = [r if r and r in calls else "" for r in results]
    res["call_ids"] = frozenset(calls)
    return res


# ------------------------------------------------------------------- claude

LINE_STR_FIELDS = ("uuid", "sessionId", "agentId", "cwd", "promptSource", "subtype", "hookEvent", "aiTitle", "summary")


def claude_line_ok(o):
    """claude.rs Line: serde rejects a field of the wrong type, skipping the line."""
    if not isinstance(o, dict):
        return False
    if not all(is_str(o.get(k)) for k in LINE_STR_FIELDS):
        return False
    if not (is_bool(o.get("isMeta")) and is_bool(o.get("isApiErrorMessage"))):
        return False
    origin = o.get("origin")
    if origin is not None and not (isinstance(origin, dict) and is_str(origin.get("kind"))):
        return False
    m = o.get("message")
    if m is not None and not (isinstance(m, dict) and is_str(m.get("role")) and is_str(m.get("model"))):
        return False
    t = o.get("type")
    return isinstance(t, str) and t != ""


def claude_block_ok(b):
    return (isinstance(b, dict) and all(is_str(b.get(k)) for k in ("type", "id", "name", "tool_use_id", "signature", "text"))
            and is_bool(b.get("is_error")))


def claude_block(b):
    """claude.rs block(): (part type, text or None, tool id or None, is_result)."""
    if not claude_block_ok(b):
        return "unknown", None
    k = b.get("type") or ""
    if k == "tool_use":
        return "tool_call", ("call", b.get("id") or "")
    if k == "tool_result":
        return "tool_result", ("result", b.get("tool_use_id") or "")
    if k in ("thinking", "redacted_thinking"):
        return "thinking", None
    if k in KNOWN_PARTS:
        return k, (("text", b.get("text")) if k == "text" and isinstance(b.get("text"), str) else None)
    return "unknown", None


def claude_parts(content):
    if content is None or content == "":
        return []
    if isinstance(content, str):
        return [("text", ("text", content))]
    if isinstance(content, list):
        return [claude_block(b) for b in content]
    return [("unknown", None)]


def claude_leading_text(content):
    """claude.rs leading_text: whole string, or first non-empty text block —
    empty if any block fails to decode (Vec<ContentBlock> is all-or-nothing)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        if not all(claude_block_ok(b) for b in content):
            return ""
        for b in content:
            if b.get("type") == "text" and b.get("text"):
                return b["text"]
    return ""


def claude_binding(path):
    """The foreign session id the file binds on (claude.rs thread()):
    a subagent's first agentId (else filename id), a session's first sessionId
    (else filename id)."""
    p = Path(path)
    sub = p.parent.name == "subagents"
    stem = p.name[:-len(".jsonl")]
    fallback = stem[len("agent-"):] if sub and stem.startswith("agent-") else stem
    key = "agentId" if sub else "sessionId"
    session = ""
    for _, o in jsonl_payloads(path):
        if not claude_line_ok(o):
            continue
        if not session and o.get("sessionId"):
            session = o["sessionId"]
        if o.get(key):
            return o[key], (session if sub else ""), sub
    return fallback, session, sub


def claude_index():
    """binding id -> [files]; parent session id -> child binding ids."""
    idx, children = defaultdict(list), defaultdict(set)
    for f in sorted(CLAUDE_ROOT.rglob("*.jsonl")):
        # Only what the import saw: a file created, or gone, since is not in it.
        if SIZE_PIN and str(f) not in SIZE_PIN:
            continue
        try:
            if f.stat().st_size > MAX_FILE_BYTES:
                continue
        except OSError:
            continue
        bid, parent, sub = claude_binding(str(f))
        idx[bid].append(str(f))
        if sub and parent and parent != bid:
            children[parent].add(bid)
    return idx, children


def claude_index_all():
    """Every Claude binding id with a transcript on disk now."""
    out = set()
    for f in CLAUDE_ROOT.rglob("*.jsonl"):
        try:
            if f.stat().st_size > MAX_FILE_BYTES:
                continue
        except OSError:
            continue
        out.add(claude_binding(str(f))[0])
    return out


def claude_source(files, n_children):
    res = new_result()
    if len(files) != 1:
        res["notes"].append(f"{len(files)} source files bind here; only the first is parsed")
    path = files[0]
    messages, candidates, line_usage = [], [], []
    seen_fids = set()
    calls, results = set(), []
    by_msg_id = {}
    for line_no, o in jsonl_payloads(path):
        if not claude_line_ok(o):
            continue
        kind = o["type"]
        if kind not in ("user", "assistant", "system"):
            continue  # attachments, titles, mode lines… are events or classified, never messages
        fid = o.get("uuid") or f"{Path(path).stem}:line:{line_no}"
        usage = [0, 0, 0, 0, 0]
        if kind == "system":
            c = o.get("content")
            parts = [] if c is None or c == "" else ([("text", ("text", c))] if isinstance(c, str) else [("unknown", None)])
            role, cand = "system", {"role": "system", "meta": True}
        else:
            m = o.get("message") or {}
            role = "user" if kind == "user" else ("error" if o.get("isApiErrorMessage") else "assistant")
            parts = claude_parts(m.get("content")) if o.get("message") is not None else []
            u = m.get("usage")
            if isinstance(u, dict):
                out = as_int(u.get("output_tokens"))
                det = u.get("output_tokens_details")
                think = min(max(as_int(det.get("thinking_tokens")) if isinstance(det, dict) else 0, 0), max(out, 0))
                # [input, output excl. thinking, cache_read, cache_write, thinking]
                usage = [as_int(u.get("input_tokens")), out - think,
                         as_int(u.get("cache_read_input_tokens")), as_int(u.get("cache_creation_input_tokens")), think]
                # Robust invariant: Claude writes one line per content block,
                # each repeating its API message's usage (output growing as it
                # streams): one message.id counts once, at its largest output
                # (first line wins a tie). Keyed session-wide, not per turn.
                key = m.get("id") or fid
                if key not in by_msg_id or usage[1] + usage[4] > by_msg_id[key][1] + by_msg_id[key][4]:
                    by_msg_id[key] = usage
            text = claude_leading_text(m.get("content")) if o.get("message") is not None else ""
            origin = o.get("origin") or {}
            injected = bool(origin.get("kind")) and origin.get("kind") != "human"
            injected = injected or o.get("promptSource") == "system" or text.startswith(INJECTED_TAGS)
            cand = {
                "role": role,
                "meta": bool(o.get("isMeta")) or (role == "user" and injected),
                "interrupt": role == "user" and text.startswith(INTERRUPT_PREFIX),
            }
        cand["types"] = [k for k, _ in parts]
        candidates.append(cand)
        line_usage.append(usage)
        # The store keeps the first row per foreign id (writer.rs:244); turns
        # and their costs are computed before that, over every line.
        if fid in seen_fids:
            continue
        seen_fids.add(fid)
        messages.append((role, [(k, (x[1] if x and x[0] == "text" else None)) for k, x in parts]))
        for k, x in parts:
            if x and x[0] == "call":
                calls.add(x[1])
            elif x and x[0] == "result":
                results.append(x[1])
    starts = lambda c: (c["role"] == "user" and not c["meta"] and not c.get("interrupt")
                        and any(t != "tool_result" for t in c["types"]))
    sp = spans(candidates, starts)
    res["turns"] = len(sp)
    # Diagnostic only: every line's usage summed, the pre-#82 importer rule.
    for u in line_usage:
        for i in range(4):
            res["tok_mirror"][i] += u[i]
    res["tok_mirror"][4] = sum(sum(u) for u in line_usage)
    for u in by_msg_id.values():
        for i in range(4):
            res["tok_uniq"][i] += u[i]
    res["reasoning"] = sum(u[4] for u in by_msg_id.values())
    # Claude's total counts every billed token, thinking included (claude.rs add_to).
    res["tok_uniq"][4] = sum(res["tok_uniq"][:4]) + res["reasoning"]
    res["children"] = n_children
    return finish(res, messages, calls, results)


# ------------------------------------------------------------------- cursor

def cursor_block(b):
    """cursor.rs block(): None for an empty text, else (type, payload)."""
    if b is None:
        return ("unknown", None)
    if isinstance(b, str):
        return ("text", ("text", b)) if b else None
    if not isinstance(b, dict):
        return ("unknown", None)
    if not (all(is_str(b.get(k)) for k in ("type", "id", "name", "tool_use_id", "callID", "call_id"))
            and is_bool(b.get("isError")) and is_bool(b.get("is_error"))):
        return ("unknown", None)
    k = b.get("type") or ""
    if k == "text":
        t = b.get("text")
        if isinstance(t, str):
            return ("text", ("text", t)) if t else None
        return None if t is None else ("text", None)
    if k == "tool_use":
        return ("tool_call", ("call", b.get("id") or None))
    if k == "tool_result":
        tid = next((b.get(x) for x in ("tool_use_id", "callID", "call_id", "id") if b.get(x)), "")
        return ("tool_result", ("result", tid))
    return (k if k in KNOWN_PARTS else "unknown", None)


def cursor_source(path):
    res = new_result()
    messages, candidates = [], []
    calls, results = set(), []
    for line_no, o in jsonl_payloads(path):
        if o is None:
            o = {}
        if not isinstance(o, dict) or not is_str(o.get("role")) or not is_str(o.get("type")):
            continue
        m = o.get("message")
        if m is not None and not isinstance(m, dict):
            continue
        role, kind = o.get("role") or "", o.get("type") or ""
        if kind and m is None:
            continue  # typed furniture (turn_ended…) is classified
        if role not in ("user", "assistant", "system", "tool", "error"):
            continue
        c = (m or {}).get("content")
        if c is None:
            parts = []
        elif isinstance(c, str):
            parts = [("text", ("text", c))] if c else []
        elif isinstance(c, list):
            parts = [p for p in (cursor_block(b) for b in c) if p is not None]
        else:
            parts = [("unknown", None)]
        k = 0
        for i, (t, x) in enumerate(parts):
            if t == "tool_call":
                # An id-less tool_use is keyed by its line (cursor.rs:274).
                tid = x[1] or (f"cursor:{line_no}" if k == 0 else f"cursor:{line_no}:{k}")
                if not x[1]:
                    k += 1
                calls.add(tid)
            elif t == "tool_result":
                results.append(x[1])
        candidates.append({"role": role, "types": [t for t, _ in parts]})
        messages.append((role, [(t, (x[1] if x and x[0] == "text" else None)) for t, x in parts]))
    starts = lambda c: c["role"] == "user" and any(t != "tool_result" for t in c["types"])
    res["turns"] = len(spans(candidates, starts))
    return finish(res, messages, calls, results)


# ----------------------------------------------------------------- opencode

def oc_connect():
    return sqlite3.connect(f"file:{OPENCODE_DB}?mode=ro", uri=True)


def oc_part_kind(t):
    return {"text": "text", "reasoning": "thinking", "file": "file", "patch": "patch",
            "step-start": "step", "step-finish": "step"}.get(t, t)


def oc_part_ok(v):
    """opencode.rs PartData::from_value: an object whose named fields are the right type."""
    if v is None:
        return True
    if not isinstance(v, dict):
        return False
    if not all(is_str(v.get(k)) for k in ("text", "snapshot", "type", "tool", "callID")):
        return False
    s = v.get("state")
    return s is None or (isinstance(s, dict) and is_str(s.get("status")))


def opencode_source(db, sid):
    # One read transaction: a live opencode write lands wholly before or after.
    db.execute("BEGIN")
    try:
        return _opencode_source(db, sid)
    finally:
        db.execute("COMMIT")


def _opencode_source(db, sid):
    res = new_result()
    messages, candidates, toks = [], [], []
    calls, results = set(), []
    rows = db.execute("SELECT id, length(CAST(data AS BLOB)) FROM message WHERE session_id = ? ORDER BY time_created, rowid", (sid,)).fetchall()
    for mid, size in rows:
        if size <= OC_MESSAGE_RAW_LIMIT:
            (data,) = db.execute("SELECT data FROM message WHERE id = ?", (mid,)).fetchone()
            try:
                v = json.loads(data if isinstance(data, str) else data.decode("utf-8", "replace"))
            except ValueError:
                continue
            if not isinstance(v, dict):
                continue
            role, tk = v.get("role"), v.get("tokens")
        else:
            # The importer scans a prefix for these; SQLite reads the same fields whole.
            role, tk = db.execute("SELECT json_extract(data,'$.role'), json_extract(data,'$.tokens') FROM message WHERE id = ?", (mid,)).fetchone()
            tk = json.loads(tk) if tk else None
            res["notes"].append(f"message {mid} over {OC_MESSAGE_RAW_LIMIT}B: fields read via json_extract")
        if role not in ("user", "assistant", "system"):
            continue  # skipped by the importer (opencode.rs:478)
        tk = tk if isinstance(tk, dict) else {}
        cache = tk.get("cache") if isinstance(tk.get("cache"), dict) else {}
        toks.append([as_int(tk.get("input")), as_int(tk.get("output")), as_int(cache.get("read")), as_int(cache.get("write")), as_int(tk.get("reasoning"))])
        parts = []
        for pid, psize in db.execute("SELECT id, length(CAST(data AS BLOB)) FROM part WHERE message_id = ? ORDER BY time_created, rowid", (mid,)):
            if psize <= OC_PART_RAW_LIMIT:
                (pdata,) = db.execute("SELECT data FROM part WHERE id = ?", (pid,)).fetchone()
                try:
                    pv = json.loads(pdata if isinstance(pdata, str) else pdata.decode("utf-8", "replace"))
                except ValueError:
                    parts.append(("unknown", None))
                    continue
            else:
                t, st = db.execute("SELECT json_extract(data,'$.type'), json_extract(data,'$.state.status') FROM part WHERE id = ?", (pid,)).fetchone()
                pv = {"type": t, "state": {"status": st}, "callID": db.execute("SELECT json_extract(data,'$.callID') FROM part WHERE id = ?", (pid,)).fetchone()[0]}
                res["notes"].append(f"part {pid} over 5MB: fields read via json_extract")
            if not oc_part_ok(pv):
                parts.append(("unknown", None))
                continue
            pv = pv or {}
            t = pv.get("type") or ""
            if t == "tool":
                # One row is the call and, once finished, its result twin.
                cid = pv.get("callID") or pid
                parts.append(("tool_call", None))
                calls.add(cid)
                if ((pv.get("state") or {}).get("status") or "") in ("completed", "error"):
                    parts.append(("tool_result", None))
                    results.append(cid)
                continue
            k = oc_part_kind(t)
            parts.append((k if k in KNOWN_PARTS else "unknown", pv.get("text") if k == "text" else None))
        candidates.append({"role": role, "types": [k for k, _ in parts]})
        messages.append((role, parts))
    starts = lambda c: c["role"] == "user" and any(t != "tool_result" for t in c["types"])
    res["turns"] = len(spans(candidates, starts))
    for t in toks:
        for i in range(4):
            res["tok_uniq"][i] += t[i]
    # opencode's total leaves reasoning out (opencode.rs turns()).
    res["tok_uniq"][4] = sum(res["tok_uniq"][:4])
    res["tok_mirror"] = list(res["tok_uniq"])
    res["reasoning"] = sum(t[4] for t in toks)
    res["children"] = db.execute("SELECT count(*) FROM session WHERE parent_id = ?", (sid,)).fetchone()[0]
    return finish(res, messages, calls, results)


# --------------------------------------------------------------- store side

def store_counts(show, n_children):
    d = show["data"]
    res = new_result()
    messages, calls, results = [], set(), []
    for m in d["messages"]:
        parts = []
        for p in m["parts"]:
            text = p["data"].get("text") if p["type"] == "text" and isinstance(p.get("data"), dict) else None
            parts.append((p["type"], text if isinstance(text, str) else None))
            if p["type"] == "tool_result":
                results.append(p.get("tool_call_id") or "")
                if not p.get("linked"):
                    res["unlinked_flag"] = res.get("unlinked_flag", 0) + 1
            elif p["type"] == "tool_call":
                calls.add(p.get("tool_call_id") or "")
        messages.append((m["role"], parts))
    res["turns"] = len(d["turns"])
    keys = ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "total_tokens")
    res["tok_uniq"] = [sum(t[k] for t in d["turns"]) for k in keys]
    res["reasoning"] = sum(t["reasoning_tokens"] for t in d["turns"])
    res["children"] = n_children
    finish(res, messages, calls, results)
    # `linked` is krowk's own verdict; it must agree with the id match above.
    res["linked_flag_disagrees"] = res.get("unlinked_flag", 0) != res["unlinked"]
    return res


# ---------------------------------------------------------------- picking

def pick_sessions(store, provider, recency, live_after_ms, live_fids, forced_children=None):
    """Five distinct sessions: largest, smallest, children, error/interrupt, recent."""
    rows = store.execute(
        """SELECT s.id, s.title, b.foreign_session_id,
                  (SELECT count(*) FROM message m WHERE m.session_id = s.id),
                  (SELECT count(*) FROM part p WHERE p.session_id = s.id),
                  (SELECT count(*) FROM session c WHERE c.parent_id = s.id),
                  (SELECT count(*) FROM message m WHERE m.session_id = s.id AND m.role = 'error'),
                  (SELECT count(*) FROM part p JOIN message m ON m.id = p.message_id WHERE p.session_id = s.id
                     AND m.role = 'user' AND p.type = 'text' AND json_extract(p.data, '$.text') LIKE '[Request interrupted%'),
                  (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_valid(m.raw_json)
                     AND json_type(m.raw_json, '$.error') IS NOT NULL)
             FROM session s JOIN session_binding b ON b.session_id = s.id
            WHERE b.harness = ?""", (provider,)).fetchall()
    sess = [dict(zip(("id", "title", "fid", "msgs", "parts", "children", "errors", "interrupts", "raw_errors"), r)) for r in rows]
    for s in sess:
        s["recent"] = recency.get(s["fid"])
    live = {s["id"] for s in sess if s["fid"] in live_fids or (s["recent"] is not None and s["recent"] > live_after_ms)}
    picks, notes, used = [], [], set()

    def take(reason, cands, why_none=None):
        for s in cands:
            if s["id"] not in used and s["id"] not in live:
                used.add(s["id"])
                picks.append((reason, s))
                return True
        if why_none:
            notes.append(why_none)
        return False

    nonempty = [s for s in sess if s["msgs"] > 0]
    take("largest", sorted(nonempty, key=lambda s: (-s["parts"], -s["msgs"])))
    take("smallest", sorted(nonempty, key=lambda s: (s["parts"], s["msgs"])))
    kids = sorted([s for s in sess if s["children"] > 0], key=lambda s: -s["children"])
    if not take("children", kids):
        by_fid = {s["fid"]: s for s in sess}
        src = [by_fid[f] for f in (forced_children or []) if f in by_fid]
        if src and take("children (source)", src):
            notes.append(f"{provider}: the store links no child session to a parent, so the children pick came from the SOURCE's parent ids")
        else:
            take("fallback: 2nd largest", sorted(nonempty, key=lambda s: (-s["parts"], -s["msgs"])),
                 None)
            notes.append(f"{provider}: no session has children in store or source; picked the next largest instead")
    errs = sorted([s for s in sess if s["errors"] or s["interrupts"]], key=lambda s: -(s["errors"] + s["interrupts"]))
    if not take("error/interrupt", errs):
        raw = sorted([s for s in sess if s["raw_errors"]], key=lambda s: -s["raw_errors"])
        if raw and take("error (raw $.error)", raw):
            notes.append(f"{provider}: no role='error' message or '[Request interrupted' text; picked a session whose raw message JSON carries an error field")
        else:
            take("fallback: median size", sorted(nonempty, key=lambda s: s["parts"])[len(nonempty) // 2:], None)
            notes.append(f"{provider}: no error or interrupted turn anywhere; picked a median-size session instead")
    rec = sorted([s for s in sess if s["recent"] is not None], key=lambda s: -s["recent"])
    take("recent", rec, f"{provider}: no session left for the recent pick")
    if live:
        notes.append(f"{provider}: {len(live)} session(s) skipped as live (source modified after the import started)")
    return picks, notes


# ---------------------------------------------------------------- reporting

def short(s, n=28):
    s = " ".join((s or "").split())
    return s if len(s) <= n else s[: n - 1] + "…"


def yn(b):
    return "yes" if b else "**NO**"


def parts_str(c):
    return " ".join(f"{k}:{v}" for k, v in sorted(c.items())) or "-"


def tok_str(t):
    return "/".join(str(x) for x in t[:4])


def relink_pct(r):
    n = r["tool_results"]
    return "n/a" if n == 0 else f"{100 * (n - r['unlinked']) / n:.1f}%"


def provider_relink(store):
    out = {}
    for prov in PROVIDERS:
        total, unlinked = store.execute(
            """SELECT count(*), coalesce(sum(CASE WHEN coalesce(p.tool_call_id, '') = '' OR NOT EXISTS
                        (SELECT 1 FROM part c WHERE c.session_id = p.session_id AND c.type = 'tool_call' AND c.tool_call_id = p.tool_call_id)
                      THEN 1 ELSE 0 END), 0)
                 FROM part p JOIN session_binding b ON b.session_id = p.session_id
                WHERE p.type = 'tool_result' AND b.harness = ?""", (prov,)).fetchone()
        out[prov] = (total, unlinked)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--krowk", required=True, help="krowk binary built with --features sessions")
    ap.add_argument("--data", required=True, help="scratch XDG_DATA_HOME; the store lands in <data>/krowk/krowk.db")
    ap.add_argument("--rebuild", action="store_true", help="rebuild the scratch store from source, hashing sources before and after")
    args = ap.parse_args()

    binary = str(Path(args.krowk).resolve())
    data = Path(args.data).resolve()
    real = [HOME / ".local/share", Path(os.environ.get("XDG_DATA_HOME", HOME / ".local/share")).resolve()]
    if data in real or not str(data).startswith(("/tmp/", "/var/tmp/")):
        die(f"--data {data} is not a scratch dir under /tmp; refusing to risk the real store")
    data.mkdir(parents=True, exist_ok=True)
    cache = data.parent / (data.name + "-cache")
    marker = cache / ".phase1-scratch"
    if cache.exists():
        # Only a directory this script made is removed.
        if not marker.exists():
            die(f"{cache} exists and was not made by this script; refusing to remove it")
        shutil.rmtree(cache)
    cache.mkdir(parents=True)
    marker.touch()
    if (HOME / ".cache/krowk").is_dir():
        shutil.copytree(HOME / ".cache/krowk", cache / "krowk")
    k = Krowk(binary, data, cache)
    db_path = data / "krowk" / "krowk.db"
    snap_path = data / "phase1_snapshot.json"

    if args.rebuild:
        print("hashing sources (before)…", file=sys.stderr)
        t_start_ns = time.time_ns()
        before = snapshot()
        print("krowk sessions rebuild --yes …", file=sys.stderr)
        rb, rb_ms = k.json("sessions", "rebuild", "--yes")
        print("hashing sources (after)…", file=sys.stderr)
        after = snapshot()
        cmp = compare_snapshots(before, after)
        sizes = {f: a["size"] for f, a in after.items() if before.get(f) == a}
        snap_path.write_text(json.dumps({"t_start_ms": t_start_ns // 1_000_000, "rebuild_ms": rb_ms,
                                         "rebuild": rb.get("data"), "compare": cmp, "sizes": sizes}, indent=1))
    if not db_path.exists():
        die(f"no store at {db_path}; run with --rebuild")
    snap = json.loads(snap_path.read_text()) if snap_path.exists() else None

    store = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    # Anything modified after the import began may be ahead of the store.
    import_start_ms = snap["t_start_ms"] if snap else store.execute("SELECT min(time_created) FROM session").fetchone()[0]
    changed = set()
    if snap:
        # JSONL is parsed only up to the bytes the import saw, so a live
        # append after it cannot fake a mismatch; a file that changed while
        # the import ran is unknowable and never picked.
        SIZE_PIN.update({f: n for f, n in snap["sizes"].items() if f.endswith(".jsonl")})
        changed = set(snap["compare"]["appended"] + snap["compare"]["rewritten"])
    inf = float("inf")
    live_after = {"claude": inf if snap else import_start_ms, "cursor": inf if snap else import_start_ms, "opencode": import_start_ms}

    print("indexing sources…", file=sys.stderr)
    c_idx, c_children = claude_index()
    def mtime_ms(f):
        try:
            return os.stat(f).st_mtime_ns // 1_000_000
        except OSError:
            return 0

    recency = {"claude": {b: max(mtime_ms(f) for f in fs) for b, fs in c_idx.items()}}
    cursor_files = {p.parent.name: str(p) for p in CURSOR_ROOT.glob("*/agent-transcripts/*/*.jsonl") if p.parent.name == p.stem}
    recency["cursor"] = {i: mtime_ms(f) for i, f in cursor_files.items()}
    oc = oc_connect() if OPENCODE_DB.exists() else None
    recency["opencode"] = dict(oc.execute("SELECT id, time_updated FROM session")) if oc else {}
    oc_parents = [r[0] for r in oc.execute("SELECT parent_id, count(*) n FROM session WHERE parent_id IS NOT NULL GROUP BY 1 ORDER BY n DESC")] if oc else []
    c_parents = sorted(c_children, key=lambda p: -len(c_children[p]))
    live_fids = {"claude": {b for b, fs in c_idx.items() if changed & set(fs)},
                 "cursor": {i for i, f in cursor_files.items() if f in changed},
                 "opencode": set()}

    rows, details, notes, mismatches = [], [], [], []
    for prov in PROVIDERS:
        forced = {"claude": c_parents, "opencode": oc_parents}.get(prov)
        picks, pnotes = pick_sessions(store, prov, recency[prov], live_after[prov], live_fids[prov], forced)
        notes += pnotes
        if len(picks) < 5:
            notes.append(f"{prov}: only {len(picks)} distinct session(s) available for 5 picks")
        for reason, s in picks:
            fid = s["fid"]
            if prov == "claude":
                files = c_idx.get(fid)
                src = claude_source(files, len(c_children.get(fid, ()))) if files else None
            elif prov == "cursor":
                src = cursor_source(cursor_files[fid]) if fid in cursor_files else None
            else:
                src = opencode_source(oc, fid) if oc else None
            show, _ = k.json("sessions", "show", s["id"])
            st = store_counts(show, s["children"])
            if src is None:
                mismatches.append(f"- {prov} `{fid[:12]}` ({reason}): no source transcript found for foreign id {fid}")
                continue
            cols = {
                "messages": src["messages"] == st["messages"],
                "turns": src["turns"] == st["turns"],
                "parts": src["parts"] == st["parts"],
                "tokens": src["tok_uniq"] == st["tok_uniq"],
                "first": src["first_user"] == st["first_user"],
                "last": src["last_user"] == st["last_user"],
                "children": src["children"] == st["children"],
                "relink": src["result_links"] == st["result_links"] and src["call_ids"] == st["call_ids"] and not st["linked_flag_disagrees"],
            }
            label = f"`{fid[:12]}` {short(show['data'].get('title'))}"
            reason += {"children": f" ({s['children']})", "error/interrupt": f" ({s['errors']} err, {s['interrupts']} int)",
                       "error (raw $.error)": f" ({s['raw_errors']})"}.get(reason, "")
            rows.append("| " + " | ".join([
                prov, label, reason,
                f"{src['messages']} / {st['messages']}",
                f"{src['turns']} / {st['turns']}",
                f"{sum(src['parts'].values())} / {sum(st['parts'].values())}",
                f"{src['tok_uniq'][4]} / {st['tok_uniq'][4]}",
                f"{src['children']} / {st['children']}",
                f"{relink_pct(src)} / {relink_pct(st)}",
                " ".join(f"{c}:{yn(v)}" for c, v in cols.items()),
            ]) + " |")
            bad = [c for c, v in cols.items() if not v]
            d = [f"- **{prov} `{fid[:12]}`** ({reason}; store id `{s['id']}`, foreign id `{fid}`)",
                 f"  - parts by type: source `{parts_str(src['parts'])}` · store `{parts_str(st['parts'])}`",
                 f"  - tokens in/out/cache_read/cache_write: source (one per message.id) `{tok_str(src['tok_uniq'])}` · store `{tok_str(st['tok_uniq'])}`"
                 + (f" · per-line mirror `{tok_str(src['tok_mirror'])}`" if prov == "claude" else "")
                 + f" · reasoning source {src.get('reasoning', 0)} / store {st['reasoning']} · total source {src['tok_uniq'][4]} / store {st['tok_uniq'][4]}",
                 f"  - first user: {short(src['first_user'], 60)!r} · last user: {short(src['last_user'], 60)!r}"]
            if src.get("reasoning", 0) != st["reasoning"]:
                bad.append("reasoning")
            if src["first_user"] != st["first_user"]:
                d.append(f"    - store first user: {short(st['first_user'], 60)!r}")
            if src["last_user"] != st["last_user"]:
                d.append(f"    - store last user: {short(st['last_user'], 60)!r}")
            d += [f"  - note: {n}" for n in src["notes"]]
            if prov != "opencode" and snap and recency[prov].get(fid, 0) > import_start_ms:
                d.append("  - note: source appended since the import; parsed only up to the bytes the import saw")
            details.append("\n".join(d))
            if bad:
                extra = ""
                if "tokens" in bad and prov == "claude" and src["tok_mirror"][:4] == st["tok_uniq"][:4]:
                    extra = " — store equals the per-line sum: repeated per-block usage counted once per line"
                mismatches.append(f"- {prov} `{fid[:12]}` ({reason}): {', '.join(bad)}{extra}")

    # Completeness: every source session the import saw is in the store — a
    # session dropped whole would never be picked above.
    bound = {prov: {r[0] for r in store.execute("SELECT foreign_session_id FROM session_binding WHERE harness = ?", (prov,))} for prov in PROVIDERS}
    seen_oc = set()
    if oc:
        seen_oc = {r[0] for r in oc.execute("SELECT id FROM session WHERE time_created <= ?", (import_start_ms,))}
    source_ids = {"claude": set(c_idx), "cursor": set(cursor_files) - live_fids["cursor"], "opencode": seen_oc}
    complete = {prov: (len(ids), sorted(ids - bound[prov])) for prov, ids in source_ids.items()}
    # And the other way: a store session no source stands behind.
    all_source = {"claude": set(claude_index_all()), "cursor": set(cursor_files), "opencode": set(recency["opencode"])}
    orphans = {prov: sorted(bound[prov] - all_source[prov]) for prov in PROVIDERS}

    # Summary figures.
    times = [k.run("sessions", "--json")[1] for _ in range(5)]
    doctor, _ = k.json("doctor", check=False)
    store_check = (doctor.get("data") or doctor).get("store") or {}
    mode = oct(os.stat(db_path).st_mode & 0o777)
    relink = provider_relink(store)

    print("## Phase 1 exit spot-check\n")
    print("Source counts are derived from the transcripts/opencode.db by this script; store counts from "
          "`krowk sessions show <id> --json`. Cells are `source / store`. "
          "the source side counts each Claude `message.id` once at its largest output, thinking split into reasoning; "
          "the tokens cell is the total (Claude: incl. reasoning; opencode: excl., as each importer defines it). Re-link % = tool_result parts "
          "whose tool_call_id matches a tool_call in the same session.\n")
    print("| provider | session | pick | messages | turns | parts | tokens | children | re-link % | match |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    print("\n".join(rows))
    print("\n### Per-session detail\n")
    print("\n".join(details))
    print("\n### Summary\n")
    print(f"- `krowk sessions --json` median of 5: **{statistics.median(times):.0f} ms** (runs: {', '.join(f'{t:.0f}' for t in times)})")
    print(f"- `krowk doctor --json` store check: **{store_check.get('status', 'missing')}** — {store_check.get('message', '')}")
    print(f"- krowk.db mode: **{mode}** ({'ok' if mode == '0o600' else 'NOT 0600'})")
    for prov, (total, unlinked) in relink.items():
        pct = "n/a" if total == 0 else f"{100 * (total - unlinked) / total:.2f}%"
        print(f"- {prov} re-link (store-wide): {total - unlinked}/{total} tool_result parts matched = **{pct}** ({unlinked} orphaned)")
    if snap:
        c = snap["compare"]
        ok = not c["rewritten"] and not c["gone"]
        print(f"- source files unchanged by the import: **{'yes' if ok else 'NO'}** — {c['files']} files hashed; "
              f"{c['unchanged']} identical, {len(c['appended'])} appended by live writers (old bytes intact), "
              f"{len(c['rewritten'])} rewritten, {len(c['gone'])} gone, {len(c['new'])} new")
        for f in c.get("live_db", []):
            print(f"  - changed by opencode itself while it ran (krowk opens it read-only): {f}")
        for f in c["appended"] + c["rewritten"]:
            print(f"  - {'appended' if f in c['appended'] else 'REWRITTEN'}: {f}")
        print(f"- rebuild: {snap['rebuild_ms']:.0f} ms")
    else:
        print("- source files unchanged: not checked (run with --rebuild)")
    for prov, (n, missing) in complete.items():
        print(f"- {prov} completeness: {n - len(missing)}/{n} source sessions in the store"
              + (f" — **missing** {', '.join(m[:12] for m in missing[:10])}" if missing else "")
              + (f"; **{len(orphans[prov])} store session(s) with no source**: {', '.join(o[:12] for o in orphans[prov][:10])}" if orphans[prov] else "; every store session has a source"))
    if notes:
        print("\n### Pick notes\n")
        print("\n".join(f"- {n}" for n in notes))
    print("\n### Mismatches\n")
    print("\n".join(mismatches) if mismatches else "none")

    failed = bool(mismatches) or store_check.get("status") != "pass" or mode != "0o600" \
        or any(missing for _, missing in complete.values()) or any(orphans.values()) \
        or (snap and (snap["compare"]["rewritten"] or snap["compare"]["gone"]))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

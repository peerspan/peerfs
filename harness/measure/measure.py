#!/usr/bin/env python3
"""Replay Claude Code session transcripts into peerfs spaces, one commit per tool call.

Reads transcripts (JSONL) read-only, reconstructs the file states each edit-tool call
produced, imports every changed state into a fresh peerfs space, and records per call the
changed files, logical changed bytes, zstd sizes, and the new objects and stored bytes
the space gained. File contents and full paths stay in the work directory; the CSV and
JSON outputs carry only counts, sizes, tool names and path classes.

Usage: measure.py --peerfs BIN --work DIR --out DIR TRANSCRIPT...
"""

import argparse
import collections
import csv
import difflib
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys

EDIT_TOOLS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}
DERIVED = {"target", "node_modules", ".venv", "venv", "dist", "build", "__pycache__"}
MANIFEST = re.compile(r"^(Cargo\.(toml|lock)|package(-lock)?\.json|pnpm-lock\.yaml|yarn\.lock"
                      r"|pyproject\.toml|requirements.*\.txt|uv\.lock|poetry\.lock|go\.(mod|sum))$")
SECRET = re.compile(r"^(\.env.*|.*\.pem|.*\.key|id_rsa.*|id_ed25519.*|credentials.*)$", re.I)


def path_class(p):
    """source, derived, secret, or other (agent scratch and agent metadata)."""
    parts = p.split("/")
    if any(x in DERIVED for x in parts[:-1]):
        return "derived"
    if SECRET.match(parts[-1]):
        return "secret"
    if p.startswith(("/tmp/", "/var/tmp/")):
        return "other"
    if "/.claude/" in p and "/.claude/worktrees/" not in p:
        return "other"
    if "/.codex/" in p:
        return "other"
    return "source"


BASH_CLASSES = [
    ("install", re.compile(
        r"\b(pip3?\s+install|uv\s+(pip|add|sync)|poetry\s+(add|install)|npm\s+(i|install|ci|add)\b"
        r"|pnpm\s+(i|install|add)\b|yarn\s+add|cargo\s+(add|install|fetch)|apt(-get)?\s+install"
        r"|go\s+get|brew\s+install|conda\s+install)")),
    ("build_test", re.compile(
        r"\b(cargo\s+(build|test|run|check|clippy|bench|fmt|nextest)|npm\s+(run|test)|pnpm\s+(run|test)"
        r"|yarn\s+(build|test|run)|make\b|pytest|go\s+(build|test)|tsc\b|cmake|ninja|rustfmt|ruff|black)")),
    ("vcs_write", re.compile(
        r"\bgit\s+(-C\s+\S+\s+)?(commit|checkout|switch|merge|rebase|reset|apply|am|cherry-pick|stash"
        r"|pull|clone|worktree\s+add|add|rm|mv|restore)\b")),
    ("file_write", re.compile(
        r"(<<-?\s*['\"]?\w+|(^|[^0-9&>2])>>?\s*(?!/dev/null|&)[^\s&|;]|\btee\b|\bsed\s+(-\w+\s+)*-i"
        r"|\bperl\s+-pi|\b(mv|cp|rm|mkdir|touch|chmod|ln|rsync|unzip|wget)\b|\btar\s+-?x|curl\b.*\s-o\s)")),
    ("remote", re.compile(r"\b(ssh|scp)\b")),
]


def bash_class(cmd):
    for name, rx in BASH_CLASSES:
        if rx.search(cmd):
            return name
    return "inspect"


def enc(s):
    return s.encode("utf-8", "surrogatepass") if isinstance(s, str) else s


def apply_hunks(base, hunks):
    """Apply a structuredPatch (list of hunks with oldStart and +/-/space lines) to text."""
    lines = base.decode("utf-8", "surrogatepass").split("\n")
    out, i = [], 0
    for h in hunks:
        start = h["oldStart"] - 1
        out.extend(lines[i:start])
        for ln in h["lines"]:
            if ln.startswith("+"):
                out.append(ln[1:])
            elif ln.startswith(" "):
                out.append(ln[1:])
            elif ln.startswith("\\"):
                continue
        i = start + h["oldLines"]
    out.extend(lines[i:])
    return enc("\n".join(out))


def hunk_base(hunks):
    """The part of the base a patch shows: its context and removed lines."""
    return enc("\n".join(ln[1:] for h in hunks for ln in h["lines"] if ln[:1] in (" ", "-")))


def load_calls(path):
    uses, order, results = {}, [], {}
    session_cwd = None
    for line in open(path, encoding="utf-8"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if session_cwd is None and d.get("cwd"):
            session_cwd = d["cwd"]
        m = d.get("message")
        if not isinstance(m, dict) or not isinstance(m.get("content"), list):
            continue
        blocks = [b for b in m["content"] if isinstance(b, dict)]
        n_results = sum(1 for b in blocks if b.get("type") == "tool_result")
        for b in blocks:
            if b.get("type") == "tool_use" and b.get("id") not in uses:
                uses[b["id"]] = dict(name=b.get("name"), input=b.get("input") or {},
                                     ts=d.get("timestamp"))
                order.append(b["id"])
            elif b.get("type") == "tool_result":
                tur = d.get("toolUseResult") if n_results == 1 else None
                results[b.get("tool_use_id")] = (tur, bool(b.get("is_error")))
    calls = []
    for i in order:
        c = uses[i]
        c["result"], c["error"] = results.get(i, (None, True))
        calls.append(c)
    return calls, session_cwd or "/"


def reconstruct(calls):
    """Walk the calls in order. Returns events and the first-known base of each path.

    An event is a tool call that changed files ("call") or a change seen between calls
    that no edit tool made ("oob": a later originalFile or full Read disagreed with the
    tracked state). Each change is (path, before, after, provenance)."""
    state, read_full, first_base = {}, {}, {}
    events, stats = [], collections.Counter()
    seen = collections.defaultdict(dict)  # path -> {digest: content} of every state reconstructed

    def note_base(p, base, prov):
        if p not in first_base:
            first_base[p] = (base, prov)

    def observe(p, content, idx):
        if p in state and state[p] != content:
            events.append(dict(kind="oob", idx=idx, tool="(between calls)",
                               changes=[(p, state[p], content, "exact")]))
            stats["oob"] += 1
        if p not in state:
            note_base(p, content, "exact")
        state[p] = content
        seen[p][hashlib.sha256(content).hexdigest()] = content

    for idx, c in enumerate(calls):
        name, r, inp = c["name"], c["result"], c["input"]
        if name == "Read" and isinstance(r, dict) and isinstance(r.get("file"), dict) \
                and r.get("type") == "text":
            f = r["file"]
            if f.get("startLine", 1) == 1 and f.get("numLines") == f.get("totalLines") \
                    and isinstance(f.get("content"), str):
                body = enc(f["content"])
                if f["filePath"] in state:
                    observe(f["filePath"], body, idx)
                else:
                    read_full[f["filePath"]] = body
            continue
        if name not in EDIT_TOOLS:
            continue
        stats["edit_calls"] += 1
        if c["error"] or not isinstance(r, dict) or "filePath" not in r:
            stats["edit_failed"] += 1
            continue
        p = r["filePath"]
        orig = r.get("originalFile")
        if orig is not None:
            base, prov = enc(orig), "exact"
            if p in state:
                stats["chain_checked"] += 1
                stats["chain_held" if state[p] == base else "chain_broken"] += 1
                if state[p] != base:
                    observe(p, base, idx)
        elif p in state:
            base, prov = state[p], "chained"
        elif name == "Write" and r.get("type") == "create":
            base, prov = None, "exact"
        elif p in read_full:
            base, prov = read_full[p], "read"
        else:
            base, prov = estimate_base(p, r, name), "estimated"
        note_base(p, base, prov)
        if name == "Write":
            after = enc(r.get("content", inp.get("content", "")))
        elif name == "Edit":
            after = apply_edit(base, r)
            if after is None:
                after = apply_hunks(base or b"", r.get("structuredPatch") or [])
                prov = "estimated"
        else:
            stats["unsupported"] += 1
            continue
        stats["prov_" + prov] += 1
        state[p] = after
        for v in (base, after):
            if v is not None:
                seen[p][hashlib.sha256(v).hexdigest()] = v
        if after != base:
            events.append(dict(kind="call", idx=idx, tool=name, changes=[(p, base, after, prov)]))
        else:
            stats["edit_nochange"] += 1
    return events, first_base, stats, state, seen


def verify_file_history(transcript, cwd, seen, root=os.path.expanduser("~/.claude/file-history")):
    """Check Claude Code's checkpoint backups against the reconstructed states.

    Each backup is a file's content at a checkpoint; a match means some state the replay
    reconstructed for that path is byte-identical to it."""
    sid = os.path.basename(transcript)[:-len(".jsonl")]
    d = os.path.join(root, sid)
    out, diffs = collections.Counter(), []
    if not os.path.isdir(d):
        return dict(out)
    pairs = set()
    for line in open(transcript, encoding="utf-8"):
        if '"trackedFileBackups"' not in line:
            continue
        try:
            snap = json.loads(line).get("snapshot") or {}
        except ValueError:
            continue
        for p, v in (snap.get("trackedFileBackups") or {}).items():
            if v.get("backupFileName"):
                pairs.add((os.path.join(cwd, p), v["backupFileName"]))
    for p, name in pairs:
        f = os.path.join(d, name)
        if not os.path.exists(f):
            out["backup_missing"] += 1
        elif p not in seen:
            out["path_not_edited"] += 1
        else:
            b = open(f, "rb").read()
            if hashlib.sha256(b).hexdigest() in seen[p]:
                out["match"] += 1
            else:
                out["mismatch"] += 1
                diffs.append(min(changed_bytes(v, b) for v in seen[p].values()))
    out = dict(out)
    out["mismatch_bytes"] = sorted(diffs)
    return out


def apply_edit(base, r):
    old, new = r.get("oldString"), r.get("newString")
    if base is None or old is None or new is None:
        return None
    s = base.decode("utf-8", "surrogatepass")
    if old == "" or old not in s:
        return None
    return enc(s.replace(old, new) if r.get("replaceAll") else s.replace(old, new, 1))


def estimate_base(p, r, name):
    """A base for an edit whose prior content the transcript does not hold."""
    hunks = r.get("structuredPatch") or []
    try:
        disk = open(p, "rb").read()
    except OSError:
        disk = None
    if name == "Edit" and disk is not None:
        old, new = enc(r.get("oldString") or ""), enc(r.get("newString") or "")
        if old and old in disk:
            return disk
        if new and new in disk:
            return disk.replace(new, old, 1)
    if disk is not None and name == "Write":
        return disk
    return hunk_base(hunks)


def changed_bytes(before, after):
    """Bytes in removed plus added lines of a line diff."""
    if before is None:
        return len(after)
    a, b = before.splitlines(keepends=True), after.splitlines(keepends=True)
    n = 0
    for op, i1, i2, j1, j2 in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
        if op != "equal":
            n += sum(map(len, a[i1:i2])) + sum(map(len, b[j1:j2]))
    return n


def zstd_sizes(before, after, tmp):
    fa, fb = os.path.join(tmp, "after"), os.path.join(tmp, "before")
    open(fa, "wb").write(after)
    alone = len(subprocess.run(["zstd", "-3", "-q", "-c", fa], capture_output=True,
                               check=True).stdout)
    if before is None:
        return alone, alone
    open(fb, "wb").write(before)
    delta = len(subprocess.run(["zstd", "-3", "-q", "-c", "--patch-from=" + fb, fa],
                               capture_output=True, check=True).stdout)
    return alone, delta


class Space:
    def __init__(self, peerfs, path):
        self.peerfs, self.path = peerfs, path
        self.env = dict(os.environ, PEERFS_AGENT="measure", PEERFS_SESSION="replay")
        if len(os.path.join(path, "peerfs.sock")) > 107:
            sys.exit("work directory too deep for a Unix socket path: " + path)
        self.run("init")

    def run(self, *args):
        p = subprocess.run([self.peerfs, "--space", self.path, *args], env=self.env,
                           capture_output=True)
        if p.returncode:
            sys.exit("peerfs %s failed: %s" % (args[0], p.stderr.decode(errors="replace")))

    def import_(self, tree, msg):
        self.run("import", tree, "--branch", "main", "-m", msg)
        return self.objects(), self.stored()

    def objects(self):
        out = subprocess.run(["curl", "-s", "--unix-socket", os.path.join(self.path, "peerfs.sock"),
                              "http://x/v1/space"], capture_output=True, check=True).stdout
        return json.loads(out)["objects"]

    def stored(self):
        d = os.path.join(self.path, "objects")
        return sum(os.path.getsize(os.path.join(d, f)) for f in os.listdir(d))

    def stop(self):
        pid = os.path.join(self.path, "serve.pid")
        if os.path.exists(pid):
            try:
                os.kill(int(open(pid).read().strip()), 15)
            except (OSError, ValueError):
                pass


def real_tree_bytes(paths):
    """Estimated encoded size of the trees a commit rewrites in the real workspace.

    The workspace root is the nearest ancestor holding .git (else the file's directory);
    each tree on the path from it is sized from today's directory listing at about
    name + 38 bytes per entry (lengths, tags and a 33-byte identity). Names only are read."""
    dirs = set()
    for p in paths:
        d = os.path.dirname(p)
        chain = [d]
        while d != "/" and not os.path.exists(os.path.join(d, ".git")):
            d = os.path.dirname(d)
            chain.append(d)
        if d == "/":
            chain = chain[:1]
        dirs |= set(chain)
    total = 0
    for d in dirs:
        try:
            names = [n for n in os.listdir(d) if n != ".git"]
        except OSError:
            names = ["x" * 12]
        total += 4 + sum(len(n.encode()) + 38 for n in names)
    return total


def tree_path(p, cwd):
    if p.startswith(cwd.rstrip("/") + "/"):
        return os.path.relpath(p, cwd)
    return "ext" + p


def ancestors(rel):
    parts = rel.split("/")[:-1]
    return {"/".join(parts[:i]) for i in range(len(parts) + 1)}


def write_tree(root, rel, content):
    full = os.path.join(root, rel)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "wb") as f:
        f.write(content)


def measure(transcript, peerfs, work, sid):
    calls, cwd = load_calls(transcript)
    events, first_base, stats, final, seen = reconstruct(calls)
    names = collections.Counter(c["name"] for c in calls)
    bash = collections.Counter(bash_class(str(c["input"].get("command", "")))
                               for c in calls if c["name"] == "Bash")
    sdir = os.path.join(work, sid)
    shutil.rmtree(sdir, ignore_errors=True)
    tree, tmp = os.path.join(sdir, "tree"), os.path.join(sdir, "tmp")
    os.makedirs(tree)
    os.makedirs(tmp)
    rows = []
    summary = dict(session=sid, tool_calls=len(calls), tools=dict(names), bash_classes=dict(bash),
                   recon=dict(stats), files_touched=len(first_base),
                   seed_files=sum(1 for b, _ in first_base.values() if b is not None),
                   seed_bytes=sum(len(b) for b, _ in first_base.values() if b is not None),
                   base_prov=dict(collections.Counter(pv for _, pv in first_base.values())),
                   final_bytes=sum(len(v) for v in final.values()),
                   final_class_bytes=dict(), active_hours=active_hours(calls),
                   file_history=verify_file_history(transcript, cwd, seen))
    fcb = collections.Counter()
    for p, v in final.items():
        fcb[path_class(p)] += len(v)
    summary["final_class_bytes"] = dict(fcb)
    if not events:
        return summary, rows
    space = Space(peerfs, os.path.join(sdir, "sp"))
    try:
        for p, (b, _) in first_base.items():
            if b is not None:
                write_tree(tree, tree_path(p, cwd), b)
        if not os.listdir(tree):
            write_tree(tree, ".seed", b"")
        o0, s0 = space.import_(tree, "seed")
        summary["seed_objects"], summary["seed_stored"] = o0, s0
        for n, ev in enumerate(events):
            dirs, row = set(), collections.Counter()
            sizes = []
            for p, before, after, prov in ev["changes"]:
                rel = tree_path(p, cwd)
                write_tree(tree, rel, after)
                dirs |= ancestors(rel)
                alone, delta = zstd_sizes(before, after, tmp)
                cls = path_class(p)
                sizes.append(len(after))
                row["changed_bytes"] += changed_bytes(before, after)
                row["file_bytes"] += len(after)
                row["zstd_alone"] += alone
                row["zstd_delta"] += delta
                row["bytes_" + cls] += changed_bytes(before, after)
                row["prov_" + prov] += 1
                row["new_file"] += before is None
                row["manifest"] += bool(MANIFEST.match(os.path.basename(p)))
                row["depth"] = max(row["depth"], rel.count("/"))
            o1, s1 = space.import_(tree, "%s %d" % (ev["tool"], n))
            meta = len(dirs) + 1  # every ancestor tree is rewritten, plus the commit
            row["tree_bytes_est"] = real_tree_bytes([p for p, *_ in ev["changes"]])
            r = dict(session=sid, n=n, kind=ev["kind"], tool=ev["tool"],
                     files=len(ev["changes"]), max_file=max(sizes),
                     cls="+".join(sorted({path_class(p) for p, *_ in ev["changes"]})),
                     new_objects=o1 - o0, meta_objects=meta,
                     content_objects=o1 - o0 - meta, new_stored=s1 - s0)
            r.update(row)
            r["provenance"] = "estimated" if row["prov_estimated"] else "measured"
            rows.append(r)
            o0, s0 = o1, s1
        summary["final_objects"], summary["final_stored"] = o0, s0
    finally:
        space.stop()
    return summary, rows


def active_hours(calls, cap=300):
    import datetime
    ts = []
    for c in calls:
        try:
            ts.append(datetime.datetime.fromisoformat(c["ts"].replace("Z", "+00:00")).timestamp())
        except (AttributeError, TypeError, ValueError):
            pass
    ts.sort()
    return sum(min(b - a, cap) for a, b in zip(ts, ts[1:])) / 3600


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--peerfs", required=True)
    ap.add_argument("--work", required=True, help="scratch for trees and spaces (holds contents)")
    ap.add_argument("--out", required=True, help="where calls.csv and sessions.json go")
    ap.add_argument("transcripts", nargs="+")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    sessions, all_rows = [], []
    for t in a.transcripts:
        sid = hashlib.blake2s(t.encode(), digest_size=4).hexdigest()
        kind = "subagent" if "/subagents/" in t else "main"
        s, rows = measure(t, a.peerfs, a.work, sid)
        s["kind"] = kind
        sessions.append(s)
        all_rows += rows
        print(sid, kind, s["tool_calls"], "calls,", sum(r["kind"] == "call" for r in rows),
              "commits", file=sys.stderr)
    json.dump(sessions, open(os.path.join(a.out, "sessions.json"), "w"), indent=1)
    keys = sorted({k for r in all_rows for k in r})
    with open(os.path.join(a.out, "calls.csv"), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=keys)
        w.writeheader()
        for r in all_rows:
            w.writerow({k: r.get(k, 0) for k in keys})


if __name__ == "__main__":
    main()

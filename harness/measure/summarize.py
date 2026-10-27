#!/usr/bin/env python3
"""Turn measure.py's sessions.json and calls.csv into the Markdown tables of the results doc."""

import collections
import csv
import json
import math
import os
import sys


def pct(xs, q):
    xs = sorted(xs)
    if not xs:
        return 0
    return xs[min(len(xs) - 1, max(0, math.ceil(q * len(xs)) - 1))]


def fmt(x):
    if isinstance(x, float):
        return "%.2f" % x
    return "{:,}".format(x)


def kib(x):
    return "%.1f KiB" % (x / 1024) if x >= 1024 else "%d B" % x


def table(head, rows):
    print("| " + " | ".join(head) + " |")
    print("|" + "|".join(" --- " for _ in head) + "|")
    for r in rows:
        print("| " + " | ".join(str(c) for c in r) + " |")
    print()


def main(out):
    sessions = json.load(open(os.path.join(out, "sessions.json")))
    rows = list(csv.DictReader(open(os.path.join(out, "calls.csv"))))
    for r in rows:
        for k, v in r.items():
            if k not in ("session", "kind", "tool", "cls", "provenance"):
                r[k] = int(v)
    calls = [r for r in rows if r["kind"] == "call"]
    oob = [r for r in rows if r["kind"] == "oob"]
    by = collections.defaultdict(list)
    for r in calls:
        by[r["session"]].append(r)
    oob_by = collections.Counter(r["session"] for r in oob)

    print("### Per session\n")
    head = ["session", "kind", "tool calls", "Bash", "edit-tool calls", "commits",
            "commits / call", "out-of-band", "files", "changed bytes", "new objects",
            "new stored", "active h", "commits / active h", "bases"]
    out_rows = []
    tot = collections.Counter()
    fh_diffs = []
    for s in sorted(sessions, key=lambda s: -s["tool_calls"]):
        if s["tool_calls"] == 0:
            continue
        c = by.get(s["session"], [])
        bash = s["tools"].get("Bash", 0)
        stored = sum(r["new_stored"] for r in c)
        objs = sum(r["new_objects"] for r in c)
        cb = sum(r["changed_bytes"] for r in c)
        bp = s.get("base_prov", {})
        out_rows.append([s["session"], s["kind"], s["tool_calls"], bash,
                         s["recon"].get("edit_calls", 0), len(c),
                         "%.3f" % (len(c) / s["tool_calls"]), oob_by[s["session"]],
                         s["files_touched"], kib(cb), objs, kib(stored),
                         "%.2f" % s["active_hours"],
                         "%.1f" % (len(c) / s["active_hours"]) if s["active_hours"] > 0.05 else "-",
                         "/".join("%s %d" % (k, v) for k, v in sorted(bp.items()))])
        tot["calls"] += s["tool_calls"]
        tot["bash"] += bash
        tot["edit"] += s["recon"].get("edit_calls", 0)
        tot["failed"] += s["recon"].get("edit_failed", 0)
        tot["nochange"] += s["recon"].get("edit_nochange", 0)
        tot["commits"] += len(c)
        tot["files"] += s["files_touched"]
        tot["cb"] += cb
        tot["objs"] += objs
        tot["stored"] += stored
        tot["hours"] += s["active_hours"]
        tot["sessions"] += 1
        for k, v in s["recon"].items():
            if k.startswith(("prov_", "chain_")):
                tot[k] += v
        for k, v in s.get("file_history", {}).items():
            if k == "mismatch_bytes":
                fh_diffs.extend(v)
            else:
                tot["fh_" + k] += v
        for k, v in s["bash_classes"].items():
            tot["bash_" + k] += v
        for k, v in s["final_class_bytes"].items():
            tot["final_" + k] += v
    out_rows.append(["**all %d**" % tot["sessions"], "", tot["calls"], tot["bash"], tot["edit"],
                     tot["commits"], "%.3f" % (tot["commits"] / tot["calls"]), len(oob),
                     tot["files"], kib(tot["cb"]), tot["objs"], kib(tot["stored"]),
                     "%.2f" % tot["hours"], "%.1f" % (tot["commits"] / tot["hours"]), ""])
    table(head, out_rows)

    print("### Per commit-producing call (n = %d)\n" % len(calls))
    metrics = [
        ("files changed", "files", fmt),
        ("size of changed file", "max_file", kib),
        ("logical changed bytes", "changed_bytes", kib),
        ("whole-file zstd-3", "zstd_alone", kib),
        ("zstd-3 delta against previous version", "zstd_delta", kib),
        ("new objects", "new_objects", fmt),
        ("  of which content", "content_objects", fmt),
        ("  of which trees and commit", "meta_objects", fmt),
        ("new stored bytes", "new_stored", kib),
        ("trees rewritten in the real workspace (estimated)", "tree_bytes_est", kib),
    ]
    rr = []
    for label, k, f in metrics:
        xs = [r[k] for r in calls]
        rr.append([label, f(pct(xs, 0.5)), f(pct(xs, 0.9)), f(max(xs)),
                   f(sum(xs) / len(xs)) if f is fmt else kib(sum(xs) / len(xs)), kib(sum(xs))
                   if f is kib else fmt(sum(xs))])
    meta = [r["new_stored"] - r["zstd_alone"] for r in calls]
    rr.append(["stored minus whole-file zstd (trees, commit, framing)", kib(pct(meta, 0.5)),
               kib(pct(meta, 0.9)), kib(max(meta)), kib(sum(meta) / len(meta)), kib(sum(meta))])
    table(["metric", "median", "p90", "max", "mean", "total"], rr)

    print("### Changed-file sizes\n")
    buckets = [(0, 4096, "< 4 KiB"), (4096, 16384, "4–16 KiB"), (16384, 65536, "16–64 KiB"),
               (65536, 1 << 62, ">= 64 KiB (chunked)")]
    table(["size", "commits", "share"],
          [[lab, sum(lo <= r["max_file"] < hi for r in calls),
            "%.1f%%" % (100 * sum(lo <= r["max_file"] < hi for r in calls) / len(calls))]
           for lo, hi, lab in buckets])

    print("### By tool\n")
    tools = collections.defaultdict(list)
    for r in calls:
        tools[r["tool"]].append(r)
    table(["tool", "commits", "new files", "median changed", "median stored", "total stored"],
          [[t, len(v), sum(r["new_file"] for r in v), kib(pct([r["changed_bytes"] for r in v], .5)),
            kib(pct([r["new_stored"] for r in v], .5)), kib(sum(r["new_stored"] for r in v))]
           for t, v in sorted(tools.items())])

    print("### Path classes\n")
    cls = collections.Counter()
    ncls = collections.Counter()
    for r in calls:
        for k in ("source", "derived", "secret", "other"):
            cls[k] += r.get("bytes_" + k, 0)
        ncls[r["cls"]] += 1
    table(["class", "commits touching", "changed bytes", "bytes at session end"],
          [[k, ncls[k], kib(cls[k]), kib(tot["final_" + k])]
           for k in ("source", "derived", "secret", "other")])

    print("### Bash calls by heuristic class (estimated)\n")
    table(["class", "calls", "share of Bash"],
          [[k[5:], v, "%.1f%%" % (100 * v / tot["bash"])]
           for k, v in sorted(tot.items(), key=lambda kv: -kv[1]) if k.startswith("bash_")])

    print("### Reconstruction\n")
    table(["item", "count"], [[k, v] for k, v in [
        ("edit-tool calls", tot["edit"]), ("failed (error result)", tot["failed"]),
        ("succeeded, no byte change", tot["nochange"]),
        ("base exact (originalFile or create)", tot["prov_exact"]),
        ("base chained from earlier call", tot["prov_chained"]),
        ("base from an earlier full Read", tot["prov_read"]),
        ("base estimated", tot["prov_estimated"]),
        ("later originalFile checked against the tracked state", tot["chain_checked"]),
        ("  agreed", tot["chain_held"]),
        ("  disagreed (out-of-band change)", tot["chain_broken"]),
        ("out-of-band changes observed (originalFile or full Read)", len(oob)),
        ("file-history backups matching a reconstructed state", tot["fh_match"]),
        ("file-history backups not matching", tot["fh_mismatch"]),
        ("file-history backups of paths never edited by a tool", tot["fh_path_not_edited"]),
        ("file-history backups referenced but absent", tot["fh_backup_missing"])]])
    if oob:
        xs = [r["changed_bytes"] for r in oob]
        print("Out-of-band changes: median %s, p90 %s, max %s changed; %s stored in total.\n" % (
            kib(pct(xs, .5)), kib(pct(xs, .9)), kib(max(xs)), kib(sum(r["new_stored"] for r in oob))))
    if fh_diffs:
        print("File-history backups that match no reconstructed state differ from the nearest "
              "one by: median %s, p90 %s, max %s.\n" % (kib(pct(fh_diffs, .5)),
                                                        kib(pct(fh_diffs, .9)), kib(max(fh_diffs))))
    mut = sum(v for k, v in tot.items() if k in ("bash_file_write", "bash_build_test",
                                                  "bash_vcs_write", "bash_install"))
    print("Commits per tool call if every possibly-mutating Bash call (file_write, build_test, "
          "vcs_write, install) also changed files: (%d + %d) / %d = %.3f (estimated upper bound; "
          "remote calls change other hosts).\n" % (tot["commits"], mut, tot["calls"],
                                                    (tot["commits"] + mut) / tot["calls"]))
    print("Commits that touch a dependency manifest or lockfile: %d of %d.\n" % (
        sum(1 for r in calls if r.get("manifest")), len(calls)))
    est = [r for r in calls if r["provenance"] == "estimated"]
    print("Commits with an estimated base: %d of %d, %s of %s stored.\n" % (
        len(est), len(calls), kib(sum(r["new_stored"] for r in est)), kib(tot["stored"])))


if __name__ == "__main__":
    main(sys.argv[1])

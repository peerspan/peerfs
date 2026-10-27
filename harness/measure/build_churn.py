#!/usr/bin/env python3
"""What one Bash build call adds to a space: a controlled stand-in for the derived churn that
transcripts cannot show.

Exports this repository's HEAD into WORK/src, builds it with cargo (debug, the profile agents
usually run), and imports the whole tree, target/ included, as the seed commit. Then, for each
of two edits — one in the leaf crate (peerfs) and one in the root crate (peerfs-core) — commits
the source edit alone, runs `cargo build`, and commits again, recording new objects and stored
bytes for each commit and how many files under target/ changed.

Usage: build_churn.py --peerfs BIN --repo REPO --work WORK
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import measure  # noqa: E402

EDITS = [("leaf", "crates/peerfs/src/main.rs"), ("root", "crates/peerfs-core/src/lib.rs")]


def digest_tree(root):
    out = {}
    for d, _, files in os.walk(root):
        for f in files:
            p = os.path.join(d, f)
            if os.path.isfile(p) and not os.path.islink(p):
                out[os.path.relpath(p, root)] = (os.path.getsize(p),
                                                 hashlib.blake2b(open(p, "rb").read(),
                                                                 digest_size=16).digest())
    return out


def build(src):
    subprocess.run(["cargo", "build", "--offline", "-q", "-p", "peerfs"], cwd=src, check=True,
                   env=dict(os.environ, CARGO_TARGET_DIR=os.path.join(src, "target")))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--peerfs", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--work", required=True)
    a = ap.parse_args()
    shutil.rmtree(a.work, ignore_errors=True)
    src = os.path.join(a.work, "src")
    os.makedirs(src)
    archive = subprocess.run(["git", "-C", a.repo, "archive", "HEAD"], capture_output=True,
                             check=True).stdout
    subprocess.run(["tar", "-x", "-C", src], input=archive, check=True)
    build(src)
    space = measure.Space(a.peerfs, os.path.join(a.work, "sp"))
    results = []
    try:
        before = digest_tree(os.path.join(src, "target"))
        o0, s0 = space.import_(src, "seed")
        srcb = sum(v[0] for k, v in digest_tree(src).items() if not k.startswith("target/"))
        results.append(dict(step="seed", source_bytes=srcb,
                            target_bytes=sum(v[0] for v in before.values()),
                            target_files=len(before), objects=o0, stored=s0))
        for n, (label, rel) in enumerate(EDITS):
            with open(os.path.join(src, rel), "a") as f:
                f.write('\n#[allow(dead_code)]\nconst MEASURE_CHURN_%d: &str = "edit %d";\n'
                        % (n, n))
            o1, s1 = space.import_(src, "edit " + label)
            results.append(dict(step="edit " + label, objects=o1 - o0, stored=s1 - s0))
            build(src)
            after = digest_tree(os.path.join(src, "target"))
            changed = [k for k in after if before.get(k) != after[k]]
            o2, s2 = space.import_(src, "build after " + label)
            results.append(dict(step="build after " + label, objects=o2 - o1, stored=s2 - s1,
                                target_files_changed=len(changed),
                                target_bytes_changed=sum(after[k][0] for k in changed),
                                target_files_removed=len(set(before) - set(after))))
            o0, s0, before = o2, s2, after
    finally:
        space.stop()
    json.dump(results, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()

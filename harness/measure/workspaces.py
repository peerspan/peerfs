#!/usr/bin/env python3
"""Size the real workspaces the replayed sessions edited, by path class, as they are on disk now.

For every git work tree (a directory holding .git) that contains a file an edit tool changed,
counts source bytes (tracked plus untracked-but-not-ignored files, as `git ls-files -co
--exclude-standard` lists them), derived bytes (directories named target, node_modules, .venv,
venv, dist, build or __pycache__), other ignored bytes, and the .git directory. Nested work
trees (git worktrees, submodules) are sized on their own and not counted in their parent.
Prints a Markdown table labelled by an anonymous id and the project's marker files only.

Usage: workspaces.py TRANSCRIPT...
"""

import hashlib
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import measure  # noqa: E402

MARKERS = {"Cargo.toml": "rust", "package.json": "node", "pyproject.toml": "python",
           "requirements.txt": "python", "setup.py": "python", "go.mod": "go"}


def roots_of(transcripts):
    roots = set()
    for t in transcripts:
        calls, _ = measure.load_calls(t)
        _, first_base, _, _, _ = measure.reconstruct(calls)
        for p in first_base:
            d = os.path.dirname(p)
            while d != "/" and not os.path.exists(os.path.join(d, ".git")):
                d = os.path.dirname(d)
            if d != "/" and os.path.isdir(d):
                roots.add(d)
    return roots


def size(path):
    try:
        return os.lstat(path).st_size
    except OSError:
        return 0


def walk(root):
    """(total bytes outside .git and nested work trees, derived bytes, derived files, .git bytes)."""
    total = derived = dfiles = 0
    for d, dirs, files in os.walk(root):
        keep = []
        for x in dirs:
            full = os.path.join(d, x)
            if x == ".git":
                continue
            if os.path.exists(os.path.join(full, ".git")):
                continue  # a nested work tree, sized separately
            if x in measure.DERIVED:
                for dd, _, ff in os.walk(full):
                    for f in ff:
                        n = size(os.path.join(dd, f))
                        derived += n
                        total += n
                        dfiles += 1
                continue
            keep.append(x)
        dirs[:] = keep
        total += sum(size(os.path.join(d, f)) for f in files if f != ".git")
    git = 0
    if os.path.isdir(os.path.join(root, ".git")):
        for d, _, files in os.walk(os.path.join(root, ".git")):
            git += sum(size(os.path.join(d, f)) for f in files)
    return total, derived, dfiles, git


def source(root):
    out = subprocess.run(["git", "-C", root, "ls-files", "-z", "-co", "--exclude-standard"],
                         capture_output=True).stdout.split(b"\0")
    files = [os.path.join(root, f.decode(errors="surrogateescape")) for f in out if f]
    files = [f for f in files if os.path.isfile(f) and not os.path.islink(f)]
    return sum(size(f) for f in files), len(files)


def mib(x):
    return "%.1f MiB" % (x / 2**20)


def main():
    rows, tot = [], [0, 0, 0, 0]
    for root in sorted(roots_of(sys.argv[1:])):
        src, nsrc = source(root)
        total, der, dfiles, git = walk(root)
        other = max(0, total - src - der)
        kind = "+".join(sorted({v for k, v in MARKERS.items()
                                if os.path.exists(os.path.join(root, k))})) or "docs/other"
        wt = "worktree" if os.path.isfile(os.path.join(root, ".git")) else "repo"
        rows.append((hashlib.blake2s(root.encode(), digest_size=3).hexdigest(), kind, wt, nsrc,
                     src, der, dfiles, other, git))
        for i, v in enumerate((src, der, other, git)):
            tot[i] += v
    print("| workspace | kind | type | source files | source | derived | derived files "
          "| other ignored | .git | derived / source |")
    print("|" + "|".join(" --- " for _ in range(10)) + "|")
    for w, kind, wt, nsrc, src, der, dfiles, other, git in rows:
        print("| %s | %s | %s | %d | %s | %s | %d | %s | %s | %s |" % (
            w, kind, wt, nsrc, mib(src), mib(der), dfiles, mib(other), mib(git),
            "%.1f" % (der / src) if src else "-"))
    print("| **all %d** | | | | %s | %s | | %s | %s | %.1f |" % (
        len(rows), mib(tot[0]), mib(tot[1]), mib(tot[2]), mib(tot[3]),
        tot[1] / tot[0] if tot[0] else 0))


if __name__ == "__main__":
    main()

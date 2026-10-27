# Commits and bytes per tool call, from recorded agent sessions

2026-10-27. Issue #2. Inputs to decisions 1, 5 and 6 of `docs-internal/docs/peerfs.md`.

Every figure is labelled **measured** (counted or replayed from recorded data, exactly) or
**estimated** (a model, a heuristic, or today's disk standing in for the past). The harness is
`harness/measure/`; its README says how to rerun it. No agent session was started for this.

## Summary

- **Commit rate (measured, lower bound).** 283 of 2,651 tool calls (10.7%) changed a file
  through an edit tool; 9.6 such commits per active agent-hour across 29.4 active hours. Every
  one changed exactly one file. Bash was 1,996 calls (75%), and what it changed is not in the
  transcripts; counting every Bash call that may write (heuristic) bounds the rate at 42% of calls
  (**estimated**).
- **Cost per edit-tool commit (measured, replayed into peerfs at `b2c157a`).** Median 7 new
  objects and 3.5 KiB stored; p90 9 objects and 7.2 KiB; max 9 and 23.7 KiB. One object is
  content; the other six are the rewritten trees on the path and the commit, which make up 85% of
  objects and 19% of bytes.
- **Changed files are small (measured).** Median 7.0 KiB, p90 18.7 KiB, max 75.3 KiB; 97.5% are
  under 64 KiB and so stored whole. The chunker was used by 7 of 283 commits.
- **Whole-file storage is the overhead (measured).** The median Edit changes 478 bytes but stores
  4.3 KiB, because the whole file is stored again. A zstd-3 delta against the previous version
  is 2.7x smaller than whole-file zstd-3 in total (384 KiB against 1,030 KiB).
- **Derived data never passes through edit tools (measured) and dwarfs source on disk
  (estimated).** 0 of 283 commits touched `target/`, `node_modules/`, `.venv/`, `dist/`,
  `build/` or `__pycache__/`. The eight work trees the sessions edited hold 26.8 MiB of source and
  33.2 GiB of derived data today. In a controlled rebuild of this repository, one edit stores
  2.4–5.3 KiB and the `cargo build` after it 5.6–22 MiB.
- **Files change between edit-tool calls (measured).** 3 of 90 checkable chains broke, and 25 of
  40 Claude Code checkpoint backups of tool-edited files match no state the tool calls produced.
  Something other than the edit tools — Bash, a subagent, the user — rewrote those files.

## Sources and their limits

| Source | What it holds | Used for | Limits |
| --- | --- | --- | --- |
| Claude Code transcripts, `~/.claude/projects/*/*.jsonl` and `*/subagents/*.jsonl` | Every tool call with its input; for Edit and Write a `toolUseResult` with the path, the edit, a structured patch and usually `originalFile`, the whole file before the call; for Read the content read | Reconstruction and replay | Bash calls record the command and its output, never the files they changed. `originalFile` is absent on some Edits (on 47 of 147 here) |
| Claude Code checkpoint backups, `~/.claude/file-history/<session>/<hash>@vN` | The content of each tracked file at each user-prompt checkpoint | Checking the reconstruction; finding out-of-band changes | Present for 3 sessions only (47 backup files); written per prompt, not per call |
| The work trees on disk today | Source, build output, dependencies | Source/derived ratio | Today's state, not the state during the sessions (**estimated**) |
| Codex, `~/.codex/sessions/` | One rollout (2026-10-08): 35 `exec` shell calls, no `apply_patch` | Not used | Shell calls only, so no file changes are recoverable |

Transcripts were copied at 2026-10-27 04:37 UTC; they span 2026-08-04 to 2026-10-27. All 30
transcripts on the host were replayed except the one doing the measuring; one was empty, 15
made at least one edit-tool change. Two of the replayed sessions were still running when copied
(the session that started this work and one of its subagents).

## Method

1. **Parse.** Pair each `tool_use` with its `tool_result` and `toolUseResult`, dedupe streamed
   repeats by tool-use id, and keep the call order.
2. **Reconstruct** each Edit and Write. The base is, in order: `originalFile` (exact); the state
   the previous call on that path produced (chained); an earlier full Read (exact); else an
   estimate from today's file on disk or the patch's context lines. The result is the base with
   `oldString` replaced by `newString` (Edit) or the written content (Write); if `oldString` is
   not in the base, the structured patch is applied instead and the call is marked estimated. A
   later `originalFile` or full Read that disagrees with the tracked state is an out-of-band
   change, replayed as its own commit and reported separately.
3. **Replay** per session into a fresh peerfs space built from this branch: a seed commit with
   the first-known base of every touched file, then one `peerfs import` per call that changed
   bytes. New objects come from `GET /v1/space`; new stored bytes are the growth of `objects/`.
   Per commit the harness also computes the line-diff changed bytes, the whole-file zstd-3 size,
   and the zstd-3 `--patch-from` delta against the previous version.
4. **Classify paths**: derived if a directory component is `target`, `node_modules`, `.venv`,
   `venv`, `dist`, `build` or `__pycache__`; secret for `.env*`, keys and credentials; other for
   agent scratch (`/tmp`) and agent metadata (`~/.claude`, `~/.codex`, outside worktrees); source
   otherwise. Bash commands are classed by regular expression (**estimated**).
5. **Check** every checkpoint backup against all states reconstructed for its path.
6. **Size** the work trees on disk (`workspaces.py`) and run one controlled build
   (`build_churn.py`) as a stand-in for what Bash does.

Reconstruction quality (**measured**):

| Base of an edit-tool call | Calls |
| --- | --- |
| Exact: `originalFile`, or a new file | 234 |
| Chained from the previous call on the path | 35 |
| An earlier full Read | 3 |
| Estimated | 11 |
| Failed calls (error result, nothing changed) | 6 |

Wherever a later call carried `originalFile`, the chain was checked: 87 of 90 agreed. The 11
commits with an estimated base hold 103.7 KiB of the 1,268.8 KiB stored (8%); the per-call
tables include them; leaving them out moves each median by under 10% (changed file 6.8 KiB,
stored 3.4 KiB, delta 655 B).

## Per session

Sessions are named by a hash of the transcript path. Work is described from file extensions and
the Bash mix only. All columns are **measured** except Bash, whose writes are unmeasured.
Active hours sum the gaps between consecutive calls, each capped at 5 minutes.

| Session | Kind | Work | Tool calls | Bash | Edit-tool commits | Commits / call | Files | Changed | New objects | New stored | Active h | Commits / active h |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4e76c979 | main | benchmark operations over ssh; Python, shell, notes | 924 | 815 | 66 | 0.071 | 32 | 166.9 KiB | 474 | 299.2 KiB | 13.48 | 4.9 |
| 78ca3f30 | main | Python simulation project | 256 | 174 | 58 | 0.227 | 15 | 146.0 KiB | 457 | 234.1 KiB | 3.66 | 15.8 |
| beb14f5c | main | shell-driven operations, one note | 215 | 194 | 1 | 0.005 | 1 | 1.5 KiB | 7 | 1.6 KiB | 1.98 | 0.5 |
| 75facd24 | main | Rust implementation and design docs (running) | 193 | 132 | 33 | 0.171 | 27 | 201.9 KiB | 249 | 113.9 KiB | 1.73 | 19.1 |
| 332eca6a | main | shell-driven multi-agent tests | 168 | 150 | 2 | 0.012 | 2 | 1.8 KiB | 10 | 1.9 KiB | 2.48 | 0.8 |
| 1a48f3a6 | main | Rust and docs | 161 | 78 | 43 | 0.267 | 30 | 170.8 KiB | 263 | 151.0 KiB | 2.01 | 21.4 |
| 4ee9c366 | subagent | Python and Go scripts | 97 | 87 | 8 | 0.082 | 4 | 26.2 KiB | 60 | 27.2 KiB | 0.45 | 18.0 |
| 68d43c86 | subagent | Python scripts | 73 | 62 | 6 | 0.082 | 6 | 86.4 KiB | 50 | 31.1 KiB | 0.68 | 8.8 |
| 54eacb83 | subagent | Rust and config | 66 | 26 | 22 | 0.333 | 14 | 137.5 KiB | 90 | 103.5 KiB | 0.35 | 62.2 |
| 969eaf66 | subagent | Rust | 63 | 34 | 21 | 0.333 | 7 | 318.1 KiB | 100 | 214.3 KiB | 0.38 | 55.1 |
| 61d022c9 | subagent | research, one note | 55 | 36 | 1 | 0.018 | 1 | 26.8 KiB | 9 | 11.8 KiB | 0.12 | 8.1 |
| f15efbd6 | main | Python scripts | 42 | 33 | 6 | 0.143 | 6 | 56.8 KiB | 30 | 23.3 KiB | 0.74 | 8.1 |
| fa3fdb1b | main | one doc | 42 | 39 | 1 | 0.024 | 1 | 11.5 KiB | 6 | 5.5 KiB | 0.26 | 3.8 |
| 66608b16 | subagent | Rust (running) | 32 | 15 | 14 | 0.438 | 5 | 13.4 KiB | 76 | 47.1 KiB | 0.07 | — |
| 2aba409e | subagent | Rust, one file | 31 | 29 | 1 | 0.032 | 1 | 7.0 KiB | 5 | 3.2 KiB | 0.18 | 5.7 |
| 14 others | 5 subagents, 9 main | web research, messaging tests; no edits | 233 | 92 | 0 | 0 | 0 | 0 | 0 | 0 | 0.81 | 0 |
| **All 29** | | | **2,651** | **1,996** | **283** | **0.107** | **152** | **1,372.5 KiB** | **1,886** | **1,268.8 KiB** | **29.38** | **9.6** |

The commit rate per call varies 0–0.44 by session and tracks how much of the work is Bash: the
benchmark-operations sessions edit almost nothing through tools, and the Rust subagents commit on
one call in three.

## Per commit

Over the 283 calls that changed a file through an edit tool. All **measured**, except the row
marked estimated.

| Metric | Median | p90 | Max | Mean | Total |
| --- | --- | --- | --- | --- | --- |
| Files changed | 1 | 1 | 1 | 1.00 | 283 |
| Size of the changed file | 7.0 KiB | 18.7 KiB | 75.3 KiB | 10.0 KiB | 2,819.6 KiB |
| Logical changed bytes (removed plus added lines) | 1.4 KiB | 8.8 KiB | 290.6 KiB | 4.8 KiB | 1,372.5 KiB |
| Whole file, zstd-3 | 2.7 KiB | 6.4 KiB | 23.1 KiB | 3.6 KiB | 1,029.8 KiB |
| zstd-3 delta against the previous version | 601 B | 3.2 KiB | 19.7 KiB | 1.4 KiB | 384.5 KiB |
| New objects | 7 | 9 | 9 | 6.66 | 1,886 |
| … content | 1 | 1 | 2 | 1.02 | 290 |
| … trees and the commit | 6 | 8 | 8 | 5.64 | 1,596 |
| New stored bytes in peerfs | 3.5 KiB | 7.2 KiB | 23.7 KiB | 4.5 KiB | 1,268.8 KiB |
| Stored minus whole-file zstd: trees, commit, framing | 883 B | 1.2 KiB | 1.4 KiB | 864 B | 239.0 KiB |
| Trees rewritten in the real work tree (**estimated**) | 674 B | 1.3 KiB | 2.0 KiB | 765 B | 211.6 KiB |

The replay lays files out relative to each session's working directory, which for most sessions
is the home directory, so its trees are deeper than a project's but hold only the touched files.
The last row sizes the trees from the work tree's root to the file from today's directory
listings at about name + 38 bytes per entry: the two biases roughly cancel, and tree and commit
overhead stays below about 2 KiB per commit.

| Size of the changed file | Commits | Share |
| --- | --- | --- |
| < 4 KiB | 72 | 25.4% |
| 4–16 KiB | 172 | 60.8% |
| 16–64 KiB | 32 | 11.3% |
| ≥ 64 KiB, chunked | 7 | 2.5% |

| Tool | Commits | New files | Median changed | Median stored | Total stored |
| --- | --- | --- | --- | --- | --- |
| Edit | 147 | 0 | 478 B | 4.3 KiB | 768.9 KiB |
| Write | 136 | 122 | 5.1 KiB | 2.8 KiB | 499.9 KiB |

No MultiEdit or NotebookEdit calls occur in these sessions.

## Source, derived and other

**Through edit tools (measured):**

| Class | Commits | Changed bytes | Bytes at session end |
| --- | --- | --- | --- |
| Source | 216 | 1,145.2 KiB | 1,036.5 KiB |
| Derived | 0 | 0 | 0 |
| Secret | 0 | 0 | 0 |
| Other: agent scratch in `/tmp`, agent memory and notes in `~/.claude` | 67 | 227.4 KiB | 236.2 KiB |

A quarter of edit-tool commits (67) write outside any project: scratch scripts and the agent's
own memory files. A workspace scoped to a project would not see them.

**On disk, the eight git work trees these sessions edited (estimated: today's state):**

| Work tree | Markers | Source files | Source | Derived | Derived files | `.git` | Derived / source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 453bb8 | Python | 676 | 4.3 MiB | 0.0 MiB | 3 | 197.2 MiB | 0 |
| f637fa | monorepo | 399 | 12.5 MiB | 9,711 MiB | 9,461 | 14.0 MiB | 780 |
| 50c7cb | monorepo | 414 | 8.5 MiB | 20,918 MiB | 33,668 | 30.8 MiB | 2,471 |
| b7d9e6 | docs | 10 | 0.1 MiB | 0 | 0 | 0.1 MiB | 0 |
| 17ceda | Rust | 26 | 0.8 MiB | 363 MiB | 1,245 | 0.6 MiB | 447 |
| ccb344 | docs | 12 | 0.2 MiB | 0 | 0 | 1.0 MiB | 0 |
| cb1059 | Rust | 33 | 0.2 MiB | 1,911 MiB | 6,317 | 0.1 MiB | 8,267 |
| 9574b2 | Rust, a git worktree | 32 | 0.2 MiB | 1,134 MiB | 3,129 | 0.0 MiB | 5,190 |
| **All 8** | | | **26.8 MiB** | **34,036 MiB** | | **243.8 MiB** | **1,269** |

Source is what `git ls-files -co --exclude-standard` lists; derived is everything under a derived
directory name; ignored files outside those came to under 0.1 MiB everywhere. Nested worktrees
are sized on their own. Where a project has derived data at all, it is 450 to 8,300 times its
source.

**One build, controlled (measured on a stand-in, not on a session).** `build_churn.py` exported
this branch, ran `cargo build -p peerfs` (debug), and committed the whole tree with `target/`;
then made one source edit, committed, rebuilt and committed again — first in the leaf crate
(`crates/peerfs`), then in the root crate (`crates/peerfs-core`):

| Step | New objects | New stored | `target/` files changed | Their size |
| --- | --- | --- | --- | --- |
| Seed: 212 KiB source, 664 MiB `target/` in 1,515 files | 6,823 | 119.6 MiB | — | — |
| Edit in the leaf crate | 6 | 5.3 KiB | — | — |
| `cargo build` after it | 152 | 5.6 MiB | 206 | 30.5 MiB |
| Edit in the root crate | 6 | 2.4 KiB | — | — |
| `cargo build` after it | 647 | 22.0 MiB | 650 (206 removed) | 164.9 MiB |

A build call stores 1,000 to 10,000 times what the edit before it stored.

## Bash: what remains unmeasured

1,996 of 2,651 calls (75%) were Bash. A transcript records the command and its output but no
file it changed, so their file effects are not in any figure above except as detected below.

Classed by regular expression on the command (**estimated**; first match wins):

| Class | Calls | Share of Bash |
| --- | --- | --- |
| Inspect only (reads, listings, queries) | 926 | 46.4% |
| May write files: redirection, heredoc, `sed -i`, `cp`, `mv`, `rm`, `mkdir` … | 522 | 26.2% |
| Remote: `ssh`, `scp` — changes land on other hosts | 229 | 11.5% |
| Build, test, format: `cargo`, `pytest`, `make`, … | 185 | 9.3% |
| Version control writes: `git commit`, `checkout`, `add`, … | 113 | 5.7% |
| Dependency installs: `pip install`, `npm install`, `cargo add`, … | 21 | 1.1% |

Evidence that Bash and other writers change agent-edited files (**measured**, lower bounds):

- 3 of 90 chains broke: a later `originalFile` disagreed with the state the previous edit-tool
  call produced (median 474 B changed, max 2.3 KiB).
- 25 of 40 checkpoint backups of tool-edited files, in the 3 sessions that have backups, match no
  state the edit tools produced; they differ from the nearest by median 1.0 KiB, p90 28.3 KiB.
  23 of the 25 are in one session. The ones inspected (by diff shape only) are small line
  replacements and insertions, the shape of `sed` or script edits.

So the commit rate is between 0.107 per call (edit tools only, **measured**) and 0.424 if every
possibly-writing local Bash call changed something (**estimated** upper bound). Not measured at all:

- What builds, tests and formatters wrote, per call. The controlled build above is the only
  figure, and it is one repository and one toolchain.
- Dependency installs: how many bytes, how often. 21 install commands (1.1% of Bash,
  **estimated**) and 4 of 283 edit-tool commits touching a manifest or lockfile (**measured**)
  are the only signals.
- Whether derived output repeats across builds, so chunk dedup catches it, beyond the one rebuild
  above.
- Files that are deleted, renamed or created by Bash and never touched by an edit tool.
- Codex sessions: shell only.

## What a live hook-based run would add

A `PostToolUse` hook (or the peerfs mount, once milestone 4 lands) that snapshots the workspace
after every call, Bash included, would measure directly what is estimated or missing here:

- The true commit rate: which Bash calls change files, and how many files each.
- Derived and dependency bytes per call, with the real `target/`, `node_modules/` and `.venv/`
  churn of builds, tests and installs, and how much chunk dedup recovers across builds.
- Deletions and renames, which transcripts never show.
- Exact trees: real directory sizes and depths instead of the replay's touched-files-only trees.
- Bytes per commit at 16, 64 and 128 KiB chunk averages on derived output, the only data here big
  enough for the chunk size to matter.
- Timings: the cost of a snapshot per call against the call itself.

## What this implies

**Decision 1, when a commit happens.** A local snapshot per tool call is cheap in bytes: about
10 commits per active hour from edit tools, each one file, 3.5 KiB and 7 objects at the median —
about 350 KiB per eight active hours of source. The out-of-band changes say the snapshot must be taken after
every call, Bash included, from the workspace, not from the edit tools' own records, or per-call
history misses changes to the files the agent is editing. Six of seven objects per commit are
trees and the commit, so object count, index size and anything signed per commit scale with path
depth and commit count rather than with bytes; that supports coalescing before signing and
replicating.

**Decision 5, what a workspace contains.** Source changes are small and all go through edit
tools or small scripts; derived data changes only through Bash and is three orders of magnitude
larger at rest and per build. Committing derived data locally for complete undo costs megabytes
per build call — 5.6 to 22 MiB here — so retention (decision 7) and the per-branch size cap need
to apply locally, not only to replication. A quarter of edit-tool commits are agent scratch and
memory outside the project, which none of the three path classes covers yet.

**Decision 6, chunking and hashing.** Files changed by edit tools have a median of 7 KiB and a
max of 75 KiB, so 97.5% are stored whole and the chunk average does not affect these commits; it
matters only for derived output. Whole-file storage is the main overhead: a median Edit changes
478 B and stores 4.3 KiB. zstd deltas against the previous version are 2.7x smaller in total, so
the planned transfer deltas are worth having, and the same deltas in local packs would cut
source storage by about 60%.

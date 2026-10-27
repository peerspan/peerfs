# Commits and bytes per tool call

Replays recorded Claude Code sessions into peerfs spaces, one commit per tool call that changed
a file, and reports what each commit costs. It reads transcripts and never starts an agent.
Results from the first run are in `docs/measurements/2026-10-27-tool-calls.md`.

## Rerun

```sh
# Take a fixed copy first: live sessions keep appending to their transcripts.
cp -r ~/.claude/projects/. "$SNAP"/
harness/measure/run.sh "$WORK" "$SNAP"/*/*.jsonl "$SNAP"/*/*/subagents/*.jsonl > results.md
MEASURE_BUILD=1 harness/measure/run.sh "$WORK" ...   # also run the controlled build experiment
```

With no transcripts named, `run.sh` replays every transcript under `~/.claude/projects`, minus
any matching the `grep -E` pattern in `$MEASURE_EXCLUDE` (exclude the session doing the
measuring). `$WORK` must be shallow: each space's socket is `$WORK/s/<id>/sp/peerfs.sock`, and a
Unix socket path is limited to 107 bytes. The run takes seconds per session; the build experiment
about a minute and 1 GB of disk.

`$WORK/s` holds the reconstructed trees and the spaces, which contain file contents: keep it in
a private scratch directory and delete it afterwards. `$WORK/out/sessions.json` and
`$WORK/out/calls.csv`, and everything printed, carry only counts, sizes, tool names, path classes
and session ids hashed from the transcript path.

## Pieces

| File | Does |
| --- | --- |
| `measure.py` | Parses transcripts, reconstructs file states, replays them into peerfs, writes `calls.csv` and `sessions.json` |
| `summarize.py` | Turns those into the Markdown tables of the results doc |
| `workspaces.py` | Sizes the git work trees the sessions edited, as they are on disk now: source, derived, other, `.git` |
| `build_churn.py` | Controlled stand-in for a Bash build call: commits this repo with `target/`, edits one file, rebuilds, commits again |

## How a file state is reconstructed

Claude Code records, for each successful Edit and Write, a `toolUseResult` holding the file
path, the edit, and usually `originalFile`, the whole file before the call. In order of
preference the base of a call is: `originalFile` (exact); the state the previous call on that
path produced (chained, checked against `originalFile` whenever a later call carries one); the
content of an earlier full Read (exact); or, failing those, an estimate from today's file on disk
or the patch's own context lines (labelled estimated in every output). A Write of a new file has
no base. When a later `originalFile` or full Read disagrees with the tracked state, something
other than an edit tool changed the file in between; the harness records that as an
out-of-band change and commits it separately.

Claude Code's checkpoint backups (`~/.claude/file-history/<session>/`) are used only to check the
reconstruction: each backup is compared with every state reconstructed for its path.

## Replay

Per session: a fresh space; a seed commit holding the first-known base of every touched file;
then one `peerfs import` per changed state, measuring the space's object count (`GET /v1/space`)
and the size of `objects/` before and after. Paths are laid out relative to the session's working
directory. The replayed tree holds only touched files, so its trees are smaller than a real
workspace's; `real_tree_bytes` estimates the real trees from today's directory listings.

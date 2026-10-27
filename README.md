# peerfs

A filesystem for agents across machines, under the Peerspan brand.

Agents work in isolated, forkable workspaces over one shared, content-addressed history,
which moves between machines peer to peer with no cloud in the data path. One person's
laptop, servers and sandboxes is the first case; machines owned by different people sharing
selectively is the broader one.

The design and its agreed decisions are in `peerspan/docs-internal`, `docs/peerfs.md`.

## Status: milestone 1, the local core

What works today, on one host:

| Piece | What it does | Design decision |
| --- | --- | --- |
| Object identities | BLAKE3-256 under a context per object kind, prefixed by an algorithm code | 6 |
| Content | Files under 64 KiB stored whole; larger files split by FastCDC 2020 at 16 / 64 / 256 KiB, so an edit stores only nearby chunks | 6 |
| Store | Append-only packs of up to 16 MiB, zstd level 3 per object (raw when that is no smaller), every read checked against its identity, a torn tail cut off on open | 3, 6 |
| Trees and commits | Canonical encodings; commits carry the host, agent and session, and a stable change ID | 1 |
| Branches | Move only by compare-and-swap, written by sync and rename; forking copies nothing | — |
| Workspaces | A writable overlay over a commit; every write synced before it returns | 3, 4 |
| Commit | Fast path when the branch has not moved; otherwise a three-way merge onto the new head. Text up to 1 MiB merges by line; a conflict commits nothing and leaves markers in the workspace to resolve | 2 |
| CLI | `peerfs init`, `import`, `export`, `branches`, `fork`, `log`, and `peerfs ws create / read / write / rm / ls / status / commit / discard` | 4 |

Not yet: signing, replication between hosts and durability receipts, owner rules for
conflicts (`regenerate`, `ours`/`theirs`, structured merge), source/derived/secret path
classes, garbage collection, branch handoff, the local contract with the Peerspan host
daemon, and the FUSE, NFS and VM frontends. Files are read and written whole in memory.
A space is held by one process at a time. Linux and macOS only.

## Try it

```sh
cargo build --release
export PEERFS_SPACE=/tmp/demo.peerfs PEERFS_AGENT=me
peerfs init
peerfs import ./my-project -m "Import"
peerfs ws create agent-1 && peerfs ws create agent-2
echo "one" | peerfs ws write agent-1 notes.txt
echo "two" | peerfs ws write agent-2 notes.txt
peerfs ws commit agent-1 -m "first"
peerfs ws commit agent-2 -m "second"    # conflicts: exit 1, markers in agent-2
peerfs ws read agent-2 notes.txt
```

## Layout

| Path | Contents |
| --- | --- |
| `crates/peerfs-core` | The library: objects, store, branches, workspaces, merge |
| `crates/peerfs` | The command-line interface |

A space on disk is `FORMAT`, `objects/pack-*.pack`, `refs/branches/<name>`, and
`workspaces/<name>/` (`state`, `whiteouts`, `upper/`).

## Checks

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The lint set is BeautifulRust's (`[workspace.lints]` in `Cargo.toml`, `clippy.toml`).

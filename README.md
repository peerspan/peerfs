# peerfs

A filesystem for agents across machines, under the Peerspan brand.

Agents work in isolated, forkable workspaces over one shared, content-addressed history,
which moves between machines peer to peer with no cloud in the data path. One person's
laptop, servers and sandboxes is the first case; machines owned by different people sharing
selectively is the broader one.

## Status

Early, and changing quickly. What works today, on one host:

| Piece | What it does |
| --- | --- |
| Object identities | BLAKE3-256 under a context per object kind, prefixed by an algorithm code |
| Content | Files under 64 KiB stored whole; larger files split by FastCDC 2020 at 16 / 64 / 256 KiB, so an edit stores only nearby chunks |
| Store | Append-only packs of up to 16 MiB, zstd level 3 per object (raw when that is no smaller), every read checked against its identity, a torn tail cut off on open |
| Trees and commits | Canonical encodings; commits carry the host, agent and session, and a stable change ID |
| Branches | Move only by compare-and-swap, written by sync and rename; forking copies nothing |
| Workspaces | A writable overlay over a commit; every write synced before it returns |
| Commit | Fast path when the branch has not moved; otherwise a three-way merge onto the new head. Text up to 1 MiB merges by line; a conflict commits nothing and leaves markers in the workspace to resolve |
| Pull | Fetches another space's branch and its whole history, only the objects missing here, each checked against its identity and expected kind; records it as the tracking reference `<host>/<branch>`. The store stays closed (it holds an object only with everything it references), so an interrupted pull leaves no reference and resumes. Between spaces on one host for now; across hosts through the Peerspan host daemon later |
| Push | Sends a branch to the space that holds it: asks which objects it lacks (a held object needs nothing beneath it sent), sends those children first, and the receiver refuses any object that does not match or references one it lacks. The home host moves its branch only by compare-and-swap against the head the pusher last pulled, and only as a fast-forward, so a push never drops commits it did not see |
| Snapshots | `ws snapshot`, meant once per tool call, stores the workspace's tree as an unsigned commit for its current change, chained to the change's previous snapshot; the branch and overlay are untouched and snapshots never leave the host. `ws commit` ends the change: it takes a last snapshot, merges onto the moved head, and stores one signed commit whose `predecessor` names that snapshot. The link is weak, so pushing and pulling never carry snapshots. `ws restore NAME SNAPSHOT` (a unique start of the ID is enough) puts the source paths back as a snapshot had them, keeping derived and secret paths; it snapshots before and after, so every step stays listed and restoring a later one goes forward again. It is refused once the change has moved onto a new base (a conflicted commit or a merge), since earlier snapshots would undo what that brought |
| Signing | Every commit is signed with Ed25519 when stored; the signature covers a domain-separated encoding of the commit and is part of its identity. A commit pulled or pushed in is refused unless it is signed by a key listed for the host it names as its author. For now a space signs with its own `signing.key` and trusts the keys in its `hosts` file (`peerfs key`, `peerfs hosts add`); later the host daemon signs and supplies peers' keys, behind the same `Signer` and `HostKeys` traits |
| Merge | `ws merge` brings a branch or `<host>/<branch>` into a workspace's branch: up to date, fast-forward, a clean merge commit with both parents, or conflict markers to resolve, after which the commit records both parents. A refused push is followed by pull, merge, push |
| Mount | `ws mount NAME DIR` serves a workspace as an ordinary directory over FUSE (Linux), from the space's server through fuse-backend-rs, the library that also serves vhost-user-fs for VMs. Base files read from the store; the first write copies a file into the overlay; removals are whiteouts; a directory holding committed content is renamed by copying (`EXDEV`), as on kernel overlay file systems; empty directories are kept; hard links share one overlay file. Data is durable at `fsync`. A base file's mtime is the workspace's date for its content at that path: the overlay file's mtime when a commit took it, or when the workspace first saw it, so commits and remounts do not make files look modified. `git` and `cargo build`/`test` run in it. `ws mount NAME DIR --nfs` serves it over NFS version 3 on `127.0.0.1` instead, for macOS, where FUSE needs a kernel extension: the same code behind a vendored `nfsserve`, exporting `/<workspace>`, accepting only reserved source ports (the kernel's client), and mounted `soft` so a dead server fails calls instead of hanging them. Writes are synced at COMMIT, which the client sends at `fsync` and when a written file is closed |
| Agent hook | `peerfs hook claude-code`, a Claude Code `PostToolUse` hook, snapshots each mounted workspace a tool call touched (the session's directory, or a path in the tool's input), shell commands included, recording the tool and its tool use ID; outside a peerfs mount it does nothing and starts no server |
| Path classes | Every path is **source** (committed, snapshotted, replicated), **derived** (dependencies and build output) or **secret** (credentials and keys). Derived and secret paths stay as files in the workspace across commits; no commit or snapshot takes them, so they are never replicated and secrets are never hashed. Built-in defaults always apply (`node_modules/`, `target/`, `.venv/`, `dist/`, `build/`, `.git/`, ...; `.env*`, keys, cloud and package credentials, with templates such as `.env.example` and `*.pub` left as source). A `.gitignore` can only add derived paths. The owner's `<space>/classes` file adds rules under `[source]`, `[derived]` and `[secret]`; an owner's source rule wins over everything, and secret wins over derived. `ws status` lists them as kept local |
| Garbage collection | `peerfs gc [--dry-run]`, run by hand, drops what the space no longer needs. It keeps branches, tracking references, and each workspace's base, merge and current snapshots, with everything they reach. Snapshots are kept like a reflog: a committed change's for 90 days after its commit, any snapshot for 30 days after it was taken, and a kept snapshot keeps the earlier ones of its change. Packs written in the last day are left alone, so a pull or push in progress keeps its objects. The packs it rewrites are copied, synced, and only then removed, so a crash leaves at worst an object held twice |
| Server | `peerfs serve` owns the space and serves it at `<space>/peerfs.sock`, so concurrent agents serialize through it; it exits after an idle timeout (not while a workspace is mounted), on SIGTERM and on `peerfs stop`, unmounting what it mounted |
| API | HTTP/1.1 and JSON over the Unix socket, following the host daemon's `admin.sock` conventions; a Rust client in `peerfs-api` |
| CLI | A client of the server, starting one in the background when none answers: `peerfs init`, `serve`, `stop`, `import`, `pull`, `push`, `remotes`, `key`, `hosts [add]`, `export`, `branches`, `fork`, `log`, `gc`, and `peerfs mounts`, and `peerfs ws create / read / write / rm / ls / status / snapshot / snapshots / restore / commit / merge / mount / unmount / discard`; `peerfs hook claude-code` runs without one |

Not yet: signing through the host daemon, replication between hosts and durability receipts, owner rules for
conflicts (`regenerate`, `ours`/`theirs`, structured merge), sharing derived data through
an environment-keyed cache, a content scan for secrets inside source files, automatic garbage collection and expiring abandoned workspaces, branch handoff, the local contract with the Peerspan host
daemon, FUSE passthrough, the VM frontends, and NFS on macOS itself (the hook reads
mounts from `/proc`, and the macOS client is untested). Files are read and written
whole in memory, and the server applies requests one at a time. Linux and macOS only.

On this repository the mount is close to native: a clean `cargo build` takes 42-46 s against
37-47 s natively, and a no-change rebuild 0.27-0.41 s against 0.20-0.25 s. Over NFS on Linux
the same take 51-57 s and 0.38-0.56 s. Writes travel up to
1 MiB per request, recently used overlay files stay open, and the mount rereads the
workspace only after the local API changes it. The kernel caches names and attributes for a
second, so for that long after a commit or another change through the local API, the
mount can still show the earlier state.

## The local API

`peerfs serve` listens on `<space>/peerfs.sock` (mode 0600) and writes `<space>/serve.pid`.
Requests and responses are JSON under `/v1`; errors are `{"code", "error"}` with `invalid`
(400), `forbidden` (403, a commit signed by a key not listed for its host), `not_found` (404), `conflict` (409), `remote` (502, another host failed or sent
something unusable) or `internal` (500); unknown fields are rejected. Wherever a branch is
named, `<host>/<branch>` names a tracking reference.
File contents are raw bodies. Commits record the server's host and the agent and session
the request names.

| Route | Does |
| --- | --- |
| `GET /v1/space` | The space's path, host, object count and signing key |
| `GET, POST /v1/branches` | List branches; fork one (`{name, from}`) |
| `GET /v1/branches/{name}/log?limit=N` | History, newest first |
| `POST /v1/import`, `POST /v1/export` | Commit a directory onto a branch; write a revision to a directory (absolute paths on the server's host) |
| `GET, POST /v1/workspaces` | List workspaces; create one (`{name, branch}`) |
| `GET, DELETE /v1/workspaces/{name}` | Describe or discard a workspace |
| `GET, PUT, DELETE /v1/workspaces/{name}/files/{path}` | Read, write or remove a file |
| `GET /v1/workspaces/{name}/list?path=dir` | List a directory |
| `GET /v1/workspaces/{name}/changes` | Uncommitted changes |
| `POST /v1/workspaces/{name}/commit` | Commit (`{message, caller}`); the outcome is `committed`, `conflicted` or `unchanged` |
| `POST /v1/workspaces/{name}/snapshot` | Snapshot the workspace (`{message?, caller}`); the outcome is `snapshot` or `unchanged` |
| `GET /v1/workspaces/{name}/snapshots` | The current change's snapshots, newest first |
| `POST /v1/workspaces/{name}/restore` | Restore the source paths to one of them (`{snapshot, caller}`, the snapshot whole or by the start of its ID); returns `{target, before?, snapshot?}`. `conflict` once the change has moved onto a new base |
| `POST /v1/objects/fetch` | Objects by id (`{ids}`, at most 256), as `application/octet-stream`; those not held are left out |
| `POST /v1/pull` | Pull a branch from another server (`{from, branch}`, `from` its socket); the space is held only between fetches |
| `GET /v1/remotes` | Tracking references, with when each was pulled |
| `POST, DELETE /v1/workspaces/{name}/mount` | Mount the workspace (`{path, frontend}`: an absolute empty directory, and `fuse`, the default, or `nfs`); unmount it |
| `GET /v1/mounts` | Mounted workspaces and their directories |
| `POST /v1/gc` | Collect garbage (`{dry_run}`); returns what was dropped, or would be, and what was kept |
| `GET, POST /v1/hosts` | Keys trusted to sign other hosts' commits; add one (`{host, key}`, the key in standard Base64) |
| `POST /v1/objects/have` | Which of these objects (`{ids}`, at most 256) the space holds |
| `POST /v1/objects/receive` | Store objects (`application/octet-stream`), each after everything it references |
| `POST /v1/branches/{name}/advance` | Move a branch (`{expected, new}`) by compare-and-swap, fast-forward only |
| `POST /v1/push` | Send a local revision to another server (`{to, branch, from}`) and advance its branch; `conflict` if it moved |
| `POST /v1/workspaces/{name}/merge` | Merge a revision (`{from, caller}`); the outcome is `up_to_date`, `fast_forward`, `merged` or `conflicted` |

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

## Snapshot every tool call (Claude Code)

Run Claude Code in a mounted workspace and add the hook to its settings
(`.claude/settings.json`, or `~/.claude/settings.json` for every project):

```json
{
  "env": { "PEERFS_SPACE": "/path/to/space" },
  "hooks": {
    "PostToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "peerfs hook claude-code" }] }
    ]
  }
}
```

After each tool call it snapshots the workspace when anything changed, with a message such
as `Bash: cargo test (toolu_01...)`; `peerfs ws snapshots NAME` lists them, each with the agent,
host and session that took it, and `ws commit` coalesces them into one signed commit. The agent recorded is `PEERFS_AGENT`, or
`claude-code`; the session is `PEERFS_SESSION`, or Claude Code's session ID. A snapshot of
this repository mounted with a 1.2 GB `target/` takes 15-40 ms, and a call that changed
nothing about 12 ms. Errors exit 1, which Claude Code shows without stopping the agent.

## Layout

| Path | Contents |
| --- | --- |
| `crates/peerfs-core` | The library: objects, store, branches, workspaces, merge |
| `crates/peerfs-api` | The local API's JSON bodies and a client over its socket |
| `crates/peerfs-mount` | A workspace as a FUSE or loopback NFS file system |
| `crates/nfsserve` | Hugging Face's `nfsserve` 0.11.0 (BSD-3-Clause), vendored with LINK, COMMIT, a READDIR fix and reserved-port checks; see its `PEERFS.md` |
| `crates/peerfs-server` | The server: one space behind the API |
| `crates/peerfs` | The command-line interface, a client of the server |

A space on disk is `FORMAT`, `HOST` (the host given at `init`), `signing.key` (mode 0600), `hosts`, `objects/pack-*.pack`, `refs/branches/<name>`, `refs/remotes/<host>/<branch>`, and
`workspaces/<name>/` (`state`, which names the base, change, latest snapshot and any merge, `whiteouts`, `upper/`), and while a server runs,
`peerfs.sock`, `serve.pid` and its log `serve.log`.

## Checks

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The lint set is BeautifulRust's (`[workspace.lints]` in `Cargo.toml`, `clippy.toml`).

## Contributing

We welcome bug reports, feature requests and design feedback as
[issues](https://github.com/peerspan/peerfs/issues/new/choose). We do not accept code
contributions from outside Peerspan, and pull requests from outside are closed. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). `crates/nfsserve` is Hugging Face's
`nfsserve`, vendored with changes, and keeps its BSD-3-Clause license
([crates/nfsserve/LICENSE](crates/nfsserve/LICENSE)).

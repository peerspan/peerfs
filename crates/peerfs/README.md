# peerfs (the command-line interface)

A thin command-line client of a space's server.

- **`peerfs init`** creates a space directly with `peerfs-core`, since no server can run
  before the space exists.
- **`peerfs serve`** runs the server from `peerfs-server` in the foreground. It owns the
  space and listens on `<space>/peerfs.sock`.
- **`peerfs stop`** stops the space's server: it sends SIGTERM to the process in
  `<space>/serve.pid` and waits up to 10 s for the server to remove its socket, failing
  if it does not. It never starts a server; with none running it says so and exits 0.
  Before signalling, it checks that the process is `peerfs serve` for this space (from
  `/proc/<pid>/cmdline` on Linux, otherwise by the socket answering), so a stale pid file
  naming a dead or unrelated process is removed and nothing is signalled.
- **Every other command** (`import`, `export`, `branches`, `fork`, `log`, `ws ...`) is a
  request to that server through the client in `peerfs-api`. If no server answers, the
  command starts `peerfs serve --idle-timeout 600` in the background, logging to
  `<space>/serve.log`, and waits for it. When several commands race to start one, one
  server wins and the others exit.

The CLI holds no storage logic. Every command maps to one or two API calls, and the CLI
formats the result: a conflicted commit prints its conflicts and exits with status 1.
Agents and sessions come from `--agent` and `--session` (or `PEERFS_AGENT`,
`PEERFS_SESSION`) and are recorded on commits. The host is the server's.

Tests in `tests/cli.rs` drive the built binary, including eight processes that start one
server between them and all commit, and `stop` against a running server, no server, and
stale pid files.

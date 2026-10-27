# peerfs (the command-line interface)

A thin command-line client of a space's server.

- **`peerfs init`** creates a space directly with `peerfs-core`, since no server can run
  before the space exists.
- **`peerfs serve`** runs the server from `peerfs-server` in the foreground. It owns the
  space and listens on `<space>/peerfs.sock`.
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
server between them and all commit.

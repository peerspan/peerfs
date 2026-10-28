# nfsserve, as vendored by peerfs

Hugging Face's [nfsserve](https://github.com/huggingface/nfsserve) 0.11.0, BSD-3-Clause
(see `LICENSE`), copied from the published crate. peerfs serves workspaces to the macOS NFS
client through it. Changes, each off or unchanged by default so they could go upstream:

- **LINK.** `NFSFileSystem::link` makes a hard link; it returns `NFS3ERR_NOTSUPP` unless
  implemented. Without it, `cargo` copies build scripts instead of linking them.
- **COMMIT and unstable writes.** With `NFSFileSystem::unstable_writes`, WRITE replies
  `UNSTABLE` unless the client asked for a stable write, and COMMIT calls
  `NFSFileSystem::commit` to sync. Before, every write was reported as `FILE_SYNC` whether or
  not it was on disk.
- **Reserved ports only.** `NFSTcpListener::with_reserved_ports_only` refuses connections from
  ports at or above 1024, so only privileged processes such as the kernel's client connect.
- **READDIR continues from its cookie.** It always listed from the start, so a directory
  needing more than one reply was listed again and again; it now calls
  `NFSFileSystem::readdir` from the cookie, as READDIRPLUS does.

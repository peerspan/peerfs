//! `peerfs`: create spaces, serve them, and work in workspaces from the command line.
//!
//! Every command except `init`, `serve` and `stop` is a client of the space's server. When
//! none is running, the command starts one in the background, which exits after ten idle
//! minutes or on `peerfs stop`.

mod stop;

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use peerfs_api::{
    Caller, ChangeInfo, Client, ClientError, CommitResponse, EntryKind, MergeResponse, Revision,
    SnapshotResponse,
};
use peerfs_core::{Author, Space};
use peerfs_server::{Options, serve, socket_path};

/// The file in a space naming the host its server records on commits.
const HOST_FILE: &str = "HOST";

/// How long a server started by a command waits for the next request before exiting.
const AUTOSTART_IDLE: Duration = Duration::from_secs(600);

/// How long a command waits for a server it started to answer.
const AUTOSTART_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(version, about = "A filesystem for agents across machines")]
struct Cli {
    /// The space's directory.
    #[arg(long, env = "PEERFS_SPACE", global = true, default_value = ".peerfs")]
    space: PathBuf,
    /// The host recorded on commits. `init` saves it in the space's `HOST` file, which a
    /// server uses when this is not given; without either, this machine's hostname. A
    /// running server records its own.
    #[arg(long, env = "PEERFS_HOST", global = true)]
    host: Option<String>,
    /// The agent recorded on commits.
    #[arg(long, env = "PEERFS_AGENT", global = true)]
    agent: Option<String>,
    /// The agent session recorded on commits.
    #[arg(long, env = "PEERFS_SESSION", global = true)]
    session: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a space with an empty `main` branch.
    Init,
    /// Serve the space on its socket until stopped.
    Serve {
        /// Exit after this many seconds without a request.
        #[arg(long)]
        idle_timeout: Option<u64>,
    },
    /// Stop the space's server, if one is running. Never starts one.
    Stop,
    /// Commit a directory's contents onto a branch.
    Import {
        /// The directory to import.
        source: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(short, long, default_value = "Import")]
        message: String,
    },
    /// Write a branch's tree, or a commit's, out to an empty directory.
    Export {
        /// The directory to write.
        target: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
        /// A commit to export instead of the branch head.
        #[arg(long)]
        commit: Option<String>,
    },
    /// List branches and their heads.
    Branches,
    /// Fetch another space's branch and its history, kept as `<host>/<branch>`.
    Pull {
        /// The other space's directory, on this host.
        from: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// List branches pulled from other hosts, as of their last pull.
    Remotes,
    /// List mounted workspaces and where they are mounted.
    Mounts,
    /// Print the host and the key this space signs its commits with, as `<host> <key>`:
    /// what another space gives `hosts add` to accept them.
    Key,
    /// List the keys trusted to sign other hosts' commits, or add one. Commits pulled or
    /// pushed here must be signed by a key listed for the host they name.
    Hosts {
        #[command(subcommand)]
        command: Option<HostsCommand>,
    },
    /// Send a branch to another space and move its branch there. Refused if that branch
    /// moved since the last pull: pull, `ws merge`, and push again.
    Push {
        /// The other space's directory, on this host.
        to: PathBuf,
        /// The branch there.
        #[arg(long, default_value = "main")]
        branch: String,
        /// The local branch to send; defaults to `--branch`.
        #[arg(long)]
        from: Option<String>,
    },
    /// Create a branch at another branch's head. Copies no data.
    Fork {
        /// The new branch.
        name: String,
        #[arg(long, default_value = "main")]
        from: String,
    },
    /// Show a branch's history, newest first.
    Log {
        #[arg(long, default_value = "main")]
        branch: String,
        /// Show at most this many commits.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Work in a workspace.
    #[command(subcommand, name = "ws")]
    Workspace(WorkspaceCommand),
}

#[derive(Debug, Subcommand)]
enum HostsCommand {
    /// Trust `key`, in standard Base64, to sign commits naming `host` as their author.
    Add { host: String, key: String },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Create a workspace following a branch.
    Create {
        name: String,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// List workspaces.
    List,
    /// Print a file.
    Read { name: String, path: String },
    /// Write a file from standard input, or from `--file`.
    Write {
        name: String,
        path: String,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Remove a file or directory.
    Rm { name: String, path: String },
    /// List a directory.
    Ls {
        name: String,
        #[arg(default_value = ".")]
        path: String,
    },
    /// Show uncommitted changes.
    Status { name: String },
    /// Snapshot the workspace for its current change, unsigned and kept on this host:
    /// meant once per tool call. Prints the snapshot, or nothing when nothing changed.
    Snapshot {
        name: String,
        /// What the snapshot follows, such as the tool call.
        #[arg(short, long)]
        message: Option<String>,
    },
    /// List the snapshots of the workspace's current change, newest first.
    Snapshots { name: String },
    /// Commit the workspace's changes to its branch as one signed commit, replacing the
    /// change's snapshots.
    Commit {
        name: String,
        #[arg(short, long)]
        message: String,
    },
    /// Merge a branch, or `<host>/<branch>`, into the branch the workspace follows.
    Merge { name: String, revision: String },
    /// Mount the workspace at an empty directory, served by the space's server until
    /// unmounted or the server stops.
    Mount { name: String, dir: PathBuf },
    /// Unmount the workspace. Files still open there stop working.
    Unmount { name: String },
    /// Delete a workspace and its uncommitted changes.
    Discard { name: String },
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "peerfs: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let space_dir = std::path::absolute(&cli.space)
        .with_context(|| format!("resolving {}", cli.space.display()))?;
    // The host a space was created with, unless `--host` says otherwise.
    let host = cli
        .host
        .clone()
        .or_else(|| {
            fs::read_to_string(space_dir.join(HOST_FILE))
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|host| !host.is_empty())
        })
        .unwrap_or_else(hostname);
    match cli.command {
        Command::Init => {
            let author = Author {
                host,
                agent: cli.agent,
                session: cli.session,
            };
            Space::init(&space_dir, &author)
                .with_context(|| format!("creating a space in {}", space_dir.display()))?;
            let host_file = space_dir.join(HOST_FILE);
            fs::write(&host_file, format!("{}\n", author.host))
                .with_context(|| format!("writing {}", host_file.display()))?;
            writeln!(io::stdout().lock(), "created {}", space_dir.display())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve { idle_timeout } => {
            let space = match Space::open(&space_dir) {
                Ok(space) => space,
                Err(peerfs_core::Error::SpaceInUse { .. })
                    if Client::new(socket_path(&space_dir)).space().await.is_ok() =>
                {
                    writeln!(
                        io::stderr().lock(),
                        "peerfs serve: {} is already served; exiting",
                        space_dir.display()
                    )?;
                    return Ok(ExitCode::SUCCESS);
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("opening {}", space_dir.display()));
                }
            };
            let options = Options {
                host,
                idle_timeout: idle_timeout.map(Duration::from_secs),
            };
            serve(space, options)
                .await
                .with_context(|| format!("serving {}", space_dir.display()))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Stop => stop::stop(&space_dir, &mut io::stdout().lock()).await,
        command => {
            let client = connect(&space_dir, cli.host.as_deref()).await?;
            let caller = Caller {
                agent: cli.agent,
                session: cli.session,
            };
            let mut out = io::stdout().lock();
            client_command(&client, command, &caller, &mut out).await
        }
    }
}

/// A client of the space's server, starting one in the background if none answers.
async fn connect(space_dir: &Path, host: Option<&str>) -> Result<Client> {
    let client = Client::new(socket_path(space_dir));
    if client.space().await.is_ok() {
        return Ok(client);
    }
    if !space_dir.join("FORMAT").exists() {
        bail!(
            "{} is not a peerfs space; create one with `peerfs init`",
            space_dir.display()
        );
    }
    let log_path = space_dir.join("serve.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut process = Process::new(std::env::current_exe().context("finding this program")?);
    process.arg("--space").arg(space_dir);
    if let Some(host) = host {
        process.arg("--host").arg(host);
    }
    let mut child = process
        .args(["serve", "--idle-timeout"])
        .arg(AUTOSTART_IDLE.as_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("starting a server")?;
    let started = Instant::now();
    loop {
        match client.space().await {
            Ok(_) => return Ok(client),
            Err(ClientError::Connect { .. }) => {}
            Err(other) => return Err(other.into()),
        }
        if let Some(status) = child.try_wait()? {
            // Another command may have started a server first; this one then finds the
            // space in use and exits.
            if client.space().await.is_ok() {
                return Ok(client);
            }
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            let last = log.lines().last().unwrap_or("no output");
            bail!("the server exited ({status}): {last}");
        }
        if started.elapsed() > AUTOSTART_WAIT {
            bail!(
                "the server did not answer within {}s; see {}",
                AUTOSTART_WAIT.as_secs(),
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn absolute(path: &Path) -> Result<String> {
    let path =
        std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    path.to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

async fn client_command(
    client: &Client,
    command: Command,
    caller: &Caller,
    out: &mut impl Write,
) -> Result<ExitCode> {
    match command {
        Command::Init | Command::Serve { .. } | Command::Stop => {
            unreachable!("handled without a client")
        }
        Command::Import {
            source,
            branch,
            message,
        } => {
            let head = client
                .import(&absolute(&source)?, &branch, &message, caller)
                .await?;
            writeln!(out, "{}", head.head)?;
        }
        Command::Export {
            target,
            branch,
            commit,
        } => {
            let from = commit.map_or(Revision::Branch(branch), Revision::Commit);
            client.export(&absolute(&target)?, from).await?;
        }
        Command::Branches => {
            for branch in client.branches().await? {
                writeln!(out, "{} {}", branch.head, branch.name)?;
            }
        }
        Command::Pull { from, branch } => return pull(client, &from, &branch, out).await,
        Command::Push { to, branch, from } => {
            let from = from.unwrap_or_else(|| branch.clone());
            return push(client, &to, &branch, &from, out).await;
        }
        Command::Remotes => {
            for tracking in client.remotes().await? {
                writeln!(
                    out,
                    "{} {}/{} pulled at {}",
                    tracking.head,
                    tracking.remote,
                    tracking.branch,
                    tracking.pulled_ms / 1000
                )?;
            }
        }
        Command::Mounts => {
            for mount in client.mounts().await? {
                writeln!(out, "{} {}", mount.workspace, mount.path)?;
            }
        }
        Command::Key => {
            let space = client.space().await?;
            let key = space.key.context("this space does not sign its commits")?;
            writeln!(out, "{} {key}", space.host)?;
        }
        Command::Hosts { command: None } => {
            for host in client.hosts().await? {
                writeln!(out, "{} {}", host.host, host.key)?;
            }
        }
        Command::Hosts {
            command: Some(HostsCommand::Add { host, key }),
        } => {
            client.add_host(&host, &key).await?;
        }
        Command::Fork { name, from } => {
            let branch = client.fork(&name, &from).await?;
            writeln!(out, "{} {}", branch.head, branch.name)?;
        }
        Command::Log { branch, limit } => {
            for commit in client.log(&branch, limit).await? {
                let who = match &commit.agent {
                    Some(agent) => format!("{agent}@{}", commit.host),
                    None => commit.host.clone(),
                };
                let unsigned = if commit.signed_by.is_none() {
                    ", unsigned"
                } else {
                    ""
                };
                writeln!(
                    out,
                    "{}\n  {who} at {}{unsigned}\n  {}",
                    commit.id,
                    commit.time_ms / 1000,
                    commit.message
                )?;
                if let Some(predecessor) = &commit.predecessor {
                    writeln!(out, "  replaces snapshot {predecessor}")?;
                }
            }
        }
        Command::Workspace(command) => return workspace(client, command, caller, out).await,
    }
    Ok(ExitCode::SUCCESS)
}

/// `ws snapshots`: one line per snapshot, newest first.
async fn print_snapshots(client: &Client, name: &str, out: &mut impl Write) -> Result<()> {
    for snapshot in client.snapshots(name).await? {
        writeln!(
            out,
            "{} at {} {}",
            snapshot.id,
            snapshot.time_ms / 1000,
            snapshot.message
        )?;
    }
    Ok(())
}

/// `ws commit`: exits 1 on a conflict.
async fn commit(
    client: &Client,
    name: &str,
    message: &str,
    caller: &Caller,
    out: &mut impl Write,
) -> Result<ExitCode> {
    match client.commit(name, message, caller).await? {
        CommitResponse::Committed { commit, merged } => {
            let how = if merged {
                " (merged onto the moved branch)"
            } else {
                ""
            };
            writeln!(out, "{commit}{how}")?;
        }
        CommitResponse::Unchanged => writeln!(out, "nothing to commit")?,
        CommitResponse::Conflicted { conflicts } => {
            for conflict in &conflicts {
                writeln!(out, "conflict: {}: {}", conflict.path, conflict.kind)?;
            }
            writeln!(
                out,
                "nothing committed; resolve the conflicts in {name} and commit again"
            )?;
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn workspace(
    client: &Client,
    command: WorkspaceCommand,
    caller: &Caller,
    out: &mut impl Write,
) -> Result<ExitCode> {
    match command {
        WorkspaceCommand::Create { name, branch } => {
            let workspace = client.create_workspace(&name, &branch).await?;
            writeln!(
                out,
                "{} follows {} at {}",
                workspace.name, workspace.branch, workspace.base
            )?;
        }
        WorkspaceCommand::List => {
            for workspace in client.workspaces().await? {
                writeln!(
                    out,
                    "{} {} {}",
                    workspace.name, workspace.branch, workspace.base
                )?;
            }
        }
        WorkspaceCommand::Read { name, path } => {
            out.write_all(&client.read(&name, &path).await?)?;
        }
        WorkspaceCommand::Write { name, path, file } => {
            let bytes = if let Some(file) = file {
                fs::read(&file).with_context(|| format!("reading {}", file.display()))?
            } else {
                let mut bytes = Vec::new();
                io::stdin().read_to_end(&mut bytes)?;
                bytes
            };
            client.write(&name, &path, bytes).await?;
        }
        WorkspaceCommand::Rm { name, path } => client.remove(&name, &path).await?,
        WorkspaceCommand::Ls { name, path } => {
            for entry in client.list(&name, &path).await? {
                let suffix = match entry.kind {
                    EntryKind::Directory => "/",
                    EntryKind::Symlink => "@",
                    EntryKind::File => "",
                };
                writeln!(out, "{}{suffix}", entry.name)?;
            }
        }
        WorkspaceCommand::Status { name } => {
            for change in client.changes(&name).await? {
                match change {
                    ChangeInfo::Added { path } => writeln!(out, "A {path}")?,
                    ChangeInfo::Written { path } => writeln!(out, "M {path}")?,
                    ChangeInfo::Removed { path } => writeln!(out, "D {path}")?,
                    ChangeInfo::Kept { path, class } => {
                        writeln!(out, "  {path} (kept local: {class})")?;
                    }
                }
            }
        }
        WorkspaceCommand::Mount { name, dir } => {
            let mount = client.mount(&name, &absolute(&dir)?).await?;
            writeln!(out, "{} mounted at {}", mount.workspace, mount.path)?;
        }
        WorkspaceCommand::Unmount { name } => client.unmount(&name).await?,
        WorkspaceCommand::Snapshot { name, message } => {
            if let SnapshotResponse::Snapshot { snapshot } =
                client.snapshot(&name, message.as_deref(), caller).await?
            {
                writeln!(out, "{snapshot}")?;
            }
        }
        WorkspaceCommand::Snapshots { name } => print_snapshots(client, &name, out).await?,
        WorkspaceCommand::Commit { name, message } => {
            return commit(client, &name, &message, caller, out).await;
        }
        WorkspaceCommand::Merge { name, revision } => {
            match client.merge(&name, &revision, caller).await? {
                MergeResponse::UpToDate => writeln!(out, "already up to date")?,
                MergeResponse::FastForward { commit } => {
                    writeln!(out, "{commit} (fast-forward)")?;
                }
                MergeResponse::Merged { commit } => writeln!(out, "{commit} (merged)")?,
                MergeResponse::Conflicted { conflicts } => {
                    for conflict in &conflicts {
                        writeln!(out, "conflict: {}: {}", conflict.path, conflict.kind)?;
                    }
                    writeln!(
                        out,
                        "nothing committed; resolve the conflicts in {name} and commit to finish \
                         the merge"
                    )?;
                    return Ok(ExitCode::from(1));
                }
            }
        }
        WorkspaceCommand::Discard { name } => client.discard(&name).await?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The socket of the space in `dir`, starting its server if it is not running.
async fn other_socket(dir: &Path) -> Result<String> {
    let dir = std::path::absolute(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let other = connect(&dir, None).await?;
    Ok(other
        .socket()
        .to_str()
        .with_context(|| format!("{} is not UTF-8", dir.display()))?
        .to_owned())
}

async fn pull(
    client: &Client,
    from: &Path,
    branch: &str,
    out: &mut impl Write,
) -> Result<ExitCode> {
    let pulled = client.pull(&other_socket(from).await?, branch).await?;
    writeln!(
        out,
        "{} {}/{} ({} objects, {} bytes fetched)",
        pulled.head, pulled.remote, pulled.branch, pulled.objects, pulled.bytes
    )?;
    Ok(ExitCode::SUCCESS)
}

async fn push(
    client: &Client,
    to: &Path,
    branch: &str,
    from: &str,
    out: &mut impl Write,
) -> Result<ExitCode> {
    match client.push(&other_socket(to).await?, branch, from).await {
        Ok(pushed) => {
            writeln!(
                out,
                "{} {}/{} ({} objects, {} bytes sent)",
                pushed.head, pushed.remote, pushed.branch, pushed.objects, pushed.bytes
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) if error.code() == Some("conflict") => {
            writeln!(out, "refused: {error}")?;
            Ok(ExitCode::from(1))
        }
        Err(error) => Err(error.into()),
    }
}

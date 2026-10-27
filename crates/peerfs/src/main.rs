//! `peerfs`: create spaces, import trees, and work in workspaces from the command line.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use peerfs_core::{Author, Change, CommitOutcome, EntryKind, Merged, ObjectId, Space};

#[derive(Debug, Parser)]
#[command(version, about = "A filesystem for agents across machines")]
struct Cli {
    /// The space's directory.
    #[arg(long, env = "PEERFS_SPACE", global = true, default_value = ".peerfs")]
    space: PathBuf,
    /// The host recorded on commits; defaults to this machine's hostname.
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
        commit: Option<ObjectId>,
    },
    /// List branches and their heads.
    Branches,
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
    /// Commit the workspace's changes to its branch.
    Commit {
        name: String,
        #[arg(short, long)]
        message: String,
    },
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "peerfs: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let author = Author {
        host: cli.host.unwrap_or_else(hostname),
        agent: cli.agent,
        session: cli.session,
    };
    let mut out = io::stdout().lock();
    if let Command::Init = cli.command {
        Space::init(&cli.space, &author)
            .with_context(|| format!("creating a space in {}", cli.space.display()))?;
        writeln!(out, "created {}", cli.space.display())?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut space =
        Space::open(&cli.space).with_context(|| format!("opening {}", cli.space.display()))?;
    match cli.command {
        Command::Init => unreachable!("handled above"),
        Command::Import {
            source,
            branch,
            message,
        } => {
            let tree = space
                .import(&source)
                .with_context(|| format!("importing {}", source.display()))?;
            let commit = space.commit_tree(&branch, tree, &message, &author)?;
            writeln!(out, "{commit}")?;
        }
        Command::Export {
            target,
            branch,
            commit,
        } => {
            let commit = match commit {
                Some(commit) => commit,
                None => head(&space, &branch)?,
            };
            space.export(space.commit(commit)?.tree, &target)?;
        }
        Command::Branches => {
            for (name, head) in space.branches()? {
                writeln!(out, "{head} {name}")?;
            }
        }
        Command::Fork { name, from } => {
            let at = head(&space, &from)?;
            space.create_branch(&name, at)?;
            writeln!(out, "{at} {name}")?;
        }
        Command::Log { branch, limit } => {
            let history = space.history(head(&space, &branch)?)?;
            for (id, commit) in history.iter().take(limit.unwrap_or(usize::MAX)) {
                let seconds = commit
                    .time
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs());
                let who = match &commit.author.agent {
                    Some(agent) => format!("{agent}@{}", commit.author.host),
                    None => commit.author.host.clone(),
                };
                writeln!(out, "{id}\n  {who} at {seconds}\n  {}", commit.message)?;
            }
        }
        Command::Workspace(command) => {
            return workspace(&mut space, command, &author, &mut out);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn head(space: &Space, branch: &str) -> Result<ObjectId> {
    match space.branch(branch)? {
        Some(head) => Ok(head),
        None => bail!("no branch {branch}"),
    }
}

fn workspace(
    space: &mut Space,
    command: WorkspaceCommand,
    author: &Author,
    out: &mut impl Write,
) -> Result<ExitCode> {
    match command {
        WorkspaceCommand::Create { name, branch } => {
            let workspace = space.create_workspace(&name, &branch)?;
            writeln!(out, "{} follows {} at {}", name, branch, workspace.base())?;
        }
        WorkspaceCommand::List => {
            for name in space.workspaces()? {
                let workspace = space.workspace(&name)?;
                writeln!(out, "{name} {} {}", workspace.branch(), workspace.base())?;
            }
        }
        WorkspaceCommand::Read { name, path } => {
            out.write_all(&space.workspace(&name)?.read(space, &path)?)?;
        }
        WorkspaceCommand::Write { name, path, file } => {
            let bytes = if let Some(file) = file {
                fs::read(&file).with_context(|| format!("reading {}", file.display()))?
            } else {
                let mut bytes = Vec::new();
                io::stdin().read_to_end(&mut bytes)?;
                bytes
            };
            space.workspace(&name)?.write(space, &path, &bytes)?;
        }
        WorkspaceCommand::Rm { name, path } => {
            space.workspace(&name)?.remove(space, &path)?;
        }
        WorkspaceCommand::Ls { name, path } => {
            for (entry, kind) in space.workspace(&name)?.list(space, &path)? {
                let suffix = match kind {
                    EntryKind::Directory => "/",
                    EntryKind::Symlink => "@",
                    EntryKind::File => "",
                };
                writeln!(out, "{entry}{suffix}")?;
            }
        }
        WorkspaceCommand::Status { name } => {
            for change in space.workspace(&name)?.changes()? {
                match change {
                    Change::Written(path) => writeln!(out, "M {path}")?,
                    Change::Removed(path) => writeln!(out, "D {path}")?,
                }
            }
        }
        WorkspaceCommand::Commit { name, message } => {
            let mut workspace = space.workspace(&name)?;
            match space.commit_workspace(&mut workspace, &message, author)? {
                CommitOutcome::Committed { commit, merged } => {
                    let how = match merged {
                        Merged::No => "",
                        Merged::Yes => " (merged onto the moved branch)",
                    };
                    writeln!(out, "{commit}{how}")?;
                }
                CommitOutcome::Unchanged => writeln!(out, "nothing to commit")?,
                CommitOutcome::Conflicted(conflicts) => {
                    for conflict in &conflicts {
                        writeln!(out, "conflict: {conflict}")?;
                    }
                    writeln!(
                        out,
                        "nothing committed; resolve the conflicts in {name} and commit again"
                    )?;
                    return Ok(ExitCode::from(1));
                }
                _ => bail!("unexpected commit outcome"),
            }
        }
        WorkspaceCommand::Discard { name } => space.discard_workspace(&name)?,
    }
    Ok(ExitCode::SUCCESS)
}

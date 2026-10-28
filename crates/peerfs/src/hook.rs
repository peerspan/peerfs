//! `peerfs hook claude-code`: snapshot the workspaces a Claude Code tool call touched.
//!
//! Run as a `PostToolUse` hook, it reads the hook's JSON on standard input and takes the
//! session's working directory and any path in the tool's input. Each that lies in a
//! workspace mounted by peerfs gets a snapshot of that workspace, so every tool call,
//! shell commands included, leaves a step in the change's history. Paths outside any peerfs
//! mount need nothing, and the hook then exits without contacting a server.
//!
//! The space is `--space` or `PEERFS_SPACE`, and its server must be the one serving the
//! mount. The snapshot records the agent (`PEERFS_AGENT`, or `claude-code`) and the session
//! (`PEERFS_SESSION`, or Claude Code's session ID).

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use peerfs_api::{Caller, Client, ClientError, SnapshotResponse};
use peerfs_server::socket_path;
use serde::Deserialize;
use serde_json::Value;

/// The agent recorded when `PEERFS_AGENT` is not set.
const AGENT: &str = "claude-code";

/// The longest detail a snapshot message quotes from a tool's input, in characters.
const DETAIL: usize = 72;

/// The tool input fields that name a path, as Claude Code's tools spell them.
const PATH_FIELDS: [&str; 3] = ["file_path", "notebook_path", "path"];

/// The tool input fields worth quoting in a snapshot message, in order of preference.
const DETAIL_FIELDS: [&str; 6] = [
    "command",
    "file_path",
    "notebook_path",
    "path",
    "pattern",
    "url",
];

/// What Claude Code sends a `PostToolUse` hook. Fields this hook does not use are ignored,
/// so newer versions of the input still parse.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolCall {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: Value,
}

/// A workspace mounted by peerfs, as the kernel lists it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MountPoint {
    pub(crate) dir: PathBuf,
    pub(crate) workspace: String,
    /// Over loopback NFS, which another server could also export, so it is peerfs's only
    /// if the space's server says so.
    pub(crate) nfs: bool,
}

/// Snapshots each workspace mounted at a path the tool call touched. Prints nothing; an
/// error goes to standard error and exits 1, which Claude Code shows without stopping the
/// agent.
pub(crate) async fn claude_code(
    space_dir: &Path,
    caller: Caller,
    input: &mut impl Read,
    err: &mut impl Write,
) -> Result<()> {
    let mut text = String::new();
    input
        .read_to_string(&mut text)
        .context("reading the hook's input")?;
    let call: ToolCall = serde_json::from_str(&text).context("parsing the hook's input")?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let touched = touched(&call, &peerfs_mounts(&mountinfo));
    if touched.is_empty() {
        return Ok(());
    }
    let client = Client::new(socket_path(space_dir));
    let served = match client.mounts().await {
        Ok(served) => served,
        Err(ClientError::Connect { .. }) => bail!(
            "no peerfs server answers for {}, so {} cannot be snapshotted; set PEERFS_SPACE \
             to the space that mounted it",
            space_dir.display(),
            touched[0].dir.display()
        ),
        Err(other) => return Err(other.into()),
    };
    let caller = Caller {
        agent: caller.agent.or_else(|| Some(AGENT.to_owned())),
        session: caller.session.or_else(|| call.session_id.clone()),
    };
    for mount in &touched {
        let ours = served
            .iter()
            .any(|info| info.workspace == mount.workspace && Path::new(&info.path) == mount.dir);
        if !ours && mount.nfs {
            continue;
        }
        if !ours {
            bail!(
                "{} is not mounted by the space {}; set PEERFS_SPACE to the space that mounted it",
                mount.dir.display(),
                space_dir.display()
            );
        }
        let message = message(&call, &mount.dir);
        if let SnapshotResponse::Snapshot { snapshot } = client
            .snapshot(&mount.workspace, Some(&message), &caller)
            .await
            .with_context(|| format!("snapshotting {}", mount.workspace))?
        {
            // Claude Code shows a hook's standard error only on failure or in verbose
            // mode, so this is quiet in normal use.
            writeln!(err, "peerfs: snapshot {snapshot} of {}", mount.workspace)?;
        }
    }
    Ok(())
}

/// The peerfs mounts in `/proc/self/mountinfo`: FUSE mounts whose source is `peerfs`, the
/// workspace being the subtype, and NFS mounts of `127.0.0.1:/<workspace>`.
pub(crate) fn peerfs_mounts(mountinfo: &str) -> Vec<MountPoint> {
    mountinfo
        .lines()
        .filter_map(|line| {
            // `id parent dev root mountpoint options [optional...] - fstype source super`
            let (before, after) = line.split_once(" - ")?;
            let dir = PathBuf::from(unescape(before.split(' ').nth(4)?));
            let mut after = after.split(' ');
            let (fstype, source) = (after.next()?, after.next()?);
            if let Some(workspace) = fstype.strip_prefix("fuse.") {
                return (source == "peerfs").then(|| MountPoint {
                    dir,
                    workspace: unescape(workspace),
                    nfs: false,
                });
            }
            let workspace = source.strip_prefix("127.0.0.1:/")?;
            (fstype == "nfs" && !workspace.is_empty() && !workspace.contains('/')).then(|| {
                MountPoint {
                    dir,
                    workspace: unescape(workspace),
                    nfs: true,
                }
            })
        })
        .collect()
}

/// Undoes mountinfo's octal escapes (`\040` for a space, and so on).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|digits| {
            let text = std::str::from_utf8(digits).ok()?;
            u8::from_str_radix(text, 8).ok()
        });
        match (bytes[i], octal) {
            (b'\\', Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mounts holding the call's working directory or a path in its input, each once.
pub(crate) fn touched(call: &ToolCall, mounts: &[MountPoint]) -> Vec<MountPoint> {
    let cwd = call.cwd.clone().unwrap_or_default();
    let mut paths = vec![cwd.clone()];
    for field in PATH_FIELDS {
        if let Some(path) = call.tool_input.get(field).and_then(Value::as_str) {
            paths.push(cwd.join(path));
        }
    }
    let found: BTreeSet<MountPoint> = paths
        .iter()
        .filter(|path| path.is_absolute())
        .filter_map(|path| {
            mounts
                .iter()
                .filter(|mount| path.starts_with(&mount.dir))
                .max_by_key(|mount| mount.dir.components().count())
                .cloned()
        })
        .collect();
    found.into_iter().collect()
}

/// A one-line snapshot message: the tool, a short detail from its input, and the tool
/// use's ID, so the step can be found in the session's transcript. A path in the detail is
/// given relative to the mount.
pub(crate) fn message(call: &ToolCall, mount: &Path) -> String {
    let tool = call.tool_name.as_deref().unwrap_or("tool");
    let detail = DETAIL_FIELDS.iter().find_map(|field| {
        let text = call.tool_input.get(*field)?.as_str()?;
        let text = if PATH_FIELDS.contains(field) {
            Path::new(text)
                .strip_prefix(mount)
                .map_or(text, |relative| relative.to_str().unwrap_or(text))
        } else {
            text
        };
        let line = text.lines().next().unwrap_or_default().trim();
        (!line.is_empty()).then(|| shorten(line))
    });
    let message = match detail {
        Some(detail) => format!("{tool}: {detail}"),
        None => tool.to_owned(),
    };
    match &call.tool_use_id {
        Some(id) => format!("{message} ({id})"),
        None => message,
    }
}

fn shorten(text: &str) -> String {
    match text.char_indices().nth(DETAIL) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(json: &str) -> ToolCall {
        serde_json::from_str(json).expect("valid hook input")
    }

    const MOUNTINFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:50 / /work/agent\\0401 rw,nosuid,nodev,relatime shared:20 - fuse.agent-1 peerfs rw,user_id=0
41 40 0:51 / /work/agent\\0401/vendor rw shared:21 - fuse.vendor peerfs rw,user_id=0
42 22 0:52 / /other rw shared:22 - fuse.sshfs remote:/x rw,user_id=0
43 22 0:53 / /work/nfs rw,relatime shared:23 - nfs 127.0.0.1:/agent-2 rw,vers=3,port=40285
44 22 0:54 / /work/remote rw,relatime shared:24 - nfs files.lan:/agent-3 rw,vers=3
45 22 0:55 / /work/deep rw,relatime shared:25 - nfs 127.0.0.1:/srv/data rw,vers=3
";

    #[test]
    fn mountinfo_lists_only_peerfs_mounts() {
        assert_eq!(
            peerfs_mounts(MOUNTINFO),
            [
                MountPoint {
                    dir: PathBuf::from("/work/agent 1"),
                    workspace: "agent-1".to_owned(),
                    nfs: false,
                },
                MountPoint {
                    dir: PathBuf::from("/work/agent 1/vendor"),
                    workspace: "vendor".to_owned(),
                    nfs: false,
                },
                MountPoint {
                    dir: PathBuf::from("/work/nfs"),
                    workspace: "agent-2".to_owned(),
                    nfs: true,
                },
            ]
        );
    }

    #[test]
    fn a_call_touches_the_innermost_mount_of_each_path() {
        let mounts = peerfs_mounts(MOUNTINFO);
        let names = |json: &str| -> Vec<String> {
            touched(&call(json), &mounts)
                .into_iter()
                .map(|mount| mount.workspace)
                .collect()
        };
        assert_eq!(
            names(r#"{"cwd": "/work/agent 1/src", "tool_input": {"command": "make"}}"#),
            ["agent-1"]
        );
        assert_eq!(
            names(r#"{"cwd": "/work/agent 1", "tool_input": {"file_path": "vendor/a.rs"}}"#),
            ["agent-1", "vendor"]
        );
        assert_eq!(
            names(r#"{"cwd": "/home/me", "tool_input": {"file_path": "/work/agent 1/a"}}"#),
            ["agent-1"]
        );
        assert!(
            names(r#"{"cwd": "/work/agent 10"}"#).is_empty(),
            "by component"
        );
        assert!(names(r#"{"cwd": "/other"}"#).is_empty());
        assert!(names("{}").is_empty());
    }

    #[test]
    fn messages_name_the_tool_and_what_it_acted_on() {
        let mount = Path::new("/work/w");
        let message = |json: &str| super::message(&call(json), mount);
        assert_eq!(
            message(
                r#"{"tool_name": "Bash", "tool_use_id": "toolu_1",
                    "tool_input": {"command": "cargo build\ncargo test"}}"#
            ),
            "Bash: cargo build (toolu_1)"
        );
        assert_eq!(
            message(r#"{"tool_name": "Edit", "tool_input": {"file_path": "/work/w/src/a.rs"}}"#),
            "Edit: src/a.rs"
        );
        assert_eq!(message(r#"{"tool_name": "TodoWrite"}"#), "TodoWrite");
        let long = format!(
            r#"{{"tool_name": "Bash", "tool_input": {{"command": "{}"}}}}"#,
            "é".repeat(80)
        );
        assert_eq!(message(&long), format!("Bash: {}...", "é".repeat(72)));
    }
}

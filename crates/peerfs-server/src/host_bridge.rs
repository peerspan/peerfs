//! The bridge to the Peerspan host for host-backed spaces: its owner-local API signs their
//! commits with the host key, and holds the binding each was prepared under.
//!
//! Requests go over the host's owner-local Unix socket, named by the space's binding. Each
//! is one HTTP/1.1 exchange, bounded to 5 seconds and an 8 KiB response, so a host that
//! hangs or misbehaves fails the commit rather than the server.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use peerfs_core::{
    Error, HostBinding, HostKeys, MAX_SIGNING_MESSAGE, PublicKey, Result, SIGNATURE_LEN, Signer,
};
use serde::Deserialize;

/// How long one request to the host may take, from connecting to the last byte.
const DEADLINE: Duration = Duration::from_secs(5);

/// The largest response the host sends to these requests.
const MAX_RESPONSE: usize = 8 * 1024;

/// A client of the host's owner-local socket.
#[derive(Debug, Clone)]
pub struct HostClient {
    socket: PathBuf,
}

/// A binding as the host reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingView {
    /// Always 1.
    pub format: u8,
    /// The logical space.
    pub space_id: String,
    /// The registration lifetime.
    pub binding_id: String,
    /// The network.
    pub network_id: String,
    /// The network's context.
    pub context_id: String,
    /// The host that signs.
    pub host_id: String,
    /// Its public key, canonical standard Base64.
    pub public_key: String,
    /// The space's directory.
    pub directory: String,
    /// `prepared`, `registered` or `released`.
    pub state: String,
    /// When the binding was prepared.
    pub created_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    host_id: String,
    public_key: String,
    signature: String,
}

#[derive(Deserialize)]
struct Refusal {
    code: String,
}

fn unavailable(what: impl fmt::Display) -> Error {
    Error::Signing(format!("the host is unavailable: {what}"))
}

impl HostClient {
    /// A client of the host listening at `socket`.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The socket this client talks to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Sends one request and returns the response's status and body.
    fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>)> {
        let started = Instant::now();
        let remaining = || {
            DEADLINE
                .checked_sub(started.elapsed())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| unavailable("it did not answer in time"))
        };
        let mut stream = connect(&self.socket, started)?;
        stream
            .set_write_timeout(Some(remaining()?))
            .map_err(unavailable)?;
        let body = body.unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n"
        );
        if method == "POST" {
            use std::fmt::Write as _;
            let _ = write!(
                request,
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            );
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.write_all(body))
            .map_err(unavailable)?;
        let mut response = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            stream
                .set_read_timeout(Some(remaining()?))
                .map_err(unavailable)?;
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                    if response.len() > MAX_RESPONSE + 4096 {
                        return Err(unavailable("its response is too large"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(unavailable(e)),
            }
        }
        parse_response(&response)
    }

    /// The binding of `space_id`, as the host holds it.
    ///
    /// # Errors
    ///
    /// Fails if the host is unavailable, or does not know the space.
    pub fn binding(&self, space_id: &str) -> Result<BindingView> {
        let (status, body) = self.request("GET", &format!("/v1/fs/spaces/{space_id}"), None)?;
        match status {
            200 => serde_json::from_slice(&body)
                .map_err(|_| unavailable("its binding response is malformed")),
            404 => Err(Error::HostBacked(format!(
                "the host has no space {space_id}"
            ))),
            _ => Err(refused(status, &body)),
        }
    }

    /// Signs `message` under `binding`, returning the signature after checking the host
    /// answered for the binding's host and key.
    fn sign(&self, binding: &HostBinding, message: &[u8]) -> Result<[u8; SIGNATURE_LEN]> {
        if message.len() > MAX_SIGNING_MESSAGE {
            return Err(Error::Signing(
                "the commit is larger than the host signs".to_owned(),
            ));
        }
        let body = serde_json::json!({
            "binding_id": binding.binding_id,
            "message_base64": STANDARD.encode(message),
        });
        let (status, body) = self.request(
            "POST",
            &format!("/v1/fs/spaces/{}/sign-commit", binding.space_id),
            Some(body.to_string().as_bytes()),
        )?;
        if status != 200 {
            return Err(refused(status, &body));
        }
        let signed: Signed = serde_json::from_slice(&body)
            .map_err(|_| unavailable("its signing response is malformed"))?;
        if signed.host_id != binding.host_id || signed.public_key != binding.public_key.to_string()
        {
            return Err(Error::HostBacked(
                "the host signed with another identity than the binding's".to_owned(),
            ));
        }
        let bytes = STANDARD
            .decode(&signed.signature)
            .ok()
            .filter(|bytes| STANDARD.encode(bytes) == signed.signature)
            .and_then(|bytes| <[u8; SIGNATURE_LEN]>::try_from(bytes).ok())
            .ok_or_else(|| unavailable("its signature is malformed"))?;
        Ok(bytes)
    }

    /// Checks that the host holds `binding` as a live binding of this directory, with the
    /// same identity, before a space signs through it.
    ///
    /// # Errors
    ///
    /// Fails if the host is unavailable, or its binding differs from the space's.
    pub fn check(&self, binding: &HostBinding, directory: &Path) -> Result<()> {
        let view = self.binding(&binding.space_id)?;
        let matches = view.format == 1
            && view.space_id == binding.space_id
            && view.binding_id == binding.binding_id
            && view.network_id == binding.network_id
            && view.context_id == binding.context_id
            && view.host_id == binding.host_id
            && view.public_key == binding.public_key.to_string()
            && Path::new(&view.directory) == directory;
        if !matches {
            return Err(Error::HostBacked(
                "the host's binding of this space is not the one it was created under".to_owned(),
            ));
        }
        if view.state == "released" {
            return Err(Error::HostBacked(
                "the host has released this space's binding".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Connects to `socket` within what is left of the deadline from `started`. A blocking
/// connect could wait forever on a host that has stopped accepting connections and whose
/// backlog is full, while the space's lock is held for signing, so the socket connects
/// without blocking: a full backlog refuses at once with `EAGAIN`, retried until the
/// deadline, and a connection in progress is waited for.
fn connect(socket: &Path, started: Instant) -> Result<UnixStream> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::io::Errno;
    use rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect as connect_socket,
        socket_with, sockopt,
    };
    let address = SocketAddrUnix::new(socket).map_err(|e| unavailable(io::Error::from(e)))?;
    let fd = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| unavailable(io::Error::from(e)))?;
    let timed_out = || unavailable("it did not accept the connection in time");
    loop {
        match connect_socket(&fd, &address) {
            Ok(()) => break,
            Err(Errno::AGAIN | Errno::INTR) => {
                if started.elapsed() >= DEADLINE {
                    return Err(timed_out());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(Errno::INPROGRESS) => {
                let left = DEADLINE
                    .checked_sub(started.elapsed())
                    .ok_or_else(timed_out)?;
                let timeout = Timespec::try_from(left).map_err(|_| timed_out())?;
                let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
                if poll(&mut fds, Some(&timeout)).map_err(|e| unavailable(io::Error::from(e)))? == 0
                {
                    return Err(timed_out());
                }
                if let Err(e) =
                    sockopt::socket_error(&fd).map_err(|e| unavailable(io::Error::from(e)))?
                {
                    return Err(unavailable(io::Error::from(e)));
                }
                break;
            }
            Err(e) => return Err(unavailable(io::Error::from(e))),
        }
    }
    rustix::io::ioctl_fionbio(&fd, false).map_err(|e| unavailable(io::Error::from(e)))?;
    Ok(UnixStream::from(fd))
}

/// What a refusal from the host means for a commit.
fn refused(status: u16, body: &[u8]) -> Error {
    let code = serde_json::from_slice::<Refusal>(body)
        .map(|refusal| refusal.code)
        .unwrap_or_default();
    match status {
        503 => unavailable("it could not sign now"),
        409 => {
            Error::HostBacked("the host refused: the binding was released or changed".to_owned())
        }
        _ => Error::Signing(format!("the host refused with {status} {code}")),
    }
}

/// Splits an HTTP/1.1 response into its status and body, honouring `Content-Length`.
fn parse_response(response: &[u8]) -> Result<(u16, Vec<u8>)> {
    let malformed = || unavailable("its response is malformed");
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(malformed)?;
    let head = std::str::from_utf8(&response[..split]).map_err(|_| malformed())?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|line| line.strip_prefix("HTTP/1.1 "))
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .ok_or_else(malformed)?;
    let mut body = response[split + 4..].to_vec();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                let len: usize = value.trim().parse().map_err(|_| malformed())?;
                if body.len() < len {
                    return Err(malformed());
                }
                body.truncate(len);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                return Err(malformed());
            }
        }
    }
    if body.len() > MAX_RESPONSE {
        return Err(unavailable("its response is too large"));
    }
    Ok((status, body))
}

/// Signs a host-backed space's commits through its host.
pub struct HostSigner {
    client: HostClient,
    binding: HostBinding,
}

impl HostSigner {
    /// A signer for the space bound as `binding`, through the host at its socket.
    pub fn new(binding: HostBinding) -> Self {
        Self {
            client: HostClient::new(binding.host_socket.clone()),
            binding,
        }
    }
}

impl fmt::Debug for HostSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostSigner")
            .field("space", &self.binding.space_id)
            .field("host", &self.binding.host_id)
            .finish_non_exhaustive()
    }
}

impl Signer for HostSigner {
    fn public_key(&self) -> PublicKey {
        self.binding.public_key
    }

    fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_LEN]> {
        self.client.sign(&self.binding, message)
    }
}

/// The host keys a host-backed space knows: its own host's, from its binding. Checking
/// another host's key needs the host's identity history, which comes with activation;
/// until then a commit from another host is refused rather than trusted.
#[derive(Debug)]
pub struct BindingHostKeys {
    host_id: String,
    key: PublicKey,
}

impl BindingHostKeys {
    /// The keys of the host `binding` names.
    pub fn new(binding: &HostBinding) -> Self {
        Self {
            host_id: binding.host_id.clone(),
            key: binding.public_key,
        }
    }
}

impl HostKeys for BindingHostKeys {
    fn is_key_of(&self, host: &str, key: &PublicKey) -> Result<bool> {
        if host == self.host_id {
            return Ok(*key == self.key);
        }
        Err(Error::HostBacked(
            "checking another host's key through the host is not available yet".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_are_parsed_by_content_length() {
        let (status, body) =
            parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}extra").unwrap();
        assert_eq!((status, body.as_slice()), (200, b"{}".as_slice()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\n{}").is_err());
        assert!(parse_response(b"garbage").is_err());
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\n{}")
                .is_err()
        );
    }

    /// A host that stops accepting with its backlog full: connecting fails within the
    /// deadline instead of hanging.
    #[test]
    fn a_host_with_a_full_backlog_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.sock");
        // A listener with a backlog of one, which never accepts.
        let listener = rustix::net::socket(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            None,
        )
        .unwrap();
        rustix::net::bind(&listener, &rustix::net::SocketAddrUnix::new(&path).unwrap()).unwrap();
        rustix::net::listen(&listener, 1).unwrap();
        // Fill the backlog with connections the listener never accepts.
        let mut held = Vec::new();
        while let Ok(stream) = connect(&path, Instant::now()) {
            held.push(stream);
            assert!(held.len() < 100, "the backlog never filled");
        }
        let started = Instant::now();
        let client = HostClient::new(&path);
        assert!(client.binding("space_x").is_err());
        let took = started.elapsed();
        assert!(
            took >= DEADLINE && took < DEADLINE + Duration::from_secs(2),
            "{took:?}"
        );
    }

    #[test]
    fn an_unreachable_host_is_unavailable() {
        let client = HostClient::new("/nonexistent/admin.sock");
        assert!(matches!(client.binding("space_x"), Err(Error::Signing(_))));
    }
}

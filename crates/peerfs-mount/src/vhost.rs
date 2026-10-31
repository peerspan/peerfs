//! A workspace served to virtual machines over vhost-user-fs, the protocol `virtiofsd`
//! speaks, so any VMM that takes an external `virtiofsd` socket can use it: libkrun, QEMU,
//! Cloud Hypervisor and crosvm among them.
//!
//! The VMM connects to a Unix socket this process listens on and hands over the guest's
//! memory and virtqueues; the guest's virtio-fs driver sends FUSE requests through them,
//! which the same [`WorkspaceFs`] the FUSE mount uses answers. So the guest sees the
//! workspace as the host's mounts do, and per-call snapshots and commits work as there.
//!
//! - One VM at a time: when it disconnects, the next can connect; stopping is refused while
//!   one is connected.
//! - Every file shows as owned by the guest's root, who runs the guest's commands; a
//!   workspace has no owners of its own.
//! - The tag is set by the VMM, or read from the device's configuration by those that ask.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::transport::{Reader, VirtioFsWriter};
use vhost::vhost_user::Listener;
use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackend, VhostUserDaemon, VringMutex, VringT};
use virtio_bindings::bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::bindings::virtio_ring::{
    VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC,
};
use virtio_queue::QueueT;
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::event::{
    EventConsumer, EventFlag, EventNotifier, new_event_consumer_and_notifier,
};

use crate::{WorkspaceFs, lock};

/// A high-priority queue and one request queue, as `virtiofsd` offers.
const QUEUES: usize = 2;
const QUEUE_SIZE: usize = 1024;

/// The length of the tag in a virtio-fs device's configuration.
const TAG_LEN: usize = 36;

type Memory = GuestMemoryAtomic<GuestMemoryMmap>;

/// The device: the file system, the guest's memory once the VMM has sent it, and the
/// configuration the guest reads.
struct FsDevice {
    server: Server<Arc<WorkspaceFs>>,
    memory: Mutex<Option<Memory>>,
    event_idx: AtomicBool,
    config: Vec<u8>,
}

impl FsDevice {
    fn new(fs: Arc<WorkspaceFs>, tag: &str) -> Self {
        // `struct virtio_fs_config`: the tag, NUL-padded, then the number of request queues.
        let mut config = vec![0; TAG_LEN];
        let tag = tag.as_bytes();
        config[..tag.len().min(TAG_LEN)].copy_from_slice(&tag[..tag.len().min(TAG_LEN)]);
        config.extend_from_slice(&u32::try_from(QUEUES - 1).unwrap_or(1).to_le_bytes());
        Self {
            server: Server::new(fs),
            memory: Mutex::new(None),
            event_idx: AtomicBool::new(false),
            config,
        }
    }

    /// Answers every request waiting on `vring`.
    fn process(&self, vring: &VringMutex) -> io::Result<()> {
        let memory = lock(&self.memory)
            .clone()
            .ok_or_else(|| io::Error::other("the VMM has not sent the guest's memory"))?;
        let event_idx = self.event_idx.load(Ordering::Relaxed);
        loop {
            if event_idx {
                vring.disable_notification().map_err(io::Error::other)?;
            }
            self.drain(vring, &memory)?;
            // With event indexes, a request may have arrived while notifications were off.
            if !event_idx || !vring.enable_notification().map_err(io::Error::other)? {
                return Ok(());
            }
        }
    }

    fn drain(&self, vring: &VringMutex, memory: &Memory) -> io::Result<()> {
        let guard = memory.memory();
        let mut answered = false;
        loop {
            let chain = vring
                .get_mut()
                .get_queue_mut()
                .pop_descriptor_chain(guard.clone());
            let Some(chain) = chain else { break };
            let head = chain.head_index();
            let reader = Reader::from_descriptor_chain(&*guard, chain.clone())
                .map_err(|e| io::Error::other(e.to_string()))?;
            let writer =
                VirtioFsWriter::new(&*guard, chain).map_err(|e| io::Error::other(e.to_string()))?;
            // A request that cannot be decoded is returned unanswered; the guest's driver
            // fails it.
            let written = self
                .server
                .handle_message(reader, writer.into(), None, None)
                .unwrap_or(0);
            vring
                .add_used(head, u32::try_from(written).unwrap_or(0))
                .map_err(io::Error::other)?;
            answered = true;
        }
        if answered && vring.needs_notification().map_err(io::Error::other)? {
            vring.signal_used_queue()?;
        }
        Ok(())
    }
}

impl VhostUserBackend for FsDevice {
    type Bitmap = ();
    type Vring = VringMutex;

    fn num_queues(&self) -> usize {
        QUEUES
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_RING_F_INDIRECT_DESC)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::REPLY_ACK
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
    }

    fn set_event_idx(&self, enabled: bool) {
        self.event_idx.store(enabled, Ordering::Relaxed);
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::try_from(size).unwrap_or(0));
        self.config
            .get(start..end)
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    }

    fn update_memory(&self, memory: Memory) -> io::Result<()> {
        *lock(&self.memory) = Some(memory);
        Ok(())
    }

    fn exit_event(&self, _thread_index: usize) -> Option<(EventConsumer, EventNotifier)> {
        new_event_consumer_and_notifier(EventFlag::NONBLOCK).ok()
    }

    fn handle_event(
        &self,
        device_event: u16,
        evset: EventSet,
        vrings: &[VringMutex],
        _thread_id: usize,
    ) -> io::Result<()> {
        if evset != EventSet::IN {
            return Err(io::Error::other(format!("unexpected event {evset:?}")));
        }
        let vring = vrings
            .get(usize::from(device_event))
            .ok_or_else(|| io::Error::other(format!("no queue {device_event}")))?;
        self.process(vring)
    }
}

/// A workspace served at a vhost-user socket by a thread of this process, one VM at a time.
#[derive(Debug)]
pub(crate) struct VhostUserMount {
    socket: PathBuf,
    /// The listening socket, shared with the serving thread, to wake it when stopping.
    listener: UnixListener,
    stopping: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VhostUserMount {
    /// Listens at `socket`, which must not exist, for VMMs to serve `fs` to, as `tag`.
    pub(crate) fn new(fs: WorkspaceFs, socket: &Path, tag: &str) -> io::Result<Self> {
        if socket.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists", socket.display()),
            ));
        }
        if tag.is_empty() || tag.len() > TAG_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a virtio-fs tag is 1 to {TAG_LEN} bytes"),
            ));
        }
        let fs = Arc::new(fs);
        let stopping = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        // Bound here, so the socket exists once this returns.
        let listener = UnixListener::bind(socket)?;
        let ours = listener.try_clone()?;
        let thread = {
            let tag = tag.to_owned();
            let (stopping, connected) = (Arc::clone(&stopping), Arc::clone(&connected));
            thread::Builder::new()
                .name("peerfs-vhost".to_owned())
                .spawn(move || serve(&fs, &tag, listener, &stopping, &connected))?
        };
        Ok(Self {
            socket: socket.to_owned(),
            listener: ours,
            stopping,
            connected,
            thread: Some(thread),
        })
    }

    /// Stops listening and removes the socket.
    ///
    /// Fails with [`io::ErrorKind::ResourceBusy`] while a VM is connected.
    pub(crate) fn stop(&mut self) -> io::Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        if self.connected.load(Ordering::SeqCst) {
            self.thread = Some(thread);
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "a virtual machine is connected",
            ));
        }
        self.stopping.store(true, Ordering::SeqCst);
        // Wake the thread waiting for a VMM, even if the socket's file is gone: shutting
        // the listening socket down fails its accept.
        // SAFETY: shutdown(2) on a socket this value owns; it has no other preconditions.
        unsafe { libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR) };
        let _ = thread.join();
        let _ = std::fs::remove_file(&self.socket);
        Ok(())
    }
}

/// Serves one VMM after another until told to stop.
fn serve(
    fs: &Arc<WorkspaceFs>,
    tag: &str,
    listener: UnixListener,
    stopping: &AtomicBool,
    connected: &AtomicBool,
) {
    let mut listener = Listener::from(listener);
    loop {
        let device = Arc::new(FsDevice::new(Arc::clone(fs), tag));
        let memory = GuestMemoryAtomic::new(GuestMemoryMmap::new());
        let Ok(mut daemon) = VhostUserDaemon::new("peerfs-vhost".to_owned(), device, memory) else {
            return;
        };
        if daemon.start(&mut listener).is_err() || stopping.load(Ordering::SeqCst) {
            return;
        }
        connected.store(true, Ordering::SeqCst);
        // Returns when the VMM disconnects.
        let _ = daemon.wait();
        for handler in daemon.get_epoll_handlers() {
            handler.send_exit_event();
        }
        connected.store(false, Ordering::SeqCst);
        if stopping.load(Ordering::SeqCst) {
            return;
        }
    }
}

//! A workspace served as a vhost-user-fs device, driven from the VMM's side of the protocol.
//! Booting a guest on it is checked with the runtime, not here.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use peerfs_core::{Author, Space};
use peerfs_mount::{WorkspaceFs, serve_vm};
use vhost::VhostBackend;
use vhost::vhost_user::message::{VhostUserConfigFlags, VhostUserVirtioFeatures};
use vhost::vhost_user::{Frontend as Vmm, VhostUserFrontend};

/// `VIRTIO_F_VERSION_1`.
const VERSION_1: u64 = 1 << 32;

#[test]
fn a_vmm_connects_to_a_lent_workspace_reads_the_tag_and_ends_the_service_by_leaving() {
    let dir = tempfile::tempdir().unwrap();
    let space_dir = dir.path().join("space");
    let mut space = Space::init(&space_dir, &Author::default()).unwrap();
    space.create_workspace("w", "main").unwrap();
    // Served from a lent view, as the confined process serves it, while the owner holds the
    // space.
    let lent = Arc::new(Mutex::new(Space::open_lent(&space_dir).unwrap()));
    let socket = dir.path().join("w.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let served =
        thread::spawn(move || serve_vm(WorkspaceFs::new(lent, "w"), listener, "workspace"));

    let mut vmm = Vmm::connect(&socket, 2).unwrap();
    vmm.set_owner().unwrap();
    let features = vmm.get_features().unwrap();
    assert_ne!(features & VERSION_1, 0, "virtio 1.0");
    vmm.set_features(features).unwrap();
    assert_ne!(
        features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits(),
        0
    );
    let protocol = vmm.get_protocol_features().unwrap();
    vmm.set_protocol_features(protocol).unwrap();
    assert_eq!(
        vmm.get_queue_num().unwrap(),
        2,
        "a high-priority and a request queue"
    );

    // `struct virtio_fs_config`: the tag, NUL-padded to 36 bytes, then the request queues.
    let (_, config) = vmm
        .get_config(0, 40, VhostUserConfigFlags::empty(), &[0; 40])
        .unwrap();
    assert_eq!(&config[..9], b"workspace");
    assert!(config[9..36].iter().all(|&byte| byte == 0));
    assert_eq!(u32::from_le_bytes(config[36..40].try_into().unwrap()), 1);

    assert!(!served.is_finished());
    drop(vmm);
    served.join().unwrap().unwrap();
    drop(space);
}

#[test]
fn a_tag_too_long_for_the_device_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &Author::default()).unwrap();
    space.create_workspace("w", "main").unwrap();
    let listener = UnixListener::bind(dir.path().join("w.sock")).unwrap();
    let fs = WorkspaceFs::new(Arc::new(Mutex::new(space)), "w");
    let error = serve_vm(fs, listener, &"t".repeat(37)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

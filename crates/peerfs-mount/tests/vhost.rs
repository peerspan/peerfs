//! A workspace served as a vhost-user-fs device, driven from the VMM's side of the protocol.
//! Booting a guest on it is checked with the runtime, not here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use peerfs_core::{Author, Space};
use peerfs_mount::{Frontend, Mount};
use vhost::VhostBackend;
use vhost::vhost_user::message::{VhostUserConfigFlags, VhostUserVirtioFeatures};
use vhost::vhost_user::{Frontend as Vmm, VhostUserFrontend};

/// `VIRTIO_F_VERSION_1`.
const VERSION_1: u64 = 1 << 32;

#[test]
fn a_vmm_connects_reads_the_tag_and_blocks_unmounting_while_connected() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &Author::default()).unwrap();
    space.create_workspace("w", "main").unwrap();
    let space = Arc::new(Mutex::new(space));
    let socket = dir.path().join("w.sock");
    let frontend = Frontend::VhostUser {
        tag: "workspace".to_owned(),
    };
    let mount = Mount::new(Arc::clone(&space), "w", &socket, frontend.clone()).unwrap();
    assert!(socket.exists());
    assert_eq!(mount.frontend(), frontend);

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

    // While the VMM is connected the device cannot go away; once it has, it can.
    let mut mount = mount;
    assert_eq!(
        mount.unmount().unwrap_err().kind(),
        std::io::ErrorKind::ResourceBusy
    );
    drop(vmm);
    unmount_soon(&mut mount);
    assert!(!socket.exists());
}

/// Unmounts, waiting for the device to notice that the VMM went away.
fn unmount_soon(mount: &mut Mount) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match mount.unmount() {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::ResourceBusy => {
                assert!(Instant::now() < deadline, "still busy");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn a_mount_unmounts_once_its_vmm_is_gone_even_without_its_socket() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &Author::default()).unwrap();
    space.create_workspace("w", "main").unwrap();
    let socket = dir.path().join("w.sock");
    let frontend = Frontend::VhostUser {
        tag: "w".to_owned(),
    };
    let mut mount = Mount::new(Arc::new(Mutex::new(space)), "w", &socket, frontend).unwrap();
    {
        let vmm = Vmm::connect(&socket, 2).unwrap();
        vmm.set_owner().unwrap();
    }
    // As when a VM's run directory is removed with the socket in it.
    std::fs::remove_file(&socket).unwrap();
    unmount_soon(&mut mount);
}

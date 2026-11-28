//! Remote access service and its same-user desktop administration interface.
pub mod access;
pub mod admin;
#[cfg(feature = "server")]
mod bridge;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub mod discovery;
pub mod host_profile;
pub mod host_updates;
#[cfg(feature = "server")]
pub mod inventory;
#[cfg(feature = "client")]
pub mod paired_hosts;
#[cfg(feature = "client")]
pub mod phone_link;
#[cfg(feature = "server")]
mod push;
pub mod serve;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod state;
#[cfg(feature = "server")]
pub mod worker_checkout;
#[cfg(feature = "server")]
pub use server::{run, run_with_jobs, run_with_receivers};
#[cfg(feature = "server")]
pub mod linked_jobs;
#[cfg(feature = "server")]
pub mod memory_sync;
#[cfg(feature = "server")]
pub mod move_manifest;
#[cfg(feature = "server")]
pub mod move_transport;
#[cfg(feature = "server")]
pub mod worker_result;

#[cfg(all(test, feature = "server"))]
mod fixture {
    #[test]
    #[ignore = "launched by the disposable browser fixture"]
    fn browser_service() {
        let dir =
            std::env::var_os("OX_TEST_REMOTE_DIR").expect("isolated browser test state directory");
        let root = std::path::PathBuf::from(dir);
        let store = root.join("managed-host-fixture");
        std::fs::create_dir(&store).unwrap();
        std::fs::write(store.join("headless"), []).unwrap();
        let updates = super::host_updates::Controller::open(&root, Some(&store), "1.0.0").unwrap();
        assert!(super::host_updates::UPDATES.set(updates).is_ok());
        super::run(root.clone(), root.join("mock-host.sock")).unwrap();
    }
}

#[cfg(feature = "server")]
mod cartridges;

#[cfg(any(feature = "client", feature = "server"))]
pub mod terminal_protocol;

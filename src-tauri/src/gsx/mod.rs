//! GSX Pro (Virtuali) integration: detection, the health post and the read
//! loop (`runner`), the frame block (`block`), and the capture mode
//! (`--gsx-record`) that records the local Remote API for fixtures.

pub mod block;
pub mod client;
pub mod detect;
pub mod port;
pub mod record;
pub mod runner;
pub mod setting;
pub mod state;

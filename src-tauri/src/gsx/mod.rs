//! GSX Pro (Virtuali) integration: detection, the health post and the read
//! loop (`runner`), the frame block (`block`), the EFB's writes
//! (`commands`), and the capture mode
//! (`--gsx-record`) that records the local Remote API for fixtures.

pub mod block;
pub mod client;
pub mod commands;
pub mod detect;
pub mod port;
pub mod record;
pub mod runner;
pub mod setting;
pub mod state;

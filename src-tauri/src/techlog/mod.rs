//! TechLog link: reads the local TechLog app (`/api/ping`, `/api/state`) and
//! turns its state into the snapshot the EFB server stores.
//!
//! Layout: `model` (the parts of TechLog's JSON we read), `snapshot` (our own
//! wire object and its hash), `client` (the local reads), `runner` (cadence,
//! change detection and the POST to SkyNexus), `presence` (the ping-only
//! check while the reader is off), `setting` (the tray switch kept on disk).

pub mod client;
pub mod model;
pub mod presence;
pub mod runner;
pub mod setting;
pub mod snapshot;

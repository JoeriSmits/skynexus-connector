//! TechLog link: reads the local TechLog app (`/api/ping`, `/api/state`) and
//! turns its state into the snapshot the EFB server stores.
//!
//! Layout: `model` (the parts of TechLog's JSON we read), `snapshot` (our own
//! wire object and its hash).

pub mod model;
pub mod snapshot;

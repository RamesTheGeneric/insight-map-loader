//! Core of the Insight Prime desktop manager.
//!
//! Several rooted Quest headsets are worn as body trackers. Each one tracks
//! itself well, but in its own arbitrary per-boot frame, so their poses are not
//! comparable until the transform between those frames is known. This crate is
//! the part that receives the poses, keeps the transforms, and hands out a
//! single consistent view of where every puck is.
//!
//! It is UI-free on purpose: the GUI and the SteamVR-facing side both sit on top
//! of it, and neither should be able to change what "where is this puck" means.

pub mod aggregate;
pub mod bridge;
pub mod config;
pub mod fleet;
pub mod ingest;
pub mod jobs;
pub mod mpt1;
pub mod service;
pub mod transform;

pub use ingest::{Ingest, Sample, SlotState};
pub use mpt1::{Device, Packet, Pose};
pub use transform::Frame4Dof;

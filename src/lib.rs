//! xcheckfs: a FUSE file system that mirrors every operation to a trusted
//! primary file system and an experimental secondary file system in
//! lockstep, compares the outcomes, and reports any disagreement.

pub mod backend;
pub mod compare;
pub mod config;
pub mod control;
pub mod daemon;
pub mod engine;
pub mod events;
pub mod fusefs;
pub mod logging;
pub mod policy;
pub mod stats;
pub mod sys;
pub mod tui;
pub mod verify;

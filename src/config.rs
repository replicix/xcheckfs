//! Runtime configuration shared by the engine, the policy and the UIs.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How much checking is done for every operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CheckLevel {
    /// Compare return codes and everything both file systems return
    /// (attributes, data, directory listings, link targets, xattrs).
    Basic,
    /// Additionally read back the effect of every mutation while still in
    /// lockstep: data after write, attributes after setattr, absence after
    /// unlink, identity after rename, ...
    Thorough,
    /// Additionally compare the complete file content when a written file is
    /// closed, and the complete parent listing after namespace changes.
    Paranoid,
}

impl CheckLevel {
    pub fn name(&self) -> &'static str {
        match self {
            CheckLevel::Basic => "basic",
            CheckLevel::Thorough => "thorough",
            CheckLevel::Paranoid => "paranoid",
        }
    }
}

/// How strictly operations on one object are serialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Serialization {
    /// Every data operation holds its object exclusively (reads shared).
    Strict,
    /// In-place data operations on disjoint byte ranges of one file run
    /// concurrently (and reach both file systems concurrently); anything
    /// that changes the size or metadata stays exclusive.
    Relaxed,
}

/// Which opened files use the kernel's direct-I/O path (no page cache).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DirectIo {
    /// None: the kernel page cache serves reads and serializes writes.
    Off,
    /// Files the application opens with O_DIRECT, with parallel direct
    /// writes: concurrent in-place writes to one file reach xcheckfs (and
    /// both file systems) concurrently.
    Auto,
    /// Every file (every read and write reaches xcheckfs; shared writable
    /// mmap needs kernel 6.7+).
    All,
}

/// What happens when the file systems disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MismatchMode {
    /// Report the mismatch, return the primary's result, then repair the
    /// secondary from the primary so comparisons stay meaningful.
    Resync,
    /// Report the mismatch and return the primary's result.
    Log,
    /// Return EIO for the operation (the primary has already applied it).
    Fail,
    /// Hold the operation, and every new one, until an operator decides.
    Freeze,
    /// Stop using the secondary; continue as a pass-through to the primary.
    Detach,
}

impl MismatchMode {
    pub fn name(&self) -> &'static str {
        match self {
            MismatchMode::Resync => "resync",
            MismatchMode::Log => "log",
            MismatchMode::Fail => "fail",
            MismatchMode::Freeze => "freeze",
            MismatchMode::Detach => "detach",
        }
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub check: CheckLevel,
    /// Allowed difference between the two mtimes, and the minimum change of
    /// one side's ctime that must be matched by the other side.
    pub time_tolerance: Duration,
    /// Compare directory link counts (some file systems always report 1).
    pub dir_nlink: bool,
    /// Probe both file systems at mount time and adapt to their legitimate differences.
    pub probe: bool,
    /// Run the two backends' halves of an operation concurrently.
    pub parallel: bool,
    /// Switch to the caller's fsuid/fsgid/groups for mutations. Only
    /// effective when running as root.
    pub creds: bool,
    /// Mirror POSIX record locks (fcntl) to both file systems.
    pub mirror_locks: bool,
    /// Record per-operation events for the TUI / control socket.
    pub op_events: bool,
    pub attr_ttl: Duration,
    pub entry_ttl: Duration,
    pub direct_io: DirectIo,
    /// Number of lock stripes (rounded up to a power of two).
    pub lock_stripes: usize,
    pub serialize: Serialization,
    /// Threads running secondary halves, at least (and at least one per CPU); at least as many as there are
    /// threads issuing operations, so that a secondary half never queues behind other operations' halves.
    pub secondary_threads: usize,
    /// Where secondary objects are copied before resync overwrites or
    /// removes them. `None`: report only.
    pub quarantine: Option<std::path::PathBuf>,
    /// Bytes saved per quarantined object (larger content is truncated).
    pub quarantine_cap: u64,
    /// Repairs of one object within ten minutes before xcheckfs gives up on
    /// it (the object then stays diverged and is handled primary-only).
    pub resync_limit: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            check: CheckLevel::Basic,
            time_tolerance: Duration::from_secs(1),
            dir_nlink: true,
            probe: true,
            parallel: true,
            creds: true,
            mirror_locks: true,
            op_events: false,
            attr_ttl: Duration::from_secs(1),
            entry_ttl: Duration::from_secs(1),
            direct_io: DirectIo::Auto,
            lock_stripes: 65536,
            serialize: Serialization::Relaxed,
            secondary_threads: 16,
            quarantine: None,
            quarantine_cap: 64 << 20,
            resync_limit: 5,
        }
    }
}

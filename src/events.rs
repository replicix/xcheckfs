//! Event stream from the engine to the interactive UI.

use std::sync::Arc;
use std::time::SystemTime;

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::policy::Mismatch;
use crate::stats::{OpKind, Stats};

#[derive(Clone, Debug)]
pub struct OpEvent {
    pub time: SystemTime,
    pub op: OpKind,
    pub ino: u64,
    /// Best-effort path or name of the object, plus arguments.
    pub detail: String,
    /// The primary's result (0 = success, otherwise errno).
    pub errno: i32,
    /// The secondary's result, `None` when it was not consulted.
    pub sec_errno: Option<i32>,
    pub total_ns: u64,
    pub primary_ns: u64,
    pub secondary_ns: u64,
    pub bytes: u64,
    pub mismatch: bool,
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Op(OpEvent),
    Mismatch(Arc<Mismatch>),
    Log { level: tracing::Level, message: String },
}

#[derive(Clone)]
pub struct EventSink {
    tx: Option<Sender<UiEvent>>,
}

impl EventSink {
    pub fn disabled() -> EventSink {
        EventSink { tx: None }
    }

    pub fn channel(capacity: usize) -> (EventSink, Receiver<UiEvent>) {
        let (tx, rx) = crossbeam_channel::bounded(capacity);
        (EventSink { tx: Some(tx) }, rx)
    }

    pub fn sender(&self) -> Option<Sender<UiEvent>> {
        self.tx.clone()
    }

    pub fn enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// Never blocks: a slow UI must not slow the file system down.
    pub fn send(&self, ev: UiEvent, stats: &Stats) {
        if let Some(tx) = &self.tx {
            if let Err(TrySendError::Full(_)) = tx.try_send(ev) {
                stats.events_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

//! Backend-agnostic progress reporting. `cli` renders a bar; `tui` feeds a view.

use std::sync::{Arc, Mutex};

use crate::model::Phase;

/// One phase of one volume within one run. `total` is `None` when unknown.
#[derive(Clone, Copy, Debug)]
pub struct PhaseProgress {
    pub run_id: i64,
    pub volume_id: i64,
    pub phase: Phase,
    pub done: u64,
    pub total: Option<u64>,
}

/// Receives progress events. Implementations must be cheap; passes call them in hot loops.
pub trait Sink: Send + Sync {
    fn phase_started(&mut self, _p: &PhaseProgress) {}
    fn advance(&mut self, _run_id: i64, _volume_id: i64, _n: u64) {}
    fn phase_finished(&mut self, _p: &PhaseProgress) {}
    fn message(&mut self, _msg: &str) {}
}

/// Shared handle passed through `Services`, so passes and the CLI hold the same sink.
pub type SharedSink = Arc<Mutex<dyn Sink>>;

/// A sink that discards everything. Useful as a default.
#[derive(Default)]
pub struct NullSink;

impl Sink for NullSink {}

/// A new `SharedSink` backed by [`NullSink`].
pub fn null_sink() -> SharedSink {
    Arc::new(Mutex::new(NullSink))
}

/// Forwards a phase start to the sink. A poisoned lock is ignored.
pub fn emit_started(s: &SharedSink, p: &PhaseProgress) {
    if let Ok(mut g) = s.lock() {
        g.phase_started(p);
    }
}

/// Forwards an advance of `n` units to the sink. A poisoned lock is ignored.
pub fn emit_advance(s: &SharedSink, run_id: i64, volume_id: i64, n: u64) {
    if let Ok(mut g) = s.lock() {
        g.advance(run_id, volume_id, n);
    }
}

/// Forwards a phase finish to the sink. A poisoned lock is ignored.
pub fn emit_finished(s: &SharedSink, p: &PhaseProgress) {
    if let Ok(mut g) = s.lock() {
        g.phase_finished(p);
    }
}

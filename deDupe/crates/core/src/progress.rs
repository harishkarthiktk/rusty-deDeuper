//! Backend-agnostic progress reporting. `cli` renders a bar; `tui` feeds a view.

use crate::model::Phase;

#[derive(Clone, Copy, Debug)]
pub struct PhaseProgress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

pub trait Sink: Send {
    fn phase_started(&mut self, _phase: Phase) {}
    fn advance(&mut self, n: u64) {
        let _ = n;
    }
    fn phase_finished(&mut self, _phase: Phase) {}
    fn message(&mut self, _msg: &str) {}
}

/// A sink that discards everything. Useful as a default.
#[derive(Default)]
pub struct NullSink;

impl Sink for NullSink {}

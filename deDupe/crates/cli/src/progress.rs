//! Terminal progress for `dedupe scan`. Counting happens in `core`; this module only draws.
//! Output goes to stderr so stdout stays a clean report.

use std::collections::HashMap;
use std::time::Duration;

use dedupe_core::model::Phase;
use dedupe_core::progress::{PhaseProgress, Sink};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// `--progress` flag: when to draw progress on stderr.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ProgressMode {
    /// Draw only when stderr is a terminal and output is not JSON.
    Auto,
    /// Draw when requested, even if stderr is not a terminal. JSON output still hides it.
    Always,
    /// Never draw.
    Never,
}

/// Whether to draw progress. JSON output and `Never` always suppress it.
pub fn should_draw(mode: ProgressMode, json: bool, stderr_is_tty: bool) -> bool {
    if json {
        return false;
    }
    match mode {
        ProgressMode::Auto => stderr_is_tty,
        ProgressMode::Always => true,
        ProgressMode::Never => false,
    }
}

/// Renders each (run, volume) phase as a spinner (total unknown) or a bar on stderr.
pub struct BarSink {
    multi: MultiProgress,
    bars: HashMap<(i64, i64), ProgressBar>,
}

impl BarSink {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::with_draw_target(ProgressDrawTarget::stderr()),
            bars: HashMap::new(),
        }
    }
}

/// Human label for the pass that completes `phase`.
fn label(phase: Phase) -> &'static str {
    match phase {
        Phase::Enumerated => "Reading files",
        Phase::PrefixDone => "Prefix hash",
        Phase::FullDone => "Full hash",
        Phase::Verified => "Verify",
        _ => "Working",
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {prefix}... {pos} found [{elapsed_precise}]")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {prefix:<12} [{bar:40.cyan/blue}] {pos}/{len} [{elapsed_precise}, {eta}]",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
}

impl Sink for BarSink {
    fn phase_started(&mut self, p: &PhaseProgress) {
        // A pass with nothing to do draws nothing.
        if p.total == Some(0) {
            return;
        }
        let bar = match p.total {
            None => {
                let bar = ProgressBar::new_spinner()
                    .with_style(spinner_style())
                    .with_prefix(label(p.phase))
                    .with_position(p.done);
                bar.enable_steady_tick(Duration::from_millis(120));
                bar
            }
            Some(total) => ProgressBar::new(total)
                .with_style(bar_style())
                .with_prefix(label(p.phase))
                .with_position(p.done),
        };
        let bar = self.multi.add(bar);
        if let Some(old) = self.bars.insert((p.run_id, p.volume_id), bar) {
            old.finish_and_clear();
        }
    }

    fn advance(&mut self, run_id: i64, volume_id: i64, n: u64) {
        if let Some(bar) = self.bars.get(&(run_id, volume_id)) {
            bar.inc(n);
        }
    }

    fn phase_finished(&mut self, p: &PhaseProgress) {
        if let Some(bar) = self.bars.remove(&(p.run_id, p.volume_id)) {
            bar.finish_and_clear();
        }
    }

    fn message(&mut self, msg: &str) {
        let _ = self.multi.println(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::{should_draw, ProgressMode};

    #[test]
    fn should_draw_truth_table() {
        for tty in [false, true] {
            // JSON output suppresses progress in every mode.
            assert!(!should_draw(ProgressMode::Auto, true, tty));
            assert!(!should_draw(ProgressMode::Always, true, tty));
            assert!(!should_draw(ProgressMode::Never, true, tty));
            // Never suppresses progress regardless of TTY.
            assert!(!should_draw(ProgressMode::Never, false, tty));
            // Always draws without a TTY.
            assert!(should_draw(ProgressMode::Always, false, tty));
            // Auto follows the TTY flag.
            assert_eq!(should_draw(ProgressMode::Auto, false, tty), tty);
        }
    }
}

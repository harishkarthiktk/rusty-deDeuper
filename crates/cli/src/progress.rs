//! Terminal progress for `dedupe scan` and `dedupe report`. Counting happens in `core`;
//! this module only draws. Output goes to stderr so stdout stays a clean report.

use std::collections::HashMap;
use std::time::Duration;

use dedupe_core::model::Phase;
use dedupe_core::progress::{PhaseProgress, Sink};
use dedupe_core::report::ReportEvent;
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

    /// Shared draw target, so other bars render in the same group.
    #[cfg(windows)]
    pub fn multi(&self) -> MultiProgress {
        self.multi.clone()
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

#[cfg(windows)]
fn mft_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} Reading $MFT [{bar:40.cyan/blue}] {pos}/{len} records [{elapsed_precise}, {eta}]",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
}

/// Draws the `$MFT` read as a bar in `multi`. The bar goes away on `Finished`.
#[cfg(windows)]
pub fn mft_observer(multi: MultiProgress) -> dedupe_backend_windows::MftObserver {
    use dedupe_backend_windows::MftEvent;
    let bar: std::sync::Mutex<Option<ProgressBar>> = std::sync::Mutex::new(None);
    std::sync::Arc::new(move |ev: MftEvent| {
        let Ok(mut slot) = bar.lock() else { return };
        match ev {
            MftEvent::Started { records } => {
                let _ = multi.println(format!("reading NTFS $MFT ({records} records)"));
                *slot = Some(multi.add(ProgressBar::new(records).with_style(mft_bar_style())));
            }
            MftEvent::Records { done } => {
                if let Some(b) = slot.as_ref() {
                    b.set_position(done);
                }
            }
            MftEvent::CacheLoaded { records } => {
                let _ = multi.println(format!("MFT cache loaded: {records} records"));
            }
            MftEvent::DeltaApplied { entries, reread } => {
                let _ = multi.println(format!(
                    "MFT cache: applied {entries} journal entries ({reread} records re-read)"
                ));
            }
            MftEvent::Finished => {
                if let Some(b) = slot.take() {
                    b.finish_and_clear();
                }
            }
        }
    })
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

fn report_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} Building report {pos}/{len} groups [{elapsed_precise}]",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
}

/// Spinner until the candidate count is known, then a counter, for `dedupe report`.
pub struct ReportBar {
    bar: ProgressBar,
}

impl ReportBar {
    pub fn new() -> Self {
        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
            .with_style(spinner_style())
            .with_prefix("Building report");
        bar.enable_steady_tick(Duration::from_millis(120));
        Self { bar }
    }

    pub fn on_event(&mut self, ev: ReportEvent) {
        match ev {
            ReportEvent::Volume { groups, .. } => {
                self.bar.set_style(report_bar_style());
                self.bar.set_length(groups);
                self.bar.set_position(0);
            }
            ReportEvent::Group { done } => self.bar.set_position(done),
            ReportEvent::Done => self.bar.finish_and_clear(),
        }
    }
}

impl Drop for ReportBar {
    /// Clears the line on error or cancellation, so the error message prints cleanly.
    fn drop(&mut self) {
        self.bar.finish_and_clear();
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

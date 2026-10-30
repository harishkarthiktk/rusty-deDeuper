//! Windows backend: the NTFS `$MFT` fast path with the open-by-id reader.

use std::sync::Arc;

use dedupe_backend_generic::{GenericPathCodec, GenericVolumeIdentity};
use dedupe_backend_windows::{
    mft_cache, AutoEnumerator, CacheConfig, EnumNotices, HandleReader, MftObserver,
};
use dedupe_core::cancel::Token;
use dedupe_core::storage_profile::Profile;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use super::{read_ahead_for, Backend, ScanExtras};
use crate::ScanArgs;

/// Smallest read chunk for overlapped reads. A small `--buf-size` would otherwise turn each
/// overlapped read into a tiny request, which costs more than the overlap saves.
const MIN_OVERLAPPED_CHUNK: usize = 256 * 1024;

/// Messages the enumerator queued during the scan, such as why the walker was used.
/// Drained after the scan, on success and on failure alike.
pub struct Extras {
    notices: Arc<EnumNotices>,
}

impl ScanExtras for Extras {
    /// Which enumerator produced the file list, as stored in `scan_runs.enumerator`.
    fn enumerator_label(&self, no_mft: bool) -> &'static str {
        match (self.notices.walker_used(), no_mft) {
            (false, _) => "mft",
            (true, true) => "walker-forced",
            (true, false) => "walker-fallback",
        }
    }

    fn drain_notices(&self) -> Vec<String> {
        self.notices.drain()
    }
}

fn mft_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} Reading $MFT [{bar:40.cyan/blue}] {pos}/{len} records [{elapsed_precise}, {eta}]",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
}

/// Draws the `$MFT` read as a bar in `multi`. The bar goes away on `Finished`.
fn mft_observer(multi: MultiProgress) -> MftObserver {
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

/// Builds the backend for one scan. Windows picks the NTFS fast path with the open-by-id
/// reader; the reader's overlapped read-ahead depends on the storage profile. The token goes to
/// the enumerator so Ctrl-C can stop the `$MFT` read, which the trait has no way to receive.
/// `multi`, when set, draws the `$MFT` read progress.
pub fn make_backend(
    args: &ScanArgs,
    profile: &Profile,
    cancel: &Token,
    multi: Option<MultiProgress>,
) -> Backend {
    let observer = multi.map(mft_observer);
    let notices = Arc::new(EnumNotices::default());
    let chunk = profile.buf_size.max(MIN_OVERLAPPED_CHUNK);
    // `--no-mft-cache` turns the cache off. Otherwise the directory comes from DEDUPE_CACHE_DIR or
    // %LOCALAPPDATA%; with neither set, `dir` is `None` and no cache is used.
    let cache = if args.no_mft_cache {
        CacheConfig::default()
    } else {
        CacheConfig {
            dir: mft_cache::cache_dir(&|key| std::env::var_os(key)),
            rebuild: args.rebuild_mft_cache,
        }
    };
    let mut enumerator =
        AutoEnumerator::new(args.no_mft, Arc::clone(&notices), cancel.clone()).with_cache(cache);
    if let Some(observer) = observer {
        enumerator = enumerator.with_observer(observer);
    }
    Backend {
        enumerator: Box::new(enumerator),
        reader: Box::new(HandleReader::new(read_ahead_for(profile), chunk)),
        codec: GenericPathCodec,
        volume: GenericVolumeIdentity,
        extras: Extras { notices },
    }
}

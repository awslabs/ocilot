//! Optional progress reporting for long running blob operations.
//!
//! The library exposes a small object-safe trait so callers can plug in any
//! progress UI (e.g. `indicatif`) without forcing that dependency on every
//! consumer. Pass `None` anywhere a `Option<&dyn ProgressReporter>` is
//! accepted to disable progress reporting entirely. When a reporter must
//! cross a `tokio::spawn` boundary, pass an [`std::sync::Arc`] wrapped
//! version via [`SharedProgress`].

use std::sync::Arc;

/// A handle to an in-flight progress bar / counter for a single operation.
///
/// Implementations should treat `Drop` as the completion signal so callers do
/// not need to remember to finish the bar manually.
pub trait ProgressHandle: Send + Sync {
    /// Advance the bar by the given number of bytes.
    fn inc(&self, delta: u64);

    /// Update the bar's message text. Implementations may ignore this.
    fn set_message(&self, _message: &str) {}

    /// Mark the bar as finished. Called automatically on drop for impls that
    /// implement [`Drop`]; provided here for explicit completion semantics.
    fn finish(&self) {}
}

/// Object-safe trait for spawning per-operation progress handles.
///
/// One [`ProgressReporter`] is shared across an entire command and produces a
/// new [`ProgressHandle`] for each blob/layer/manifest operation that wants
/// to report.
pub trait ProgressReporter: Send + Sync {
    /// Begin a new progress unit with the given total byte count and label.
    fn start(&self, total: u64, label: &str) -> Box<dyn ProgressHandle>;
}

/// A no-op handle for when progress is disabled.
pub struct NoopHandle;

impl ProgressHandle for NoopHandle {
    fn inc(&self, _delta: u64) {}
}

/// Cheap-to-clone shared owner of a [`ProgressReporter`] that can be moved
/// into spawned tasks. `None` represents "no progress" without forcing
/// callers to allocate.
#[derive(Clone, Default)]
pub struct SharedProgress(Option<Arc<dyn ProgressReporter>>);

impl SharedProgress {
    pub fn new(reporter: Arc<dyn ProgressReporter>) -> Self {
        Self(Some(reporter))
    }

    pub fn none() -> Self {
        Self(None)
    }

    /// Borrow the inner reporter as `Option<&dyn ProgressReporter>` for
    /// passing into the per-blob APIs.
    pub fn as_ref(&self) -> Option<&dyn ProgressReporter> {
        self.0.as_deref()
    }
}

#[cfg(feature = "progress")]
mod indicatif_impl {
    use super::{ProgressHandle, ProgressReporter};
    use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

    /// `indicatif`-backed implementation of [`ProgressReporter`].
    ///
    /// Wraps a [`MultiProgress`] and produces a styled [`ProgressBar`] for
    /// every operation.
    pub struct IndicatifReporter {
        multi: MultiProgress,
    }

    impl IndicatifReporter {
        pub fn new(multi: MultiProgress) -> Self {
            Self { multi }
        }
    }

    impl ProgressReporter for IndicatifReporter {
        fn start(&self, total: u64, label: &str) -> Box<dyn ProgressHandle> {
            let bar = self.multi.add(ProgressBar::new(total));
            // The unwrap on the template is a literal known-good template.
            let style = ProgressStyle::with_template(
                "{prefix}: [{elapsed_precise}] {bar:40.cyan/blue} {msg} ({binary_bytes:>7}/{binary_total_bytes:7})",
            )
            .expect("invariant: progress template literal is valid")
            .progress_chars("##-");
            bar.set_style(style);
            bar.set_prefix(label.to_string());
            Box::new(IndicatifHandle { bar })
        }
    }

    /// Handle wrapping a single [`ProgressBar`].
    pub struct IndicatifHandle {
        bar: ProgressBar,
    }

    impl ProgressHandle for IndicatifHandle {
        fn inc(&self, delta: u64) {
            self.bar.inc(delta);
        }

        fn set_message(&self, message: &str) {
            self.bar.set_message(message.to_string());
        }

        fn finish(&self) {
            self.bar.finish_with_message("done");
        }
    }

    impl Drop for IndicatifHandle {
        fn drop(&mut self) {
            if !self.bar.is_finished() {
                self.bar.finish_with_message("done");
            }
        }
    }
}

#[cfg(feature = "progress")]
pub use indicatif_impl::{IndicatifHandle, IndicatifReporter};

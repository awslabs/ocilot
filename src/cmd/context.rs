use cfg_if::cfg_if;
use ocilot::progress::SharedProgress;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// Application context passed through command execution.
///
/// Holds a single `SharedProgress` (Arc-backed) that all commands clone
/// when calling library APIs. With the `progress` feature this wraps the
/// `indicatif` reporter; without it, [`SharedProgress::none`] is used so
/// progress calls become no-ops.
pub struct Ctx {
    progress: SharedProgress,
}

impl Ctx {
    pub fn init() -> ocilot::Result<Self> {
        cfg_if! {
            if #[cfg(feature = "progress")] {
                let indicatif_layer = tracing_indicatif::IndicatifLayer::new();
                tracing_subscriber::registry()
                    .with(tracing_subscriber::fmt::layer()
                        .with_writer(indicatif_layer.get_stdout_writer())
                        .with_filter(EnvFilter::from_default_env())
                    )
                    .with(indicatif_layer.with_filter(EnvFilter::from_default_env()))
                    .try_init()
                    .ok();
                let multi = indicatif::MultiProgress::new();
                let reporter = std::sync::Arc::new(
                    ocilot::progress::IndicatifReporter::new(multi),
                );
                let progress = SharedProgress::new(reporter);
            } else {
                tracing_subscriber::registry()
                    .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()))
                    .try_init()
                    .ok();
                let progress = SharedProgress::none();
            }
        }
        Ok(Self { progress })
    }

    /// Get a clone of the shared progress reporter to hand off to library
    /// calls. Cheap (single Arc clone).
    pub fn progress(&self) -> SharedProgress {
        self.progress.clone()
    }
}

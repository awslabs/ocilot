use cfg_if::cfg_if;
use indicatif::MultiProgress;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

pub struct Ctx {
    multi: MultiProgress,
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
                    .unwrap();
            } else {
                tracing_subscriber::registry()
                    .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()))
                    .try_init()
                    .unwrap();;

            }
        }
        let multi = MultiProgress::new();
        Ok(Self { multi })
    }

    pub fn get(&mut self) -> &mut MultiProgress {
        &mut self.multi
    }
}

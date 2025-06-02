use cfg_if::cfg_if;
use indicatif::{MultiProgress, ProgressBar};
use std::pin::Pin;
use std::task::Poll;
use tokio::io::AsyncWrite;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub struct Ctx {
    multi: MultiProgress,
}

impl Ctx {
    pub fn init() -> anyhow::Result<Self> {
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

pub struct ProgressWriter<'a, W: AsyncWrite + 'a> {
    pub inner: Pin<Box<W>>,
    pub progress: &'a mut ProgressBar,
}

impl<'a, W: AsyncWrite + 'a> AsyncWrite for ProgressWriter<'a, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_write(cx, buf) {
            Poll::Ready(Ok(size)) => {
                info!("adding: {} bytes", size);
                this.progress.inc(size as u64);
                Poll::Ready(Ok(size))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_shutdown(cx)
    }
}

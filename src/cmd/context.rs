use anyhow::Context;
use indicatif::{MultiProgress, ProgressBar};
use indicatif_log_bridge::LogWrapper;
use std::pin::Pin;
use std::task::Poll;
use tokio::io::AsyncWrite;

pub struct Ctx {
    multi: MultiProgress,
}

impl Ctx {
    pub fn init() -> anyhow::Result<Self> {
        let logger = pretty_env_logger::formatted_builder()
            .filter_level(log::LevelFilter::Info)
            .build();
        let level = logger.filter();
        let multi = MultiProgress::new();
        LogWrapper::new(multi.clone(), logger)
            .try_init()
            .context("failed to initialize logger")?;
        log::set_max_level(level);
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

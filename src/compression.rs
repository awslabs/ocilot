use std::pin::Pin;

use async_compression::tokio::bufread::{
    BzDecoder, GzipDecoder, LzmaDecoder, XzDecoder, ZstdDecoder,
};
use tokio::io::AsyncRead;
use tokio::io::BufReader;

use crate::{
    layer::Reader,
    models::{Compression, MediaType},
};

pub struct Decompress {
    inner: Pin<Box<dyn AsyncRead>>,
}

unsafe impl Send for Decompress {}
unsafe impl Sync for Decompress {}

impl Decompress {
    pub fn new(media: &MediaType, reader: Reader) -> Self {
        Self {
            inner: match media {
                MediaType::DockerImageRootfs(compression) => match compression {
                    Compression::Gzip | Compression::None => {
                        Box::pin(GzipDecoder::new(BufReader::new(reader)))
                    }
                    Compression::Bzip2 => Box::pin(BzDecoder::new(BufReader::new(reader))),
                    Compression::Lz4 => Box::pin(LzmaDecoder::new(BufReader::new(reader))),
                    Compression::Xz => Box::pin(XzDecoder::new(BufReader::new(reader))),
                    Compression::Zstd => Box::pin(ZstdDecoder::new(BufReader::new(reader))),
                },
                MediaType::Layer(compression) => match compression {
                    Compression::Gzip => Box::pin(GzipDecoder::new(BufReader::new(reader))),
                    Compression::Bzip2 => Box::pin(BzDecoder::new(BufReader::new(reader))),
                    Compression::Lz4 => Box::pin(LzmaDecoder::new(BufReader::new(reader))),
                    Compression::Xz => Box::pin(XzDecoder::new(BufReader::new(reader))),
                    Compression::Zstd => Box::pin(ZstdDecoder::new(BufReader::new(reader))),
                    Compression::None => Box::pin(BufReader::new(reader)),
                },
                _ => Box::pin(BufReader::new(reader)),
            },
        }
    }
}

impl AsyncRead for Decompress {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_read(cx, buf)
    }
}

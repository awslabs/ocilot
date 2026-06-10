use std::pin::Pin;

use async_compression::tokio::bufread::{BzDecoder, GzipDecoder, XzDecoder, ZstdDecoder};
use tokio::io::AsyncRead;
use tokio::io::BufReader;

use crate::{
    error,
    layer::Reader,
    models::{Compression, MediaType},
};

/// A decompressing wrapper around a [`Reader`].
///
/// The inner stream is bound by `Send + Sync` so the type can be used
/// across thread boundaries (e.g. with `tokio::spawn`) without any unsafe
/// impls.
pub struct Decompress {
    inner: Pin<Box<dyn AsyncRead + Send + Sync>>,
}

impl Decompress {
    /// Construct a new decompressor for the given media type around a
    /// [`Reader`]. Returns an `Unsupported` error for layer formats this
    /// crate cannot transparently decode (currently lz4).
    pub fn new(media: &MediaType, reader: Reader) -> crate::Result<Self> {
        let inner: Pin<Box<dyn AsyncRead + Send + Sync>> = match media {
            // Docker rootfs layers historically default to gzip when the
            // media type does not encode a compression suffix; spec calls
            // this an "uncompressed" diff which is a misnomer in practice.
            MediaType::DockerImageRootfs(compression) => match compression {
                Compression::Gzip => Box::pin(GzipDecoder::new(BufReader::new(reader))),
                Compression::Bzip2 => Box::pin(BzDecoder::new(BufReader::new(reader))),
                Compression::Xz => Box::pin(XzDecoder::new(BufReader::new(reader))),
                Compression::Zstd => Box::pin(ZstdDecoder::new(BufReader::new(reader))),
                Compression::None => Box::pin(BufReader::new(reader)),
                Compression::Lz4 => {
                    return error::UnsupportedSnafu {
                        reason: "lz4 layer decompression is not supported".to_string(),
                    }
                    .fail();
                }
            },
            MediaType::Layer(compression) => match compression {
                Compression::Gzip => Box::pin(GzipDecoder::new(BufReader::new(reader))),
                Compression::Bzip2 => Box::pin(BzDecoder::new(BufReader::new(reader))),
                Compression::Xz => Box::pin(XzDecoder::new(BufReader::new(reader))),
                Compression::Zstd => Box::pin(ZstdDecoder::new(BufReader::new(reader))),
                Compression::None => Box::pin(BufReader::new(reader)),
                Compression::Lz4 => {
                    return error::UnsupportedSnafu {
                        reason: "lz4 layer decompression is not supported".to_string(),
                    }
                    .fail();
                }
            },
            _ => Box::pin(BufReader::new(reader)),
        };
        Ok(Self { inner })
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

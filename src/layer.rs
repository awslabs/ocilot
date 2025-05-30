use crate::error;
use crate::models::MediaType;
use crate::models::Platform;
use crate::uri::{Reference, Uri};
use bytes::Bytes;
use cfg_if::cfg_if;
use derive_builder::Builder;
use futures::future::BoxFuture;
use futures::FutureExt;
#[cfg(feature = "progress")]
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Response;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snafu::{ensure, ResultExt};
use std::cmp::min;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio_util::io::StreamReader;

/// Minimum chunk size for network operations
const MIN_CHUNK_SIZE: usize = 5 * 1024 * 1024;
/// Maximum chunk size for network operations
const MAX_CHUNK_SIZE: usize = 128 * 1024 * 1024;

/// A layer represents a blob or sub-object (like a image config) associated with an
/// image. As such operations for reading or writing blobs operate off this object.
#[derive(Debug, Serialize, Deserialize, Clone, Builder)]
#[builder(setter(into))]
#[serde(rename_all = "camelCase")]
pub struct Layer {
    media_type: MediaType,
    size: usize,
    digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<Platform>,
}

impl Layer {
    /// Perform a chunked copy of a layer from one reader to another. Any time you want to interact with a
    /// layer in a registry, it is recommended to use this method. While most OCI registry implementations do not
    /// need special handling to make the chunks of data sent uniform, certain implementations (i.e. ECR) work better when
    /// using more uniform chunked operations.
    pub async fn copy<'a, R, W>(
        reader: &'a mut R,
        writer: &'a mut W,
        size: usize,
    ) -> crate::Result<()>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        let mut index = 0;
        // To determine the chunk size we do some math:
        // 1. The chunk size should always be >= MIN_CHUNK_SIZE
        // 2. The chunk size should always be <= MAX_CHUNK_SIZE
        // 3. Ideally the chunk size should be 1/40th of the size of the layer (this lines up with how we print progress bar updates)
        let chunk_size = (size / 40).clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE);
        while index < size {
            let read_size = min(chunk_size, size - index);
            let mut buffer = vec![0; read_size];
            reader
                .read_exact(&mut buffer)
                .await
                .context(error::LayerReadSnafu)?;
            writer
                .write_all(buffer.as_slice())
                .await
                .context(error::LayerWriteSnafu)?;
            index += chunk_size;
        }
        Ok(())
    }

    /// Create a new later on a registry and repository
    pub async fn create(
        uri: &Uri,
        media_type: &MediaType,
        size: usize,
        digest: Option<String>,
    ) -> crate::Result<Option<Writer>> {
        if let Some(digest) = digest.as_ref() {
            // Check if the registry already has this layer
            trace!(target: "layer", "checking if a blob already exists with the digest: {digest}");
            if uri
                .registry()
                .check_blob(uri.repository(), digest.as_str())
                .await?
            {
                debug!(target: "layer", "blob already exists with the digest: {digest}");
                return Ok(None);
            }
        }

        cfg_if! {
            if #[cfg(feature = "progress")] {
                Ok(Some(Writer {
                    uri: uri.clone(),
                    index: 0,
                    size,
                    media_type: media_type.clone(),
                    upload_url: None,
                    active: None,
                    digest: Sha256::new(),
                    progress: None,
                }))
            } else {
                Ok(Some(Writer {
                    uri: uri.clone(),
                    index: 0,
                    size,
                    media_type: media_type.clone(),
                    upload_url: None,
                    active: None,
                    digest: Sha256::new(),
                }))
            }
        }
    }

    /// Create a new layer and report upload progress via an indicatif progress bar
    #[cfg(feature = "progress")]
    pub async fn create_progress(
        uri: &Uri,
        media_type: &MediaType,
        prefix: &str,
        size: u64,
        multi: &mut MultiProgress,
        digest: Option<String>,
    ) -> crate::Result<Option<Writer>> {
        let bar = multi.add(ProgressBar::new(size));
        bar.set_style(
            ProgressStyle::with_template(
                "-> {prefix}: [{elapsed_precise}] {bar:40.cyan/blue} {msg} ({binary_bytes:>7}/{binary_total_bytes:7})",
            )
            .unwrap()
            .progress_chars("##-"),
        );
        bar.set_prefix(prefix.to_string());
        if let Some(digest) = digest.as_ref() {
            // Check if the registry already has this layer
            trace!(target: "layer", "checking if a blob already exists with the digest: {digest}");
            if uri
                .registry()
                .check_blob(uri.repository(), digest.as_str())
                .await?
            {
                debug!(target: "layer", "blob already exists with the digest: {digest}");
                bar.finish_with_message("already exists");
                return Ok(None);
            }
        }

        Ok(Some(Writer {
            uri: uri.clone(),
            index: 0,
            size: size as usize,
            media_type: media_type.clone(),
            upload_url: None,
            active: None,
            digest: Sha256::new(),
            progress: Some(bar),
        }))
    }

    /// Open a layer blob for reading
    pub async fn open(&self, uri: &Uri) -> crate::Result<Reader> {
        let (reader, _) = uri
            .registry()
            .fetch_blob(uri.repository(), self.digest.as_str())
            .await?;
        let reader = StreamReader::new(reader);
        Ok(Reader::new(reader))
    }

    /// Open a layer blob for reading and report progress to an indicatif progress bar
    #[cfg(feature = "progress")]
    pub async fn open_progress(
        &self,
        uri: &Uri,
        multi: &mut MultiProgress,
    ) -> crate::Result<Reader> {
        let prefix = &self.digest.strip_prefix("sha256:").unwrap()[0..9];
        let (reader, _) = uri
            .registry()
            .fetch_blob(uri.repository(), self.digest.as_str())
            .await?;
        let bar = multi.add(ProgressBar::new(self.size as u64));
        bar.set_style(
            ProgressStyle::with_template(
                "<- {prefix}: [{elapsed_precise}] {bar:40.cyan/blue} {msg} ({binary_bytes:>7}/{binary_total_bytes:7})",
            )
            .unwrap()
            .progress_chars("##-"),
        );
        bar.set_prefix(format!("blob {prefix}"));
        let reader = StreamReader::new(reader);
        Ok(Reader::new_progress(reader, bar))
    }

    /// Open a layer for reading at the specified uri
    pub async fn open_uri(uri: &Uri) -> crate::Result<Reader> {
        ensure!(
            matches!(uri.reference(), Reference::Digest { .. }),
            error::DirectLoadBlobSnafu { uri: uri.clone() }
        );
        let (reader, _) = uri
            .registry()
            .fetch_blob(uri.repository(), uri.reference().to_string().as_str())
            .await?;
        Ok(Reader::new(StreamReader::new(reader)))
    }

    /// Media type of the layer
    pub fn media_type(&self) -> &MediaType {
        &self.media_type
    }

    /// Digest string for the layer
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Size in bytes
    pub fn size(&self) -> usize {
        self.size
    }

    /// Platform this layer is specific to, this is primarily only used in an image index
    pub fn platform(&self) -> Option<Platform> {
        self.platform.clone()
    }

    /// Delete this layer from the registry and repository provided by a uri
    pub async fn delete(&self, uri: &Uri) -> crate::Result<()> {
        uri.registry()
            .delete_blob(uri.repository(), self.digest.as_str())
            .await
    }
}

/// Reader implements a layer AsyncRead implementation that
/// automatically will report to a progress bar if provided
/// and the progress feature is enabled. It optionally can also
/// automatically decompress the contents of the reader
pub struct Reader {
    inner: Pin<Box<dyn AsyncRead>>,
    #[cfg(feature = "progress")]
    progress: Option<ProgressBar>,
}

#[cfg(feature = "progress")]
impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(progress) = self.progress.as_mut() {
            progress.finish_with_message("done");
        }
    }
}

unsafe impl Send for Reader {}
unsafe impl Sync for Reader {}

impl Reader {
    /// Create a base reader
    pub fn new(inner: impl AsyncRead + 'static) -> Self {
        cfg_if! {
            if #[cfg(feature = "progress")] {
                Self {
                    inner: Box::pin(inner),
                    progress: None,
                }
            } else {
                Self {
                    inner: Box::pin(inner),
                }
            }
        }
    }

    /// Create a reader that will report progress to an indicatif progress bar
    #[cfg(feature = "progress")]
    pub fn new_progress(inner: impl AsyncRead + 'static, progress: ProgressBar) -> Self {
        Self {
            inner: Box::pin(inner),
            progress: Some(progress),
        }
    }
}

impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                cfg_if! {
                    if #[cfg(feature = "progress")] {
                        if let Some(bar) = this.progress.as_mut() {
                            if buf.remaining() == 0 {
                                bar.inc(buf.filled().len() as u64);
                            }
                        }
                    }
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Implementation of AsyncWrite that writes a blob to a registry. This implementation
/// will automatically handle using chunked upload versus single upload based on the size
/// of the blob. Construction of this type is done by the Layer create methods.
pub struct Writer {
    uri: Uri,

    media_type: MediaType,
    upload_url: Option<String>,
    index: usize,
    size: usize,
    digest: Sha256,
    #[cfg(feature = "progress")]
    progress: Option<ProgressBar>,
    active: Option<Operation>,
}

enum Operation {
    Error(BoxFuture<'static, Result<Bytes, reqwest::Error>>),
    Start(BoxFuture<'static, crate::Result<Response>>),
    Upload(BoxFuture<'static, crate::Result<Response>>),
}

impl Writer {
    /// Construct a layer object out of this writer, this also will signal a finish to the progress
    /// bar in this writer if the feature is being used.
    pub async fn layer(&mut self) -> crate::Result<Layer> {
        let digest_bytes = self.digest.clone().finalize();
        let digest = base16::encode_lower(&digest_bytes);
        let digest = format!("sha256:{}", digest.clone());

        cfg_if! {
            if #[cfg(feature = "progress")] {
                if let Some(bar) = self.progress.as_mut() {
                    bar.finish_with_message("done");
                }
            }

        }
        Ok(Layer {
            media_type: self.media_type.clone(),
            digest: digest.clone(),
            size: self.index,
            platform: None,
        })
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        if let Some(operation) = this.active.as_mut() {
            match operation {
                Operation::Start(poll) => match poll.poll_unpin(cx) {
                    Poll::Ready(Ok(response)) => {
                        trace!(target: "layer", "RESPONSE {:?}", response);
                        this.active = None;
                        if !response.status().is_success() {
                            this.active = Some(Operation::Error(Box::pin(response.bytes())));
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                        this.upload_url = response
                            .headers()
                            .get("Location")
                            .and_then(|x| x.to_str().ok())
                            .map(|x| x.to_string());
                        trace!(target: "layer", "registry provided upload_url = {:?}", this.upload_url);
                        // We return pending here with a wake to ensure we write the first buf
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Poll::Ready(Err(e)) => Poll::Ready(Err(std::io::Error::other(e))),
                    Poll::Pending => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                },
                Operation::Upload(poll) => match poll.poll_unpin(cx) {
                    Poll::Ready(Ok(response)) => {
                        trace!(target: "layer", "RESPONSE {:?}", response);
                        this.active = None;
                        if response.status().is_success() {
                            cfg_if! {
                                if #[cfg(feature = "progress")] {
                                    if let Some(bar) = this.progress.as_mut() {
                                        bar.inc(buf.len() as u64);
                                    }
                                }
                            }
                            Poll::Ready(Ok(buf.len()))
                        } else {
                            this.active = Some(Operation::Error(Box::pin(response.bytes())));
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                    }
                    Poll::Ready(Err(e)) => Poll::Ready(Err(std::io::Error::other(e))),
                    Poll::Pending => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                },
                Operation::Error(poll) => match poll.poll_unpin(cx) {
                    Poll::Ready(Ok(response)) => {
                        this.active = None;
                        Poll::Ready(Err(std::io::Error::other(String::from_utf8_lossy(
                            response.as_ref(),
                        ))))
                    }
                    Poll::Ready(Err(e)) => Poll::Ready(Err(std::io::Error::other(e))),
                    Poll::Pending => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                },
            }
        } else if let Some(upload_url) = this.upload_url.as_ref() {
            if this.index + buf.len() >= this.size {
                // If our position plus the buffer we want to write is the end we should
                // finish the upload
                this.digest.update(buf);
                let hash = this.digest.clone().finalize();
                let digest = base16::encode_lower(hash.as_slice());
                let url = this.uri.registry().url().map_err(std::io::Error::other)?;
                this.active = Some(Operation::Upload(Box::pin(
                    this.uri.registry().client.clone().finish_blob_upload(
                        url,
                        upload_url.clone(),
                        Bytes::from_owner(buf.to_vec()),
                        format!("sha256:{digest}"),
                        this.index,
                        this.size,
                    ),
                )));
                this.index += buf.len();
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                // Otherwise we should send what we have as a patch
                let url = this.uri.registry().url().map_err(std::io::Error::other)?;
                this.active = Some(Operation::Upload(Box::pin(
                    this.uri.registry().client.clone().upload_part(
                        url,
                        upload_url.clone(),
                        Bytes::from_owner(buf.to_vec()),
                        this.index,
                        this.index + buf.len(),
                    ),
                )));
                this.index += buf.len();
                this.digest.update(buf);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        } else if buf.len() == this.size {
            // If we haven't started an upload and the passed buffer is equal to the size of the layer
            // we are writing, we can send a single post upload
            this.digest.update(buf);
            let hash = this.digest.clone().finalize();
            let digest = base16::encode_lower(hash.as_slice());
            let url = this.uri.registry().url().map_err(std::io::Error::other)?;
            this.active = Some(Operation::Upload(Box::pin(
                this.uri.registry().client.clone().post_blob(
                    url,
                    this.uri.repository().clone(),
                    Bytes::from_owner(buf.to_vec()),
                    format!("sha256:{digest}"),
                ),
            )));
            this.index = buf.len();
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            // If we have not started an upload we should do so now
            let url = this.uri.registry().url().map_err(std::io::Error::other)?;
            this.active = Some(Operation::Start(Box::pin(
                this.uri
                    .registry()
                    .client
                    .clone()
                    .start_upload(url, this.uri.repository().clone()),
            )));
            this.index = 0;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

use crate::digest::Digest as DigestRef;
use crate::error;
use crate::models::MediaType;
use crate::models::Platform;
use crate::progress::{NoopHandle, ProgressHandle, ProgressReporter};
use crate::uri::{Reference, Uri};
use bon::Builder;
use bytes::Bytes;
use futures::FutureExt;
use futures::future::BoxFuture;
use reqwest::Response;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snafu::{ResultExt, ensure};
use std::cmp::min;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio_util::io::StreamReader;
use url::Url;

/// Minimum chunk size for layer operations (5 MiB).
const MIN_CHUNK_SIZE: usize = 5 * 1024 * 1024;
/// Maximum chunk size for layer operations (100 MiB).
const MAX_CHUNK_SIZE: usize = 100 * 1024 * 1024;

/// A layer represents a blob or sub-object associated with an image.
///
/// Operations for reading or writing blobs operate off this object.
#[derive(Debug, Serialize, Deserialize, Clone, Builder)]
#[serde(rename_all = "camelCase")]
pub struct Layer {
    #[builder(into)]
    media_type: MediaType,
    #[builder(into)]
    size: usize,
    #[builder(into)]
    digest: String,
    #[builder(into)]
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<Platform>,
}

impl Layer {
    /// Perform a chunked copy of a layer from one reader to another.
    ///
    /// Any time you want to interact with a layer in a registry, it is
    /// recommended to use this method. While most OCI registry
    /// implementations do not need special handling to make the chunks of
    /// data sent uniform, certain implementations (e.g. ECR) work better
    /// when using more uniform chunked operations.
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
        // Chunk size: clamped to [MIN_CHUNK_SIZE, MAX_CHUNK_SIZE]; aims for
        // ~1/40th of the total to keep progress bars responsive.
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
            // Advance by the bytes actually read this iteration, not the
            // unclamped chunk size; otherwise the final partial chunk
            // would over-count and we'd terminate the loop early on
            // exact-size layers.
            index += read_size;
        }
        Ok(())
    }

    /// Create a new layer in a registry. Returns `Ok(None)` if the registry
    /// already contains a blob with the given digest.
    ///
    /// Pass `Some(reporter)` to attach progress; pass `None` to suppress.
    pub async fn create(
        uri: &Uri,
        media_type: &MediaType,
        size: usize,
        digest: Option<String>,
        progress: Option<&dyn ProgressReporter>,
    ) -> crate::Result<Option<Writer>> {
        if let Some(digest) = digest.as_ref() {
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
        let handle: Box<dyn ProgressHandle> = match progress {
            Some(reporter) => {
                let label = digest
                    .as_ref()
                    .and_then(|d| DigestRef::parse(d).ok())
                    .map(|d| format!("blob {} ->", d.short(9)))
                    .unwrap_or_else(|| "blob ->".to_string());
                reporter.start(size as u64, &label)
            }
            None => Box::new(NoopHandle),
        };
        Ok(Some(Writer {
            uri: uri.clone(),
            size,
            media_type: media_type.clone(),
            state: WriterState::Initial,
            digest: Sha256::new(),
            written: 0,
            buffer: Vec::new(),
            progress: handle,
        }))
    }

    /// Open a layer blob for reading. Pass `Some(reporter)` to attach
    /// progress; pass `None` to suppress.
    pub async fn open(
        &self,
        uri: &Uri,
        progress: Option<&dyn ProgressReporter>,
    ) -> crate::Result<Reader> {
        let (stream, content_length) = uri
            .registry()
            .fetch_blob(uri.repository(), self.digest.as_str())
            .await?;
        // #18: validate the registry's reported size up front when present.
        let expected = self.size as u64;
        if content_length != 0 && content_length != expected {
            return error::LayerSizeMismatchSnafu {
                expected: expected as usize,
                actual: content_length as usize,
            }
            .fail();
        }
        let handle: Box<dyn ProgressHandle> = match progress {
            Some(reporter) => {
                let label = DigestRef::parse(&self.digest)
                    .map(|d| format!("blob {} <-", d.short(9)))
                    .unwrap_or_else(|_| "blob <-".to_string());
                reporter.start(expected, &label)
            }
            None => Box::new(NoopHandle),
        };
        let stream_reader = StreamReader::new(stream);
        Ok(Reader::new(
            stream_reader,
            handle,
            Some(self.digest.clone()),
            Some(self.size),
        ))
    }

    /// Open a layer for reading at the specified URI (used for raw blob
    /// access without a parent layer descriptor).
    pub async fn open_uri(uri: &Uri) -> crate::Result<Reader> {
        ensure!(
            matches!(uri.reference(), Reference::Digest { .. }),
            error::DirectLoadBlobSnafu { uri: uri.clone() }
        );
        let digest = uri.reference().to_string();
        let (stream, _size) = uri
            .registry()
            .fetch_blob(uri.repository(), digest.as_str())
            .await?;
        Ok(Reader::new(
            StreamReader::new(stream),
            Box::new(NoopHandle),
            None,
            None,
        ))
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

/// Layer `AsyncRead` implementation with progress reporting and optional
/// digest/size verification.
pub struct Reader {
    inner: Pin<Box<dyn AsyncRead + Send + Sync>>,
    progress: Box<dyn ProgressHandle>,
    /// Expected digest of the streamed bytes; verified at EOF when set.
    expected_digest: Option<String>,
    expected_size: Option<usize>,
    hasher: Sha256,
    bytes_read: usize,
    /// Set to true after we've validated the final digest at EOF, so we
    /// don't run validation twice.
    finalized: bool,
}

impl Reader {
    /// Create a new reader. The progress handle is always present; pass
    /// [`Box::new(NoopHandle)`] when progress is not desired.
    pub fn new(
        inner: impl AsyncRead + Send + Sync + 'static,
        progress: Box<dyn ProgressHandle>,
        expected_digest: Option<String>,
        expected_size: Option<usize>,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            progress,
            expected_digest,
            expected_size,
            hasher: Sha256::new(),
            bytes_read: 0,
            finalized: false,
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.progress.finish();
    }
}

impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match this.inner.as_mut().poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let after = buf.filled().len();
                let delta = after - before;
                if delta > 0 {
                    let new_chunk = &buf.filled()[before..after];
                    this.hasher.update(new_chunk);
                    this.bytes_read += delta;
                    if let Some(expected) = this.expected_size
                        && this.bytes_read > expected
                    {
                        return Poll::Ready(Err(std::io::Error::other(format!(
                            "registry returned more bytes than declared (expected {expected}, got {})",
                            this.bytes_read
                        ))));
                    }
                    this.progress.inc(delta as u64);
                } else if !this.finalized {
                    // EOF: validate digest and final size.
                    this.finalized = true;
                    if let Some(expected) = this.expected_size
                        && this.bytes_read != expected
                    {
                        return Poll::Ready(Err(std::io::Error::other(format!(
                            "short layer read (expected {expected}, got {})",
                            this.bytes_read
                        ))));
                    }
                    if let Some(expected) = this.expected_digest.as_ref() {
                        let computed = format!(
                            "sha256:{}",
                            base16::encode_lower(this.hasher.clone().finalize().as_slice())
                        );
                        if computed != *expected {
                            return Poll::Ready(Err(std::io::Error::other(format!(
                                "layer digest mismatch: expected {expected}, computed {computed}"
                            ))));
                        }
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// `AsyncWrite` implementation that writes a blob to a registry.
///
/// The state machine progresses Initial → Starting → Idle ↔ Uploading →
/// Finishing → Done. Hash and offset state advance only after the registry
/// confirms each chunk, so a failed PATCH does not leave callers with a
/// digest computed from bytes that aren't actually durable.
pub struct Writer {
    uri: Uri,
    media_type: MediaType,
    /// Total expected blob size (from layer descriptor).
    size: usize,
    /// Bytes the registry has confirmed accepted.
    written: usize,
    digest: Sha256,
    state: WriterState,
    /// Buffer of bytes that have been logically accepted from the caller
    /// but not yet flushed/uploaded.
    buffer: Vec<u8>,
    progress: Box<dyn ProgressHandle>,
}

enum WriterState {
    /// No upload has been initiated yet.
    Initial,
    /// POST {repo}/blobs/uploads/ is in flight to start the upload session.
    Starting(BoxFuture<'static, crate::Result<Response>>),
    /// We have an upload URL and are buffering bytes in `buffer`.
    Idle { upload_url: Url },
    /// PATCH is in flight; on success advance offset and update upload URL.
    Uploading {
        fut: BoxFuture<'static, crate::Result<Response>>,
        chunk_len: usize,
    },
    /// PUT (final) is in flight. `pending_advance` is the number of bytes
    /// in this final body that have not yet been credited to `written` or to
    /// the progress bar; we advance both only after the registry confirms.
    Finishing {
        fut: BoxFuture<'static, crate::Result<Response>>,
        pending_advance: usize,
    },
    /// Drained successfully and the blob is durable.
    Done,
    /// Sticky failure state; further polls return the saved error string.
    Failed(String),
}

impl Writer {
    /// Finalize this writer into a [`Layer`]. Must be called after
    /// `shutdown()` has returned `Ready(Ok)`.
    pub async fn layer(&mut self) -> crate::Result<Layer> {
        if !matches!(self.state, WriterState::Done) {
            return Err(error::Error::LayerWrite {
                source: std::io::Error::other("writer.layer() called before shutdown"),
            });
        }
        let digest_bytes = self.digest.clone().finalize();
        let digest = format!("sha256:{}", base16::encode_lower(&digest_bytes));
        self.progress.finish();
        Ok(Layer {
            media_type: self.media_type.clone(),
            digest,
            size: self.written,
            platform: None,
        })
    }

    fn fail<E: std::fmt::Display>(&mut self, e: E) -> std::io::Error {
        let s = e.to_string();
        self.state = WriterState::Failed(s.clone());
        std::io::Error::other(s)
    }

    /// Returns true if the buffer is large enough to flush eagerly.
    fn buffer_ready_to_flush(&self) -> bool {
        self.buffer.len() >= MIN_CHUNK_SIZE
    }

    /// Drive an in-flight upload future. On success, advance offsets and
    /// transition back to `Idle`. On failure, transition to `Failed`.
    fn poll_uploading(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let WriterState::Uploading { fut, chunk_len } = &mut self.state else {
            unreachable!("poll_uploading called outside Uploading state");
        };
        match fut.poll_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(response)) => {
                let chunk_len = *chunk_len;
                if !response.status().is_success() {
                    let status = response.status();
                    let err = self.fail(format!("registry rejected chunk: {status}"));
                    return Poll::Ready(Err(err));
                }
                // Resolve next upload URL from Location.
                let next_url = match crate::client::extract_location(&response, &self.current_url())
                {
                    Ok(u) => u,
                    Err(e) => {
                        let err = self.fail(e);
                        return Poll::Ready(Err(err));
                    }
                };
                self.written += chunk_len;
                self.progress.inc(chunk_len as u64);
                self.state = WriterState::Idle {
                    upload_url: next_url,
                };
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                let err = self.fail(e);
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_finishing(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let WriterState::Finishing {
            fut,
            pending_advance,
        } = &mut self.state
        else {
            unreachable!("poll_finishing called outside Finishing state");
        };
        match fut.poll_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(response)) => {
                if !response.status().is_success() {
                    let status = response.status();
                    let err = self.fail(format!("registry rejected blob finalize: {status}"));
                    return Poll::Ready(Err(err));
                }
                // Only credit the durable bytes once the registry confirms
                // the final PUT succeeded.
                let advance = *pending_advance;
                self.written += advance;
                self.progress.inc(advance as u64);
                self.state = WriterState::Done;
                self.progress.finish();
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                let err = self.fail(e);
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_starting(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let WriterState::Starting(fut) = &mut self.state else {
            unreachable!("poll_starting called outside Starting state");
        };
        match fut.poll_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(response)) => {
                if !response.status().is_success() {
                    let status = response.status();
                    let err = self.fail(format!("registry rejected upload start: {status}"));
                    return Poll::Ready(Err(err));
                }
                let url = match crate::client::extract_location(&response, &self.current_url()) {
                    Ok(u) => u,
                    Err(e) => {
                        let err = self.fail(e);
                        return Poll::Ready(Err(err));
                    }
                };
                self.state = WriterState::Idle { upload_url: url };
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                let err = self.fail(e);
                Poll::Ready(Err(err))
            }
        }
    }

    fn current_url(&self) -> Url {
        // Best-effort base URL for resolving Location headers; we use the
        // original registry URL because that's what reqwest sent the
        // request to. Errors here mean the registry URI itself is broken,
        // which is fatal anyway.
        self.uri
            .registry()
            .url()
            .unwrap_or_else(|_| Url::parse("http://invalid.local/").expect("invariant"))
    }

    /// Kick off a PATCH for the buffered chunk. Caller guarantees that
    /// `self.state` is `Idle` and the buffer is non-empty.
    fn launch_patch(&mut self) {
        let WriterState::Idle { upload_url } =
            std::mem::replace(&mut self.state, WriterState::Done)
        else {
            unreachable!("launch_patch called outside Idle state");
        };
        let chunk = std::mem::take(&mut self.buffer);
        let chunk_len = chunk.len();
        // Hash the durable bytes; offset advance happens after the
        // registry confirms the PATCH succeeded (#15).
        self.digest.update(&chunk);
        let bytes = Bytes::from(chunk);
        let start = self.written;
        let end = start + chunk_len;
        let client = self.uri.registry().client.clone();
        let upload_url_for_fut = upload_url.clone();
        let fut = async move {
            client
                .upload_part(upload_url_for_fut, bytes, start, end)
                .await
        }
        .boxed();
        self.state = WriterState::Uploading { fut, chunk_len };
    }

    /// Kick off the final PUT. Caller guarantees Idle state.
    fn launch_finish(&mut self) {
        let WriterState::Idle { upload_url } =
            std::mem::replace(&mut self.state, WriterState::Done)
        else {
            unreachable!("launch_finish called outside Idle state");
        };
        let chunk = std::mem::take(&mut self.buffer);
        let chunk_len = chunk.len();
        if chunk_len > 0 {
            self.digest.update(&chunk);
        }
        let digest_bytes = self.digest.clone().finalize();
        let digest = format!("sha256:{}", base16::encode_lower(&digest_bytes));
        let bytes = Bytes::from(chunk);
        let start = self.written;
        let end = start + chunk_len;
        let client = self.uri.registry().client.clone();
        let fut = async move {
            client
                .finish_blob_upload(upload_url, bytes, digest, start, end)
                .await
        }
        .boxed();
        // Defer offset/progress advance to poll_finishing on success so that
        // failed finalize attempts don't leave a corrupt accounting state.
        self.state = WriterState::Finishing {
            fut,
            pending_advance: chunk_len,
        };
    }

    /// Kick off a monolithic POST (single-shot upload). Caller guarantees
    /// the buffer holds the entire blob and we are in Initial state.
    fn launch_monolithic_post(&mut self) {
        let chunk = std::mem::take(&mut self.buffer);
        let chunk_len = chunk.len();
        self.digest.update(&chunk);
        let digest_bytes = self.digest.clone().finalize();
        let digest = format!("sha256:{}", base16::encode_lower(&digest_bytes));
        let bytes = Bytes::from(chunk);
        let registry_url = match self.uri.registry().url() {
            Ok(u) => u,
            Err(e) => {
                self.state = WriterState::Failed(e.to_string());
                return;
            }
        };
        let repository = self.uri.repository().clone();
        let client = self.uri.registry().client.clone();
        let fut = async move {
            client
                .post_blob(registry_url, repository, bytes, digest)
                .await
        }
        .boxed();
        // Defer offset/progress advance until poll_finishing confirms success.
        self.state = WriterState::Finishing {
            fut,
            pending_advance: chunk_len,
        };
    }

    /// Kick off a session-start POST. Caller guarantees Initial state.
    fn launch_start(&mut self) {
        let registry_url = match self.uri.registry().url() {
            Ok(u) => u,
            Err(e) => {
                self.state = WriterState::Failed(e.to_string());
                return;
            }
        };
        let repository = self.uri.repository().clone();
        let client = self.uri.registry().client.clone();
        let fut = async move { client.start_upload(registry_url, repository).await }.boxed();
        self.state = WriterState::Starting(fut);
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        // Drive any pending async work to readiness first; that may
        // transition us back to Idle so we can accept more bytes.
        loop {
            match &this.state {
                WriterState::Failed(reason) => {
                    return Poll::Ready(Err(std::io::Error::other(reason.clone())));
                }
                WriterState::Done => {
                    return Poll::Ready(Err(std::io::Error::other(
                        "writer is closed; further writes are rejected",
                    )));
                }
                WriterState::Starting(_) => match this.poll_starting(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Uploading { .. } => match this.poll_uploading(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Finishing { .. } => match this.poll_finishing(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Initial | WriterState::Idle { .. } => break,
            }
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Bound how much we accept this call so the buffer doesn't grow
        // unbounded in one shot; the caller will simply be invoked again.
        let to_take = buf.len().min(MAX_CHUNK_SIZE);
        this.buffer.extend_from_slice(&buf[..to_take]);
        // Decide whether to fire an upload now or accept and yield.
        match this.state {
            WriterState::Initial => {
                // Have we now buffered the full blob? Then a single POST
                // suffices. Otherwise we need a session-start.
                if this.buffer.len() >= this.size && this.size > 0 {
                    this.launch_monolithic_post();
                } else if this.buffer_ready_to_flush() {
                    this.launch_start();
                }
            }
            WriterState::Idle { .. } => {
                if this.buffer_ready_to_flush() {
                    this.launch_patch();
                }
            }
            _ => unreachable!("only Initial/Idle reachable post-drive"),
        }
        Poll::Ready(Ok(to_take))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        loop {
            match &this.state {
                WriterState::Failed(reason) => {
                    return Poll::Ready(Err(std::io::Error::other(reason.clone())));
                }
                WriterState::Done => return Poll::Ready(Ok(())),
                WriterState::Starting(_) => match this.poll_starting(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Uploading { .. } => match this.poll_uploading(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Finishing { .. } => match this.poll_finishing(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Initial | WriterState::Idle { .. } => return Poll::Ready(Ok(())),
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        loop {
            match &this.state {
                WriterState::Failed(reason) => {
                    return Poll::Ready(Err(std::io::Error::other(reason.clone())));
                }
                WriterState::Done => return Poll::Ready(Ok(())),
                WriterState::Starting(_) => match this.poll_starting(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Uploading { .. } => match this.poll_uploading(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Finishing { .. } => match this.poll_finishing(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                },
                WriterState::Initial => {
                    // No upload was started yet. Always issue a (possibly
                    // zero-length) monolithic POST rather than assuming an
                    // empty blob is already present on the registry: the
                    // digest was already confirmed absent by `check_blob`
                    // in `Layer::create`, so skipping the request here
                    // would mark the writer `Done` without ever actually
                    // persisting the blob.
                    this.launch_monolithic_post();
                    continue;
                }
                WriterState::Idle { .. } => {
                    // Drain any remaining buffer through PUT.
                    this.launch_finish();
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// Reader that returns short reads; verifies `Layer::copy` advances by
    /// the actually-read amount, not by the unclamped chunk size.
    struct ShortReader {
        data: Vec<u8>,
        pos: usize,
    }

    impl AsyncRead for ShortReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let remaining = this.data.len() - this.pos;
            // Always serve in tiny pieces.
            let take = remaining.min(7).min(buf.remaining());
            buf.put_slice(&this.data[this.pos..this.pos + take]);
            this.pos += take;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn copy_handles_short_reads() {
        let data = vec![0xABu8; 31];
        let mut src = ShortReader {
            data: data.clone(),
            pos: 0,
        };
        let mut dst = Vec::new();
        // Use a small "size" to force tight loop iterations.
        Layer::copy(&mut src, &mut dst, data.len()).await.unwrap();
        assert_eq!(dst, data);
    }

    #[tokio::test]
    async fn reader_finalizes_digest_on_eof_when_present() {
        // Empty stream with empty-data sha256.
        let empty_digest =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let mut r = Reader::new(
            tokio::io::empty(),
            Box::new(NoopHandle),
            Some(empty_digest.to_string()),
            Some(0),
        );
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn reader_detects_digest_mismatch() {
        let bad_digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let mut r = Reader::new(
            tokio::io::empty(),
            Box::new(NoopHandle),
            Some(bad_digest.to_string()),
            Some(0),
        );
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).await.unwrap_err();
        assert!(err.to_string().contains("digest mismatch"));
    }
}

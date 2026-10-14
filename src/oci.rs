//! Export of images and image indexes as OCI image layout archives.
//!
//! Spec: <https://github.com/opencontainers/image-spec/blob/main/image-layout.md>

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_recursion::async_recursion;
use bon::Builder;
use futures::future::try_join_all;
use snafu::{ResultExt, ensure};
use tempfile::{TempDir, tempdir};
use tokio::fs::{File, create_dir_all};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_tar::Builder as ArchiveBuilder;

use crate::digest::Digest;
use crate::error;
use crate::image::Image;
use crate::index::Index;
use crate::layer::{Layer, digest_bytes};
use crate::manifest::Manifest;
use crate::models::Platform;
use crate::progress::SharedProgress;
use crate::uri::{Reference, Uri};

/// Annotation key naming a reference in an image layout's `index.json`.
pub const ANNOTATION_REF_NAME: &str = "org.opencontainers.image.ref.name";

/// Options controlling how an OCI image layout archive is written.
///
/// ```
/// use ocilot::oci::OciOptions;
///
/// let options = OciOptions::builder().annotate_ref(true).build();
/// assert!(options.annotate_ref());
/// ```
#[derive(Debug, Clone, Default, Builder)]
pub struct OciOptions {
    /// Store only the image matching this platform instead of the full index.
    platform: Option<Platform>,
    /// Record the reference that was pulled as the
    /// `org.opencontainers.image.ref.name` annotation on the `index.json`
    /// entry (equivalent to crane's `--annotate-ref`).
    #[builder(default)]
    annotate_ref: bool,
}

impl OciOptions {
    /// Platform to select, if any
    pub fn platform(&self) -> Option<&Platform> {
        self.platform.as_ref()
    }

    /// Whether the pulled reference is recorded as an annotation
    pub fn annotate_ref(&self) -> bool {
        self.annotate_ref
    }
}

/// Build a uri addressing `digest` within the same registry and repository as `uri`.
pub(crate) fn digest_uri(uri: &Uri, digest: &str) -> crate::Result<Uri> {
    Ok(Uri::builder()
        .registry(uri.registry().clone())
        .repository(uri.repository())
        .reference(Reference::from_str(digest)?)
        .build())
}

/// Digest for the root manifest bytes, using sha256 for tag references.
/// For digest references the bytes must hash to the requested digest.
pub(crate) fn root_digest(uri: &Uri, bytes: &[u8]) -> crate::Result<String> {
    match uri.reference() {
        Reference::Digest { .. } => {
            let expected = uri.reference().to_string();
            let actual = digest_bytes(Some(&expected), bytes)?;
            ensure!(
                actual == expected,
                error::DigestMismatchSnafu { expected, actual }
            );
            Ok(actual)
        }
        Reference::Tag(_) => digest_bytes(None, bytes),
    }
}

/// The descriptor written to `index.json`, annotated per `options`.
fn root_descriptor(uri: &Uri, mut root: Layer, options: &OciOptions) -> Layer {
    if options.annotate_ref() {
        root.set_annotation(ANNOTATION_REF_NAME, uri.to_string());
    }
    root
}

/// Path of the blob for `digest` under the layout at `root`, creating the
/// algorithm directory as needed.
async fn blob_path(root: &Path, digest: &str) -> crate::Result<PathBuf> {
    let digest = Digest::parse(digest)?;
    let dir = root.join("blobs").join(digest.algorithm());
    create_dir_all(&dir).await.context(error::DirectorySnafu)?;
    Ok(dir.join(digest.hex()))
}

/// An OCI image layout staged in a temporary directory.
pub(crate) struct OciLayout {
    dir: TempDir,
    /// Digests of blobs already stored, so shared content is written once.
    written: HashSet<String>,
}

impl OciLayout {
    pub(crate) async fn new() -> crate::Result<Self> {
        let dir = tempdir().context(error::TempSnafu)?;
        tokio::fs::write(
            dir.path().join("oci-layout"),
            r#"{ "imageLayoutVersion": "1.0.0" }"#,
        )
        .await
        .context(error::FileSnafu)?;
        Ok(Self {
            dir,
            written: HashSet::new(),
        })
    }

    /// Store a manifest blob, verifying its bytes match the digest.
    async fn write_manifest(&mut self, digest: &str, bytes: &[u8]) -> crate::Result<()> {
        if self.written.contains(digest) {
            return Ok(());
        }
        let actual = digest_bytes(Some(digest), bytes)?;
        ensure!(
            actual == digest,
            error::DigestMismatchSnafu {
                expected: digest,
                actual,
            }
        );
        tokio::fs::write(blob_path(self.dir.path(), digest).await?, bytes)
            .await
            .context(error::FileSnafu)?;
        self.written.insert(digest.to_string());
        Ok(())
    }

    /// Download blobs from the registry concurrently.
    async fn write_blobs(
        &mut self,
        uri: &Uri,
        blobs: impl Iterator<Item = &Layer>,
        progress: &SharedProgress,
    ) -> crate::Result<()> {
        let mut tasks: Vec<JoinHandle<crate::Result<()>>> = Vec::new();
        for blob in blobs {
            if !self.written.insert(blob.digest().to_string()) {
                continue;
            }
            let blob = blob.clone();
            let uri = uri.clone();
            let root = self.dir.path().to_path_buf();
            let progress = progress.clone();
            tasks.push(tokio::spawn(async move {
                let mut reader = blob.open(&uri, progress.as_ref()).await?;
                let mut file = File::create(blob_path(&root, blob.digest()).await?)
                    .await
                    .context(error::FileSnafu)?;
                Layer::copy(&mut reader, &mut file, blob.size()).await?;
                reader.verify().await?;
                file.flush().await.context(error::FileSnafu)
            }));
        }
        // try_join_all aborts on first error and surfaces JoinErrors
        // through the typed `Error::Join` variant.
        try_join_all(tasks)
            .await
            .context(error::JoinSnafu)?
            .into_iter()
            .collect::<crate::Result<Vec<_>>>()?;
        Ok(())
    }

    /// Store an image manifest along with its config and layers.
    pub(crate) async fn write_image(
        &mut self,
        uri: &Uri,
        image: &Image,
        digest: &str,
        progress: &SharedProgress,
    ) -> crate::Result<()> {
        let blobs = std::iter::once(image.config()).chain(image.layers().iter());
        self.write_blobs(uri, blobs, progress).await?;
        self.write_manifest(digest, &image.manifest_bytes()?).await
    }

    /// Store an index along with everything it references, recursing into
    /// nested indexes.
    #[async_recursion]
    pub(crate) async fn write_index(
        &mut self,
        uri: &Uri,
        index: &Index,
        digest: &str,
        progress: &SharedProgress,
    ) -> crate::Result<()> {
        for child in index.manifests() {
            if self.written.contains(child.digest()) {
                continue;
            }
            match Manifest::fetch(&digest_uri(uri, child.digest())?).await? {
                Manifest::Index(nested) => {
                    self.write_index(uri, &nested, child.digest(), progress)
                        .await?
                }
                Manifest::Image(image) => {
                    self.write_image(uri, &image, child.digest(), progress)
                        .await?
                }
            }
        }
        self.write_manifest(digest, &index.manifest_bytes()?).await
    }

    /// Write `index.json` referencing `root` and archive the layout to `output`.
    pub(crate) async fn finish<W>(
        self,
        uri: &Uri,
        root: Layer,
        options: &OciOptions,
        output: W,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let index = Index::new(&[root_descriptor(uri, root, options)]).await;
        let index_content = serde_json::to_vec(&index).context(error::SerializeSnafu)?;
        tokio::fs::write(self.dir.path().join("index.json"), &index_content)
            .await
            .context(error::FileSnafu)?;

        let mut archive = ArchiveBuilder::new(output);
        archive
            .append_dir_all(".", self.dir.path().to_path_buf())
            .await
            .context(error::ArchiveSnafu)?;
        archive.finish().await.context(error::ArchiveSnafu)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MediaType;

    fn descriptor() -> Layer {
        Layer::builder()
            .media_type(MediaType::ImageIndex)
            .size(42usize)
            .digest("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
            .build()
    }

    async fn uri() -> Uri {
        Uri::new("localhost:5000/library/alpine:latest")
            .await
            .unwrap()
    }

    #[test]
    fn options_default_does_not_annotate() {
        let options = OciOptions::default();
        assert!(!options.annotate_ref());
        assert!(options.platform().is_none());
        assert!(!OciOptions::builder().build().annotate_ref());
    }

    #[tokio::test]
    async fn root_descriptor_without_annotation() {
        let root = root_descriptor(&uri().await, descriptor(), &OciOptions::default());
        let value = serde_json::to_value(&root).unwrap();
        assert!(value.get("annotations").is_none());
    }

    #[tokio::test]
    async fn root_descriptor_annotates_ref() {
        let options = OciOptions::builder().annotate_ref(true).build();
        let root = root_descriptor(&uri().await, descriptor(), &options);
        let value = serde_json::to_value(&root).unwrap();
        assert_eq!(
            value["annotations"][ANNOTATION_REF_NAME],
            "localhost:5000/library/alpine:latest"
        );
    }

    #[tokio::test]
    async fn root_digest_matches_sha256() {
        let digest = root_digest(&uri().await, b"").unwrap();
        assert_eq!(
            digest,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn root_digest_rejects_mismatched_digest_reference() {
        let empty = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let uri = Uri::new(&format!("localhost:5000/library/alpine@{empty}"))
            .await
            .unwrap();
        assert_eq!(root_digest(&uri, b"").unwrap(), empty);
        assert!(root_digest(&uri, b"not empty").is_err());
    }

    #[tokio::test]
    async fn write_manifest_rejects_mismatched_digest() {
        let mut layout = OciLayout::new().await.unwrap();
        let digest = descriptor().digest().to_string();
        assert!(layout.write_manifest(&digest, b"not empty").await.is_err());
        layout.write_manifest(&digest, b"").await.unwrap();
        assert!(
            layout
                .dir
                .path()
                .join("blobs/sha256")
                .join(Digest::parse(&digest).unwrap().hex())
                .exists()
        );
    }
}

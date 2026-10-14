use std::str::FromStr;

use crate::error;
use crate::image::Image;
use crate::layer::Layer;
use crate::models::MediaType;
use crate::models::Platform;
use crate::oci::{OciLayout, OciOptions, digest_uri, root_digest};
use crate::progress::SharedProgress;
use crate::uri::{Reference, Uri};
use bon::Builder;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use tokio::io::AsyncWrite;

/// Represents an Image Index and handles all operations that require or utilize one.
#[derive(Debug, Serialize, Deserialize, Clone, Builder)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    #[builder(into)]
    schema_version: usize,
    #[builder(into)]
    media_type: MediaType,
    #[builder(into)]
    manifests: Vec<Layer>,
    /// Exact index bytes as served by the registry.
    #[builder(skip)]
    #[serde(skip)]
    raw: Option<Bytes>,
}

impl Index {
    /// Create a new image index with the provided manifests
    pub async fn new(manifests: &[Layer]) -> Self {
        Self {
            schema_version: 2,
            media_type: MediaType::ImageIndex,
            manifests: manifests.to_vec(),
            raw: None,
        }
    }

    /// Check if there is an image index at the provided URI.
    ///
    /// Note: This only checks that a manifest exists so it could return a false
    /// positive as it does not verify the media type of the manifest to ensure
    /// it is an index.
    pub async fn check(uri: &Uri) -> crate::Result<bool> {
        uri.registry()
            .check_manifest(uri.repository(), uri.reference().to_string().as_str())
            .await
    }

    /// Fetch an image index from a registry
    pub async fn fetch(uri: &Uri) -> crate::Result<Self> {
        let bytes = uri
            .registry()
            .fetch_manifest_bytes(uri.repository(), uri.reference().to_string().as_str())
            .await?;
        let mut me: Self = serde_json::from_slice(&bytes).context(error::BodyDeserializeSnafu)?;
        me.raw = Some(bytes);
        Ok(me)
    }

    /// Record the exact bytes this index was parsed from.
    pub(crate) fn set_raw(&mut self, raw: Bytes) {
        self.raw = Some(raw);
    }

    /// Index bytes: the original bytes when available, otherwise this index
    /// serialized.
    pub fn manifest_bytes(&self) -> crate::Result<Bytes> {
        match &self.raw {
            Some(raw) => Ok(raw.clone()),
            None => Ok(serde_json::to_vec(self)
                .context(error::SerializeSnafu)?
                .into()),
        }
    }

    /// Schema version
    pub fn schema_version(&self) -> usize {
        self.schema_version
    }

    /// Media type
    pub fn media_type(&self) -> &MediaType {
        &self.media_type
    }

    /// Manifest layers included in this index
    pub fn manifests(&self) -> &[Layer] {
        self.manifests.as_slice()
    }

    /// Fetch an image from this index.
    ///
    /// If a platform is provided, looks for the first matching image. If not
    /// provided, loads an image matching the current running environment or
    /// the first image in the index.
    pub async fn fetch_image(
        &self,
        uri: &Uri,
        platform: Option<Platform>,
    ) -> crate::Result<Option<Image>> {
        if let Some(platform) = platform {
            let oci = self
                .manifests
                .iter()
                .find(|x| x.platform() == Some(platform.clone()))
                .context(error::IndexNoPlatformSnafu {
                    platform: platform.clone(),
                })?;
            // Use the digest
            let new_uri = Uri::builder()
                .registry(uri.registry().clone())
                .repository(uri.repository())
                .reference(Reference::from_str(oci.digest())?)
                .build();
            Ok(Some(Image::fetch(&new_uri, Some(platform)).await?))
        } else {
            // See if we can match by architecture
            let current = Platform::default();
            if let Some(oci) = self
                .manifests
                .iter()
                .find(|x| x.platform() == Some(current.clone()))
            {
                // Use the digest
                let new_uri = Uri::builder()
                    .registry(uri.registry().clone())
                    .repository(uri.repository())
                    .reference(Reference::from_str(oci.digest())?)
                    .build();
                return Ok(Some(Image::fetch(&new_uri, Some(current.clone())).await?));
            }
            // Otherwise we return the first image
            if let Some(oci) = self.manifests.first() {
                // Use the digest
                let new_uri = Uri::builder()
                    .registry(uri.registry().clone())
                    .repository(uri.repository())
                    .reference(Reference::from_str(oci.digest())?)
                    .build();
                Ok(Some(Image::fetch(&new_uri, oci.platform()).await?))
            } else {
                Ok(None)
            }
        }
    }

    /// Push this image index to a registry. Indexes that were fetched or
    /// read are pushed with their original bytes so their digest is kept.
    pub async fn push(&self, uri: &Uri) -> crate::Result<()> {
        uri.registry()
            .push_manifest(
                &self.media_type,
                uri.repository(),
                uri.reference().to_string().as_str(),
                self.manifest_bytes()?,
                None,
            )
            .await?;
        Ok(())
    }

    /// Write this index out as an OCI image layout tar archive.
    ///
    /// The archive's `index.json` holds a single descriptor. Without a
    /// platform in `options` it points at this index (stored as a blob along
    /// with every image it references). With a platform it points directly at
    /// the matching image manifest and only that image is stored. Pass a
    /// populated [`SharedProgress`] for progress reporting or
    /// [`SharedProgress::none`] for silent operation.
    pub async fn to_oci<W>(
        &self,
        uri: &Uri,
        output: W,
        progress: SharedProgress,
        options: &OciOptions,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut layout = OciLayout::new().await?;
        let root = match options.platform() {
            Some(platform) => {
                // Only the selected image is stored, but the index naming it
                // must still match a requested digest.
                root_digest(uri, &self.manifest_bytes()?)?;
                let descriptor = self
                    .manifests
                    .iter()
                    .find(|x| x.platform().as_ref() == Some(platform))
                    .context(error::IndexNoPlatformSnafu {
                        platform: platform.clone(),
                    })?
                    .clone();
                let image_uri = digest_uri(uri, descriptor.digest())?;
                let image = Image::fetch(&image_uri, Some(platform.clone())).await?;
                layout
                    .write_image(uri, &image, descriptor.digest(), &progress)
                    .await?;
                descriptor
            }
            None => {
                let bytes = self.manifest_bytes()?;
                let digest = root_digest(uri, &bytes)?;
                layout.write_index(uri, self, &digest, &progress).await?;
                Layer::builder()
                    .media_type(self.media_type.clone())
                    .size(bytes.len())
                    .digest(digest)
                    .build()
            }
        };
        layout.finish(uri, root, options, output).await
    }
}

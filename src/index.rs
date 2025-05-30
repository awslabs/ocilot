use std::str::FromStr;

use crate::error;
use crate::image::Image;
use crate::layer::Layer;
use crate::models::MediaType;
use crate::models::Platform;
use crate::uri::{Reference, Uri, UriBuilder};
use derive_builder::Builder;
use futures::future::join_all;
#[cfg(feature = "progress")]
use indicatif::MultiProgress;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use tempfile::tempdir;
use tokio::fs::{create_dir_all, File};
use tokio::io::AsyncWrite;
use tokio::task::JoinHandle;
use tokio_tar::Builder as ArchiveBuilder;

/// Represents an Image Index and handles all operations that require or utilize one
#[derive(Debug, Serialize, Deserialize, Clone, Builder)]
#[builder(setter(into))]
#[serde(rename_all = "camelCase")]
pub struct Index {
    schema_version: usize,
    media_type: MediaType,
    manifests: Vec<Layer>,
}

impl Index {
    /// Create a new image index with the provided manifests
    pub async fn new(manifests: &[Layer]) -> Self {
        Self {
            schema_version: 2,
            media_type: MediaType::ImageIndex,
            manifests: manifests.to_vec(),
        }
    }

    /// Check if their is an image index at the provided uri.
    /// Note: This only checks that a manifest exists so it could return a false positive
    /// as it does not verify the media type of the manifest to ensure it is an index
    pub async fn check(uri: &Uri) -> crate::Result<bool> {
        uri.registry()
            .check_manifest(uri.repository(), uri.reference().to_string().as_str())
            .await
    }

    /// Fetch an image index from a registry
    pub async fn fetch(uri: &Uri) -> crate::Result<Self> {
        uri.registry()
            .fetch_manifest(uri.repository(), uri.reference().to_string().as_str())
            .await
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

    /// Fetch an image in this index if a platform is provided it will look for the first matching image with the platform.
    /// If a platform is not provided it will either load an image matching the platform matching the current running environment or
    /// the first image in the index
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
            let new_uri = UriBuilder::default()
                .registry(uri.registry().clone())
                .repository(uri.repository())
                .reference(Reference::from_str(oci.digest())?)
                .build()
                .context(error::UriSnafu)?;
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
                let new_uri = UriBuilder::default()
                    .registry(uri.registry().clone())
                    .repository(uri.repository())
                    .reference(Reference::from_str(oci.digest())?)
                    .build()
                    .context(error::UriSnafu)?;
                return Ok(Some(Image::fetch(&new_uri, Some(current.clone())).await?));
            }
            // Otherwise we return the first image
            if let Some(oci) = self.manifests.first() {
                // Use the digest
                let new_uri = UriBuilder::default()
                    .registry(uri.registry().clone())
                    .repository(uri.repository())
                    .reference(Reference::from_str(oci.digest())?)
                    .build()
                    .context(error::UriSnafu)?;
                Ok(Some(Image::fetch(&new_uri, oci.platform().clone()).await?))
            } else {
                Ok(None)
            }
        }
    }

    /// Push this image index to a registry
    pub async fn push(&self, uri: &Uri) -> crate::Result<()> {
        uri.registry()
            .push_manifest(
                &self.media_type,
                uri.repository(),
                uri.reference().to_string().as_str(),
                self,
                None,
            )
            .await?;
        Ok(())
    }

    /// Create an OCI tar archive that contains either all of the index images (if no platform provided)
    /// or only the platforms specified
    pub async fn to_oci<W>(
        &self,
        uri: &Uri,
        platform: Option<Platform>,
        output: W,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let tmp_dir = tempdir().context(error::TempSnafu)?;
        tokio::fs::write(
            tmp_dir.path().join("oci-layout"),
            r#"{ "imageLayoutVersion": "1.0.0" }"#,
        )
        .await
        .context(error::FileSnafu)?;

        let blob_dir = tmp_dir.path().join("blobs/sha256");
        create_dir_all(&blob_dir)
            .await
            .context(error::DirectorySnafu)?;

        // Start with ourselves for the index
        let mut index = self.clone();
        if let Some(platform) = platform {
            // If we are selecting only a single platform then filter the manifests down
            index.manifests = index
                .manifests
                .iter()
                .filter(|x| x.platform() == Some(platform.clone()))
                .cloned()
                .collect::<Vec<Layer>>();
            if index.manifests.is_empty() {
                return error::IndexNoPlatformSnafu { platform }.fail();
            }
        }
        let index_content = serde_json::to_string(&index).context(error::SerializeSnafu)?;
        tokio::fs::write(tmp_dir.path().join("index.json"), &index_content)
            .await
            .context(error::FileSnafu)?;

        // Now for every manifest we are working with we need to store it out
        for manifest in index.manifests.iter() {
            let image_uri = UriBuilder::default()
                .registry(uri.registry().clone())
                .repository(uri.repository())
                .reference(Reference::from_str(manifest.digest())?)
                .build()
                .context(error::UriSnafu)?;
            let image = Image::fetch(&image_uri, manifest.platform().clone()).await?;
            // Write the image manifest as a blob
            let manifest_bytes = serde_json::to_string(&image).context(error::SerializeSnafu)?;
            tokio::fs::write(
                blob_dir.join(manifest.digest().strip_prefix("sha256:").unwrap()),
                &manifest_bytes,
            )
            .await
            .context(error::FileSnafu)?;
            // Copy the image config
            let mut config_reader = image.config().open(uri).await?;
            let mut config_file = File::create(
                blob_dir.join(image.config().digest().strip_prefix("sha256:").unwrap()),
            )
            .await
            .context(error::FileSnafu)?;
            Layer::copy(&mut config_reader, &mut config_file, image.config().size()).await?;

            let mut tasks: Vec<JoinHandle<crate::Result<()>>> = Vec::new();
            for layer in image.layers().iter() {
                let layer = layer.clone();
                let uri = uri.clone();
                let blob_dir = blob_dir.clone();
                tasks.push(tokio::spawn(async move {
                    let mut reader = layer.open(&uri).await?;
                    let mut blob_file = File::create(
                        blob_dir.join(layer.digest().strip_prefix("sha256:").unwrap()),
                    )
                    .await
                    .context(error::FileSnafu)?;
                    Layer::copy(&mut reader, &mut blob_file, layer.size()).await?;
                    Ok(())
                }));
            }
            for result in join_all(tasks).await {
                let result = result.context(error::LayerWaitSnafu)?;
                result?;
            }
        }

        let mut archive = ArchiveBuilder::new(output);
        archive
            .append_dir_all(".", tmp_dir.path().to_path_buf())
            .await
            .context(error::ArchiveSnafu)?;
        archive.finish().await.context(error::ArchiveSnafu)?;

        Ok(())
    }

    /// Create an OCI tar archive that contains either all of the index images (if no platform provided)
    /// or only the platforms specified
    #[cfg(feature = "progress")]
    pub async fn to_oci_progress<W>(
        &self,
        uri: &Uri,
        platform: Option<Platform>,
        output: W,
        multi: &mut MultiProgress,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let tmp_dir = tempdir().context(error::TempSnafu)?;
        tokio::fs::write(
            tmp_dir.path().join("oci-layout"),
            r#"{ "imageLayoutVersion": "1.0.0" }"#,
        )
        .await
        .context(error::FileSnafu)?;

        let blob_dir = tmp_dir.path().join("blobs/sha256");
        create_dir_all(&blob_dir)
            .await
            .context(error::DirectorySnafu)?;

        // Start with ourselves for the index
        let mut index = self.clone();
        if let Some(platform) = platform {
            // If we are selecting only a single platform then filter the manifests down
            index.manifests = index
                .manifests
                .iter()
                .filter(|x| x.platform() == Some(platform.clone()))
                .cloned()
                .collect::<Vec<Layer>>();
            if index.manifests.is_empty() {
                return error::IndexNoPlatformSnafu { platform }.fail();
            }
        }
        let index_content = serde_json::to_string(&index).context(error::SerializeSnafu)?;
        tokio::fs::write(tmp_dir.path().join("index.json"), &index_content)
            .await
            .context(error::FileSnafu)?;

        // Now for every manifest we are working with we need to store it out
        for manifest in index.manifests.iter() {
            let image_uri = UriBuilder::default()
                .registry(uri.registry().clone())
                .repository(uri.repository())
                .reference(Reference::from_str(manifest.digest())?)
                .build()
                .context(error::UriSnafu)?;
            let image = Image::fetch(&image_uri, manifest.platform().clone()).await?;
            // Write the image manifest as a blob
            let manifest_bytes = serde_json::to_string(&image).context(error::SerializeSnafu)?;
            tokio::fs::write(
                blob_dir.join(manifest.digest().strip_prefix("sha256:").unwrap()),
                &manifest_bytes,
            )
            .await
            .context(error::FileSnafu)?;
            // Copy the image config
            let mut config_reader = image.config().open_progress(uri, multi).await?;
            let mut config_file = File::create(
                blob_dir.join(image.config().digest().strip_prefix("sha256:").unwrap()),
            )
            .await
            .context(error::FileSnafu)?;
            Layer::copy(&mut config_reader, &mut config_file, image.config().size()).await?;

            let mut tasks: Vec<JoinHandle<crate::Result<()>>> = Vec::new();
            for layer in image.layers().iter() {
                let layer = layer.clone();
                let uri = uri.clone();
                let mut multi = multi.clone();
                let blob_dir = blob_dir.clone();
                tasks.push(tokio::spawn(async move {
                    let mut reader = layer.open_progress(&uri, &mut multi).await?;
                    let mut blob_file = File::create(
                        blob_dir.join(layer.digest().strip_prefix("sha256:").unwrap()),
                    )
                    .await
                    .context(error::FileSnafu)?;
                    Layer::copy(&mut reader, &mut blob_file, layer.size()).await?;
                    Ok(())
                }));
            }
            for result in join_all(tasks).await {
                let result = result.context(error::LayerWaitSnafu)?;
                result?;
            }
        }

        let mut archive = ArchiveBuilder::new(output);
        archive
            .append_dir_all(".", tmp_dir.path().to_path_buf())
            .await
            .context(error::ArchiveSnafu)?;
        archive.finish().await.context(error::ArchiveSnafu)?;

        Ok(())
    }
}

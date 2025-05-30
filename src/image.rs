#[cfg(feature = "compression")]
use crate::compression::Decompress;
use crate::error;
use crate::layer::Layer;
use crate::models::{Config, ImageConfig, MediaType, Platform, TarballManifestBuilder};
use crate::uri::{Reference, Uri};
use derive_builder::Builder;
use futures::future::join_all;
use futures::StreamExt;
#[cfg(feature = "progress")]
use indicatif::MultiProgress;
use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt};
use std::collections::HashSet;
use tempfile::tempdir;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_tar::{Archive, Builder as ArchiveBuilder};

const WHITEOUT: &str = ".wh.";

/// Represents a single Image or Manifest object in an OCI registry + repository
/// all operations working with a single image work with this type.
#[derive(Debug, Serialize, Deserialize, Clone, Builder)]
#[builder(setter(into))]
#[serde(rename_all = "camelCase")]
pub struct Image {
    schema_version: usize,
    media_type: MediaType,
    config: Layer,
    layers: Vec<Layer>,
    #[serde(skip)]
    platform: Option<Platform>,
}

impl Image {
    /// Read an image manifest from the provided reader and save a platform if specified
    pub async fn read<R>(reader: &mut R, platform: Option<Platform>) -> crate::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let mut buffer = Vec::new();
        reader
            .read_to_end(&mut buffer)
            .await
            .context(error::ArchiveSnafu)?;
        let mut me: Self =
            serde_json::from_slice(buffer.as_slice()).context(error::ImageInvalidManifestSnafu)?;
        me.platform = platform;
        Ok(me)
    }

    /// Create a new Image manifest with the provided config layer and layers
    pub async fn create(config: &Layer, layers: &[Layer], platform: Option<Platform>) -> Self {
        Self {
            schema_version: 2,
            media_type: MediaType::Config,
            config: config.clone(),
            layers: layers.to_vec(),
            platform,
        }
    }

    /// Fetch an image manigest from an oci registry
    pub async fn fetch(uri: &Uri, platform: Option<Platform>) -> crate::Result<Self> {
        ensure!(
            matches!(uri.reference(), Reference::Digest { .. }),
            error::DirectLoadImageSnafu { uri: uri.clone() }
        );
        let mut me: Self = uri
            .registry()
            .fetch_manifest(uri.repository(), uri.reference().to_string().as_str())
            .await?;
        me.platform = platform.clone();
        Ok(me)
    }

    /// Schema version
    pub fn schema_version(&self) -> usize {
        self.schema_version
    }

    /// Media type
    pub fn media_type(&self) -> &MediaType {
        &self.media_type
    }

    /// Config layer reference
    pub fn config(&self) -> &Layer {
        &self.config
    }

    /// Content blob layers
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Stored platform hint, primarily used for construction of an index
    pub fn platform(&self) -> Option<Platform> {
        self.platform.clone()
    }

    /// Fetch and deserialize the image configuration from the registry
    pub async fn fetch_config(&self, uri: &Uri) -> crate::Result<ImageConfig> {
        let mut layer = self.config.open(uri).await?;
        let mut config = String::new();
        layer
            .read_to_string(&mut config)
            .await
            .context(error::LayerReadSnafu)?;
        serde_json::from_str(config.as_str()).context(error::ConfigDeserializeSnafu)
    }

    /// Extract the content of this image to filesystem. This method assumes that the layers are a series
    /// of tar archives that can be extracted. It requires the compression feature in order to automatically
    /// decompress the layers
    #[cfg(feature = "compression")]
    pub async fn filesystem<W>(&self, uri: &Uri, output: W) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut archive = ArchiveBuilder::new(output);
        let mut filemap: HashSet<String> = HashSet::new();

        for layer in self.layers.iter().rev() {
            let reader = Decompress::new(layer.media_type(), layer.open(uri).await?);
            let mut layer = Archive::new(reader);
            // Make sure to use the raw entry stream to avoid truncation of long links and long paths
            let mut entries = layer.entries_raw().context(error::LayerArchiveSnafu)?;
            while let Some(entry) = entries.next().await {
                let mut entry = entry.context(error::LayerArchiveSnafu)?;
                let header = entry.header().clone();
                let path = header.path().context(error::LayerArchiveSnafu)?;
                let path = path.to_string_lossy();
                if path.contains(WHITEOUT)
                    || (header.entry_type().is_file() && filemap.contains(path.as_ref()))
                {
                    continue;
                }

                filemap.insert(path.to_string());
                archive
                    .append(&header, &mut entry)
                    .await
                    .context(error::LayerCopySnafu)?;
            }
        }
        archive.finish().await.context(error::ArchiveSnafu)?;

        Ok(())
    }

    /// Extract the content of this image to filesystem. This method assumes that the layers are a series
    /// of tar archives that can be extracted. It requires the compression feature in order to automatically
    /// decompress the layers. It also reports to indicatif progress bars.
    #[cfg(all(feature = "progress", feature = "compression"))]
    pub async fn filesystem_progress<W>(
        &self,
        uri: &Uri,
        output: W,
        multi: &mut MultiProgress,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut archive = ArchiveBuilder::new(output);
        let mut filemap: HashSet<String> = HashSet::new();

        for layer in self.layers.iter().rev() {
            let reader =
                Decompress::new(layer.media_type(), layer.open_progress(uri, multi).await?);
            let mut layer = Archive::new(reader);
            // Make sure to use the raw entry stream to avoid truncation of long links and long paths
            let mut entries = layer.entries_raw().context(error::LayerArchiveSnafu)?;
            while let Some(entry) = entries.next().await {
                let mut entry = entry.context(error::LayerArchiveSnafu)?;
                let header = entry.header().clone();
                let path = header.path().context(error::LayerArchiveSnafu)?;
                let path = path.to_string_lossy();
                if path.contains(WHITEOUT)
                    || (header.entry_type().is_file() && filemap.contains(path.as_ref()))
                {
                    continue;
                }

                filemap.insert(path.to_string());
                archive
                    .append(&header, &mut entry)
                    .await
                    .context(error::LayerCopySnafu)?;
            }
        }
        archive.finish().await.context(error::ArchiveSnafu)?;

        Ok(())
    }

    /// Write this image out as a docker loadable tarball. This is NOT an oci archive and is primarily to be used with
    /// docker/finch/podman/nerdctl load
    #[cfg(feature = "compression")]
    pub async fn to_tarball<W>(&self, uri: &Uri, output: W) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut manifest = TarballManifestBuilder::default()
            .config(self.config.digest())
            .repo_tags(vec![uri.to_string()])
            .layers(vec![])
            .build()
            .context(error::TarballManifestSnafu)?;
        let tmp_dir = tempdir().context(error::TempSnafu)?;
        let mut config_reader = self.config.open(uri).await?;
        let mut config_file = File::create(tmp_dir.path().join(self.config.digest()))
            .await
            .context(error::FileSnafu)?;
        Layer::copy(&mut config_reader, &mut config_file, self.config.size()).await?;

        let mut tasks: Vec<JoinHandle<crate::Result<String>>> = Vec::new();
        let tmp_path = tmp_dir.path().to_path_buf();
        for layer in self.layers.iter() {
            let layer = layer.clone();
            let uri = uri.clone();
            let tmp_path = tmp_path.clone();
            tasks.push(tokio::spawn(async move {
                let mut reader = layer.open(&uri).await?;
                let blob_layer = format!(
                    "{}.tar{}",
                    layer.digest().split_once(":").unwrap().1,
                    layer.media_type().compression().to_ext()
                );
                let mut blob_file = File::create(tmp_path.join(blob_layer.clone()))
                    .await
                    .context(error::FileSnafu)?;
                Layer::copy(&mut reader, &mut blob_file, layer.size()).await?;
                Ok(blob_layer)
            }));
        }
        for result in join_all(tasks).await {
            let result = result.unwrap();
            manifest.layers.push(result?);
        }
        let manifest_bytes =
            serde_json::to_string(&vec![manifest]).context(error::SerializeSnafu)?;
        tokio::fs::write(tmp_dir.path().join("manifest.json"), manifest_bytes)
            .await
            .context(error::FileSnafu)?;
        let mut archive = ArchiveBuilder::new(output);
        archive
            .append_dir_all(".", tmp_dir.path().to_path_buf())
            .await
            .context(error::ArchiveSnafu)?;
        archive.finish().await.context(error::ArchiveSnafu)?;

        Ok(())
    }

    /// Write this image out as a docker loadable tarball. This is NOT an oci archive and is primarily to be used with
    /// docker/finch/podman/nerdctl load. This version will report as it fetches the image to indicatif progress bars.
    #[cfg(all(feature = "compression", feature = "progress"))]
    pub async fn to_tarball_progress<W>(
        &self,
        uri: &Uri,
        output: W,
        progress: &mut MultiProgress,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut manifest = TarballManifestBuilder::default()
            .config(self.config.digest())
            .repo_tags(vec![uri.to_string()])
            .layers(vec![])
            .build()
            .context(error::TarballManifestSnafu)?;
        let tmp_dir = tempdir().context(error::TempSnafu)?;
        let mut config_reader = self.config.open_progress(uri, progress).await?;
        let mut config_file = File::create(tmp_dir.path().join(self.config.digest()))
            .await
            .context(error::FileSnafu)?;
        Layer::copy(&mut config_reader, &mut config_file, self.config.size()).await?;

        let mut tasks: Vec<JoinHandle<crate::Result<String>>> = Vec::new();
        let tmp_path = tmp_dir.path().to_path_buf();
        for layer in self.layers.iter() {
            let layer = layer.clone();
            let uri = uri.clone();
            let tmp_path = tmp_path.clone();
            let mut multi = progress.clone();
            tasks.push(tokio::spawn(async move {
                let mut reader = layer.open_progress(&uri, &mut multi).await?;
                let blob_layer = format!(
                    "{}.tar{}",
                    layer.digest().split_once(":").unwrap().1,
                    layer.media_type().compression().to_ext()
                );
                let mut blob_file = File::create(tmp_path.join(blob_layer.clone()))
                    .await
                    .context(error::FileSnafu)?;
                Layer::copy(&mut reader, &mut blob_file, layer.size()).await?;
                Ok(blob_layer)
            }));
        }
        for result in join_all(tasks).await {
            let result = result.unwrap();
            manifest.layers.push(result?);
        }
        let manifest_bytes =
            serde_json::to_string(&vec![manifest]).context(error::SerializeSnafu)?;
        tokio::fs::write(tmp_dir.path().join("manifest.json"), manifest_bytes)
            .await
            .context(error::FileSnafu)?;
        let mut archive = ArchiveBuilder::new(output);
        archive
            .append_dir_all(".", tmp_dir.path().to_path_buf())
            .await
            .context(error::ArchiveSnafu)?;
        archive.finish().await.context(error::ArchiveSnafu)?;

        Ok(())
    }

    /// Push this image to an oci registry
    pub async fn push(&self, uri: &Uri) -> crate::Result<Layer> {
        uri.registry()
            .push_manifest(
                &self.media_type,
                uri.repository(),
                uri.reference().to_string().as_str(),
                &self,
                self.platform.clone(),
            )
            .await
    }

    /// Create a new config layer blob for an image
    pub async fn create_config(uri: &Uri, config: &Config) -> crate::Result<Layer> {
        let config_bytes = serde_json::to_vec(config).context(error::SerializeSnafu)?;
        let mut writer = Layer::create(uri, &MediaType::Config, config_bytes.len(), None)
            .await?
            .unwrap();
        writer
            .write_all(config_bytes.as_slice())
            .await
            .context(error::LayerWriteSnafu)?;
        writer.flush().await.context(error::LayerWriteSnafu)?;
        writer.layer().await
    }
}

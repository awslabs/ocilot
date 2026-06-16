#[cfg(feature = "compression")]
use crate::compression::Decompress;
use crate::digest::Digest;
use crate::error;
use crate::layer::Layer;
use crate::models::{Config, ImageConfig, MediaType, Platform, TarballManifest};
use crate::progress::{ProgressReporter, SharedProgress};
use crate::uri::{Reference, Uri};
use bon::Builder;
use futures::StreamExt;
use futures::future::try_join_all;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt, ensure};
use std::collections::HashSet;
use tempfile::tempdir;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_tar::{Archive, Builder as ArchiveBuilder};

const WHITEOUT: &str = ".wh.";

/// Represents a single Image or Manifest object in an OCI registry + repository.
///
/// All operations working with a single image work with this type.
#[derive(Debug, Serialize, Deserialize, Clone, Builder)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    #[builder(into)]
    schema_version: usize,
    #[builder(into)]
    media_type: MediaType,
    #[builder(into)]
    config: Layer,
    #[builder(into)]
    layers: Vec<Layer>,
    #[builder(into)]
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
        let mut layer = self.config.open(uri, None).await?;
        let mut config = String::new();
        layer
            .read_to_string(&mut config)
            .await
            .context(error::LayerReadSnafu)?;
        serde_json::from_str(config.as_str()).context(error::ConfigDeserializeSnafu)
    }

    /// Extract the content of this image to filesystem. This method assumes
    /// that the layers are a series of tar archives that can be extracted.
    /// It requires the compression feature in order to automatically
    /// decompress the layers. Pass `Some(reporter)` for progress, `None`
    /// for silent operation.
    #[cfg(feature = "compression")]
    pub async fn filesystem<W>(
        &self,
        uri: &Uri,
        output: W,
        progress: Option<&dyn ProgressReporter>,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut archive = ArchiveBuilder::new(output);
        let mut filemap: HashSet<String> = HashSet::new();

        for layer in self.layers.iter().rev() {
            let reader = Decompress::new(layer.media_type(), layer.open(uri, progress).await?)?;
            let mut layer = Archive::new(reader);
            // Use the raw entry stream to avoid truncation of long links and long paths.
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

    /// Write this image out as a docker loadable tarball. This is NOT an
    /// oci archive and is primarily to be used with
    /// docker/finch/podman/nerdctl load. Pass a populated [`SharedProgress`]
    /// for progress reporting or [`SharedProgress::none`] for silent
    /// operation. `SharedProgress` is `Arc`-backed and `Clone` so it can
    /// be moved into spawned tasks safely without `unsafe` lifetime
    /// extension.
    #[cfg(feature = "compression")]
    pub async fn to_tarball<W>(
        &self,
        uri: &Uri,
        output: W,
        progress: SharedProgress,
    ) -> crate::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut manifest = TarballManifest::builder()
            .config(self.config.digest())
            .repo_tags(vec![uri.to_string()])
            .layers(vec![])
            .build();
        let tmp_dir = tempdir().context(error::TempSnafu)?;
        let mut config_reader = self.config.open(uri, progress.as_ref()).await?;
        let mut config_file = File::create(tmp_dir.path().join(self.config.digest()))
            .await
            .context(error::FileSnafu)?;
        Layer::copy(&mut config_reader, &mut config_file, self.config.size()).await?;

        // Each spawned task gets its own clone of the Arc-backed
        // SharedProgress; no `unsafe` lifetime extension required.
        let mut tasks: Vec<JoinHandle<crate::Result<String>>> = Vec::new();
        let tmp_path = tmp_dir.path().to_path_buf();
        for layer in self.layers.iter() {
            let layer = layer.clone();
            let uri = uri.clone();
            let tmp_path = tmp_path.clone();
            let progress = progress.clone();
            tasks.push(tokio::spawn(async move {
                let parsed = Digest::parse(layer.digest())?;
                let mut reader = layer.open(&uri, progress.as_ref()).await?;
                let blob_layer = format!(
                    "{}.tar{}",
                    parsed.hex(),
                    layer.media_type().compression().to_ext()
                );
                let mut blob_file = File::create(tmp_path.join(blob_layer.clone()))
                    .await
                    .context(error::FileSnafu)?;
                Layer::copy(&mut reader, &mut blob_file, layer.size()).await?;
                Ok(blob_layer)
            }));
        }
        // try_join_all aborts on first error and returns the join error
        // properly typed; #4 + #11.
        let names = try_join_all(tasks).await.context(error::JoinSnafu)?;
        for name in names {
            manifest.layers.push(name?);
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
        let mut writer = Layer::create(uri, &MediaType::Config, config_bytes.len(), None, None)
            .await?
            .context(error::InternalSnafu {
                context: "Layer::create returned None for config blob",
            })?;
        writer
            .write_all(config_bytes.as_slice())
            .await
            .context(error::LayerWriteSnafu)?;
        writer.shutdown().await.context(error::LayerWriteSnafu)?;
        writer.layer().await
    }
}

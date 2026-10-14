use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_recursion::async_recursion;
use clap::Parser;
use futures::StreamExt;
use futures::future::try_join_all;
use ocilot::digest::Digest;
use ocilot::error;
use ocilot::image::Image;
use ocilot::index::Index;
use ocilot::layer::{Layer, digest_bytes};
use ocilot::manifest::Manifest;
use ocilot::progress::SharedProgress;
use ocilot::uri::{Reference, Uri};
use snafu::{OptionExt, ResultExt};
use std::io::SeekFrom;
use tokio::io::AsyncSeekExt;
use tokio::task::JoinHandle;
use tokio::{fs::File, io::AsyncReadExt};
use tokio_tar::{Archive, Entry};

use super::context::Ctx;

/// Push an OCI archive to a registry.
#[derive(Parser, Debug)]
#[command(version, about = "Push an oci archive to repo", long_about = None)]
pub struct Push {
    archive: PathBuf,
    uri: String,
    #[arg(short, long)]
    insecure: bool,
}

impl Push {
    pub async fn run(&self, ctx: &mut Ctx) -> Result<(), error::Error> {
        let mut uri = Uri::new(self.uri.as_str()).await?;
        uri.set_secure(!self.insecure);
        let progress = ctx.progress();
        let mut archive = File::open(&self.archive).await.context(error::FileSnafu)?;
        // We need to find the index first
        let mut index_entry = afind(&mut archive, |x| x.ends_with("index.json"))
            .await?
            .context(error::ImageNotValidSnafu {})?;
        let mut buffer = Vec::new();
        index_entry
            .read_to_end(&mut buffer)
            .await
            .context(error::ArchiveSnafu)?;
        let Manifest::Index(index) = Manifest::from_slice(buffer.as_slice())? else {
            return error::ImageNotValidSnafu {}.fail();
        };
        // A layout whose index.json names a single manifest (as written by
        // `ocilot pull` and crane) is pushing that manifest, unless the
        // destination is index.json's own digest (a flat layout holding a
        // single image). Otherwise index.json is the index being pushed.
        let root = match index.manifests() {
            [single] if !addresses(&uri, &buffer)? => {
                read_manifest(&mut archive, single.digest()).await?
            }
            _ => Manifest::Index(index),
        };
        match root {
            Manifest::Index(index) => self.push_index(&uri, &index, &progress).await,
            Manifest::Image(image) => self.push_image(&uri, &image, &progress).await,
        }
    }

    /// Push every manifest referenced by `index` by digest, then the index
    /// itself to `uri`.
    #[async_recursion]
    async fn push_index(
        &self,
        uri: &Uri,
        index: &Index,
        progress: &SharedProgress,
    ) -> Result<(), error::Error> {
        let mut archive = File::open(&self.archive).await.context(error::FileSnafu)?;
        for child in index.manifests().iter() {
            let child_uri = Uri::builder()
                .registry(uri.registry().clone())
                .repository(uri.repository())
                .reference(Reference::from_str(child.digest())?)
                .build();
            match read_manifest(&mut archive, child.digest()).await? {
                Manifest::Index(nested) => self.push_index(&child_uri, &nested, progress).await?,
                Manifest::Image(image) => self.push_image(&child_uri, &image, progress).await?,
            }
        }
        index.push(uri).await
    }

    /// Upload the config and layers of `image`, then push its manifest to `uri`.
    async fn push_image(
        &self,
        uri: &Uri,
        image: &Image,
        progress: &SharedProgress,
    ) -> Result<(), error::Error> {
        let mut archive = File::open(&self.archive).await.context(error::FileSnafu)?;
        // First lets copy the config blob
        let config_digest = Digest::parse(image.config().digest())?;
        let mut config_entry = afind(&mut archive, |x| x.ends_with(config_digest.hex()))
            .await?
            .context(error::BlobMissingSnafu {
                digest: image.config().digest(),
            })?;
        let config_size = config_entry
            .header()
            .raw_entry_size()
            .context(error::ArchiveSnafu)?;

        let mut writer = Layer::create(
            uri,
            image.config().media_type(),
            config_size as usize,
            Some(image.config().digest().to_string()),
            progress.as_ref(),
        )
        .await?;
        if let Some(writer) = writer.as_mut() {
            Layer::copy(&mut config_entry, writer, config_size as usize).await?;
            writer.layer().await?;
        }
        let mut tasks: Vec<JoinHandle<Result<(), error::Error>>> = Vec::new();
        // Copy all the blobs
        for layer in image.layers().iter() {
            let mut larchive = File::open(&self.archive).await.context(error::FileSnafu)?;
            let layer = layer.clone();
            let uri = uri.clone();
            let progress = progress.clone();
            tasks.push(tokio::spawn(async move {
                let layer_digest = Digest::parse(layer.digest())?;
                let mut layer_entry = afind(&mut larchive, |x| x.ends_with(layer_digest.hex()))
                    .await?
                    .context(error::BlobMissingSnafu {
                        digest: layer.digest(),
                    })?;
                let layer_size = layer_entry
                    .header()
                    .raw_entry_size()
                    .context(error::ArchiveSnafu)?;
                let mut writer = Layer::create(
                    &uri,
                    layer.media_type(),
                    layer_size as usize,
                    Some(layer.digest().to_string()),
                    progress.as_ref(),
                )
                .await?;
                if let Some(writer) = writer.as_mut() {
                    Layer::copy(&mut layer_entry, writer, layer_size as usize).await?;
                    writer.layer().await?;
                }
                Ok(())
            }));
        }
        try_join_all(tasks)
            .await
            .context(error::JoinSnafu)?
            .into_iter()
            .collect::<Result<Vec<_>, error::Error>>()?;
        image.push(uri).await?;
        Ok(())
    }
}

/// Whether `uri` is a digest reference naming exactly `bytes`.
fn addresses(uri: &Uri, bytes: &[u8]) -> Result<bool, error::Error> {
    match uri.reference() {
        Reference::Digest { .. } => {
            let digest = uri.reference().to_string();
            Ok(digest_bytes(Some(&digest), bytes)? == digest)
        }
        Reference::Tag(_) => Ok(false),
    }
}

/// Read the manifest blob for `digest` from the archive.
async fn read_manifest(archive: &mut File, digest: &str) -> Result<Manifest, error::Error> {
    let parsed = Digest::parse(digest)?;
    let mut entry = afind(archive, |x| x.ends_with(parsed.hex()))
        .await?
        .context(error::BlobMissingSnafu { digest })?;
    let mut buffer = Vec::new();
    entry
        .read_to_end(&mut buffer)
        .await
        .context(error::ArchiveSnafu)?;
    Manifest::from_slice(buffer.as_slice())
}

async fn afind<F>(
    archive: &mut File,
    predicate: F,
) -> Result<Option<Entry<Archive<&mut File>>>, error::Error>
where
    F: Fn(&Path) -> bool,
{
    archive
        .seek(SeekFrom::Start(0))
        .await
        .context(error::FileSnafu)?;
    let mut archive = Archive::new(archive);
    let mut entries = archive.entries().context(error::ArchiveSnafu)?;
    while let Some(entry) = entries.next().await {
        let entry = entry.context(error::ArchiveSnafu)?;
        let path = entry.path().context(error::ArchiveSnafu)?;
        if predicate(path.as_ref()) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

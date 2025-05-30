use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_recursion::async_recursion;
use clap::Parser;
use futures::future::join_all;
use futures::StreamExt;
use ocilot::error;
use ocilot::image::Image;
use ocilot::index::Index;
use ocilot::layer::Layer;
use ocilot::models::MediaType;
use ocilot::uri::{Reference, Uri, UriBuilder};
use snafu::{OptionExt, ResultExt};
use std::io::SeekFrom;
use tokio::io::AsyncSeekExt;
use tokio::task::JoinHandle;
use tokio::{fs::File, io::AsyncReadExt};
use tokio_tar::{Archive, Entry};

use super::context::Ctx;

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
        let multi = ctx.get();
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
        let mut index: Index =
            serde_json::from_slice(buffer.as_slice()).context(error::ImageInvalidIndexSnafu)?;
        index = find_index(&mut archive, &index).await?;
        for manifest in index.manifests().iter() {
            let digest = manifest.digest().split_once(':').unwrap().1;
            let mut blob_entry = afind(&mut archive, |x| x.ends_with(digest))
                .await?
                .context(error::BlobMissingSnafu {
                    digest: manifest.digest(),
                })?;
            let mut buffer = Vec::new();
            blob_entry
                .read_to_end(&mut buffer)
                .await
                .context(error::ArchiveSnafu)?;
            let image: Image = serde_json::from_slice(buffer.as_slice())
                .context(error::ImageInvalidManifestSnafu)?;
            // First lets copy the config blob
            let cdigest = image.config().digest().split_once(':').unwrap().1;
            let mut config_entry = afind(&mut archive, |x| x.ends_with(cdigest))
                .await?
                .context(error::BlobMissingSnafu {
                    digest: image.config().digest(),
                })?;
            let config_size = config_entry
                .header()
                .entry_size()
                .context(error::ArchiveSnafu)?;

            let mut writer = Layer::create_progress(
                &uri,
                image.config().media_type(),
                format!("blob {}", &cdigest[0..9]).as_str(),
                config_size,
                multi,
                Some(image.config().digest().to_string()),
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
                let mut multi = multi.clone();
                tasks.push(tokio::spawn(async move {
                    let ldigest = layer.digest().split_once(":").unwrap().1;
                    let mut layer_entry = afind(&mut larchive, |x| x.ends_with(ldigest))
                        .await?
                        .context(error::BlobMissingSnafu {
                            digest: layer.digest(),
                        })?;
                    let layer_size = layer_entry
                        .header()
                        .entry_size()
                        .context(error::ArchiveSnafu)?;
                    let mut writer = Layer::create_progress(
                        &uri,
                        layer.media_type(),
                        format!("blob {}", &ldigest[0..9]).as_str(),
                        layer_size,
                        &mut multi,
                        Some(layer.digest().to_string()),
                    )
                    .await?;
                    if let Some(writer) = writer.as_mut() {
                        Layer::copy(&mut layer_entry, writer, layer_size as usize).await?;
                        writer.layer().await?;
                    }
                    Ok(())
                }));
            }
            for result in join_all(tasks).await {
                let result = result.expect("failed to join");
                result?;
            }
            let manifest_uri = UriBuilder::default()
                .registry(uri.registry().clone())
                .repository(uri.repository())
                .reference(Reference::from_str(manifest.digest())?)
                .build()
                .context(error::UriSnafu)?;
            image.push(&manifest_uri).await?;
        }
        // Now that all the layers are uploaded we can push the image
        index.push(&uri).await?;

        Ok(())
    }
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

#[async_recursion]
async fn find_index<'a>(archive: &'a mut File, index: &Index) -> Result<Index, error::Error> {
    for manifest in index.manifests().iter() {
        let digest = manifest.digest().split_once(':').unwrap().1;
        let mut blob_entry =
            afind(archive, |x| x.ends_with(digest))
                .await?
                .context(error::BlobMissingSnafu {
                    digest: manifest.digest(),
                })?;
        let mut buffer = Vec::new();
        blob_entry
            .read_to_end(&mut buffer)
            .await
            .context(error::ArchiveSnafu)?;
        let value: serde_json::Value =
            serde_json::from_slice(buffer.as_slice()).context(error::ImageInvalidIndexSnafu)?;
        if let Some(mvalue) = value.get("mediaType") {
            let mtype: MediaType =
                serde_json::from_value(mvalue.clone()).context(error::ImageInvalidIndexSnafu)?;
            if mtype == MediaType::ImageIndex || mtype == MediaType::DockerManifestList {
                // Nested image index so recurse
                let next: Index =
                    serde_json::from_value(value.clone()).context(error::ImageInvalidIndexSnafu)?;
                return find_index(archive, &next).await;
            } else {
                // Non-index this is our root
                return Ok(index.clone());
            }
        }
    }
    error::ImageNotValidSnafu {}.fail()
}

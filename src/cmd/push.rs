use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_recursion::async_recursion;
use clap::Parser;
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
use std::collections::HashMap;
use std::io::SeekFrom;
use tar::Archive;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::task::JoinHandle;

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
        // One header-only pass over the tar; every later read seeks directly
        // to the blob, so nothing holds a sync tar `Entry` across an await.
        let blobs = scan_archive(&self.archive)?;
        let loc = locate(&blobs, |x| x.ends_with("index.json"))
            .context(error::ImageNotValidSnafu {})?;
        let buffer = read_blob(&self.archive, loc).await?;
        let Manifest::Index(index) = Manifest::from_slice(buffer.as_slice())? else {
            return error::ImageNotValidSnafu {}.fail();
        };
        // A layout whose index.json names a single manifest (as written by
        // `ocilot pull` and crane) is pushing that manifest, unless the
        // destination is index.json's own digest (a flat layout holding a
        // single image). Otherwise index.json is the index being pushed.
        let root = match index.manifests() {
            [single] if !addresses(&uri, &buffer)? => {
                self.read_manifest(&blobs, single.digest()).await?
            }
            _ => Manifest::Index(index),
        };
        match root {
            Manifest::Index(index) => self.push_index(&uri, &index, &blobs, &progress).await,
            Manifest::Image(image) => self.push_image(&uri, &image, &blobs, &progress).await,
        }
    }

    /// Push every manifest referenced by `index` by digest, then the index
    /// itself to `uri`.
    #[async_recursion]
    async fn push_index(
        &self,
        uri: &Uri,
        index: &Index,
        blobs: &HashMap<PathBuf, BlobLocation>,
        progress: &SharedProgress,
    ) -> Result<(), error::Error> {
        for child in index.manifests().iter() {
            let child_uri = Uri::builder()
                .registry(uri.registry().clone())
                .repository(uri.repository())
                .reference(Reference::from_str(child.digest())?)
                .build();
            match self.read_manifest(blobs, child.digest()).await? {
                Manifest::Index(nested) => {
                    self.push_index(&child_uri, &nested, blobs, progress).await?
                }
                Manifest::Image(image) => {
                    self.push_image(&child_uri, &image, blobs, progress).await?
                }
            }
        }
        index.push(uri).await
    }

    /// Upload the config and layers of `image`, then push its manifest to `uri`.
    async fn push_image(
        &self,
        uri: &Uri,
        image: &Image,
        blobs: &HashMap<PathBuf, BlobLocation>,
        progress: &SharedProgress,
    ) -> Result<(), error::Error> {
        let find = |layer: &Layer| -> Result<BlobLocation, error::Error> {
            let digest = Digest::parse(layer.digest())?;
            locate(blobs, |x| x.ends_with(digest.hex())).context(error::BlobMissingSnafu {
                digest: layer.digest(),
            })
        };
        // Config first, then all layers concurrently.
        copy_layer(
            uri.clone(),
            image.config().clone(),
            self.archive.clone(),
            find(image.config())?,
            progress.clone(),
        )
        .await?;

        let mut tasks: Vec<JoinHandle<Result<(), error::Error>>> = Vec::new();
        for layer in image.layers().iter() {
            let loc = find(layer)?;
            tasks.push(tokio::spawn(copy_layer(
                uri.clone(),
                layer.clone(),
                self.archive.clone(),
                loc,
                progress.clone(),
            )));
        }
        try_join_all(tasks)
            .await
            .context(error::JoinSnafu)?
            .into_iter()
            .collect::<Result<Vec<_>, error::Error>>()?;
        image.push(uri).await?;
        Ok(())
    }

    /// Read the manifest blob for `digest` from the archive.
    async fn read_manifest(
        &self,
        blobs: &HashMap<PathBuf, BlobLocation>,
        digest: &str,
    ) -> Result<Manifest, error::Error> {
        let parsed = Digest::parse(digest)?;
        let loc = locate(blobs, |x| x.ends_with(parsed.hex()))
            .context(error::BlobMissingSnafu { digest })?;
        Manifest::from_slice(read_blob(&self.archive, loc).await?.as_slice())
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

/// Location of a regular file's contents inside an uncompressed tar.
#[derive(Clone, Copy, Debug)]
struct BlobLocation {
    offset: u64,
    size: u64,
}

/// Scan the archive headers once and record where each regular file's data
/// lives. Uses `entries_with_seek` so blob data is skipped, not read.
fn scan_archive(path: &Path) -> Result<HashMap<PathBuf, BlobLocation>, error::Error> {
    let file = std::fs::File::open(path).context(error::ArchiveSnafu)?;
    let mut archive = Archive::new(file);
    let mut locations = HashMap::new();
    for entry in archive.entries_with_seek().context(error::ArchiveSnafu)? {
        let entry = entry.context(error::ArchiveSnafu)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context(error::ArchiveSnafu)?.into_owned();
        locations.insert(
            path,
            BlobLocation {
                offset: entry.raw_file_position(),
                size: entry.size(),
            },
        );
    }
    Ok(locations)
}

fn locate<F>(
    locations: &HashMap<PathBuf, BlobLocation>,
    predicate: F,
) -> Option<BlobLocation>
where
    F: Fn(&Path) -> bool,
{
    locations
        .iter()
        .find(|(path, _)| predicate(path))
        .map(|(_, loc)| *loc)
}

/// Open an async reader over exactly the bytes of `loc`.
async fn open_blob(
    archive: &Path,
    loc: BlobLocation,
) -> Result<tokio::io::Take<File>, error::Error> {
    let mut file = File::open(archive).await.context(error::FileSnafu)?;
    file.seek(SeekFrom::Start(loc.offset))
        .await
        .context(error::FileSnafu)?;
    Ok(file.take(loc.size))
}

async fn read_blob(archive: &Path, loc: BlobLocation) -> Result<Vec<u8>, error::Error> {
    let mut buffer = Vec::with_capacity(loc.size as usize);
    open_blob(archive, loc)
        .await?
        .read_to_end(&mut buffer)
        .await
        .context(error::ArchiveSnafu)?;
    Ok(buffer)
}

/// Upload the blob described by `layer` from the archive to `uri`.
async fn copy_layer(
    uri: Uri,
    layer: Layer,
    archive: PathBuf,
    loc: BlobLocation,
    progress: SharedProgress,
) -> Result<(), error::Error> {
    let size = loc.size as usize;
    let mut writer = Layer::create(
        &uri,
        layer.media_type(),
        size,
        Some(layer.digest().to_string()),
        progress.as_ref(),
    )
    .await?;
    if let Some(writer) = writer.as_mut() {
        let mut reader = open_blob(&archive, loc).await?;
        Layer::copy(&mut reader, writer, size).await?;
        writer.layer().await?;
    }
    Ok(())
}

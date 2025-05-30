use clap::Parser;
use ocilot::error;
use ocilot::index::IndexBuilder;
use ocilot::layer::LayerBuilder;
use ocilot::models::Platform;
use ocilot::uri::Reference;
use ocilot::uri::Uri;
use ocilot::{image::Image, index::Index};
use sha2::{Digest, Sha256};
use snafu::OptionExt;
use snafu::ResultExt;

use super::context::Ctx;

#[derive(Parser, Debug)]
#[command(version, about = "Commands to interact with an image index", long_about = None)]
pub struct IndexCmd {
    #[clap(subcommand)]
    command: IndexCommands,
}

#[derive(Parser, Debug)]
pub enum IndexCommands {
    Get(GetIndex),
    Add(AddIndex),
}

impl IndexCmd {
    pub async fn run(&self, ctx: &mut Ctx) -> Result<(), ocilot::error::Error> {
        match &self.command {
            IndexCommands::Get(cmd) => cmd.run(ctx).await,
            IndexCommands::Add(cmd) => cmd.run(ctx).await,
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "Get the index of an image", long_about = None)]
pub struct GetIndex {
    url: String,
    #[arg(short, long)]
    insecure: bool,
}

impl GetIndex {
    pub async fn run(&self, _ctx: &Ctx) -> Result<(), ocilot::error::Error> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        let index = Index::fetch(&uri).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&index).context(ocilot::error::SerializeSnafu)?
        );
        Ok(())
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "Create or add to image index in oci registry", long_about = None)]
pub struct AddIndex {
    target: String,
    source: String,
    #[arg(short, long)]
    platform: Option<String>,
    #[arg(short, long)]
    insecure: bool,
}

impl AddIndex {
    pub async fn run(&self, _ctx: &mut Ctx) -> Result<(), ocilot::error::Error> {
        let mut target = Uri::new(self.target.as_str()).await?;
        target.set_secure(!self.insecure);
        let mut source = Uri::new(self.source.as_str()).await?;
        source.set_secure(!self.insecure);
        let index = if Index::check(&target).await? {
            Index::fetch(&target).await?
        } else {
            Index::new(&[]).await
        };

        // Now load the manifest we want to add
        let platform: Option<Platform> = self.platform.clone().map(|x| x.into());
        // If a platform is set and reference is a tag we can use an index to find the right
        // image
        let image = if let Some(platform) = platform.as_ref() {
            if matches!(source.reference(), Reference::Tag(..)) {
                let source_index = Index::fetch(&source).await?;
                source_index
                    .fetch_image(&source, Some(platform.clone()))
                    .await?
                    .context(error::IndexNoPlatformSnafu {
                        platform: platform.clone(),
                    })?
            } else {
                Image::fetch(&source, Some(platform.clone())).await?
            }
        } else {
            Image::fetch(&source, None).await?
        };
        let image_bytes = serde_json::to_vec(&image).context(error::SerializeSnafu)?;
        let hash = Sha256::digest(image_bytes.as_slice());
        let digest = format!("sha256:{}", base16::encode_lower(hash.as_slice()));
        let layer = LayerBuilder::default()
            .media_type(image.media_type().clone())
            .digest(digest.clone())
            .build()
            .context(error::LayerSnafu)?;
        let mut manifests = index.manifests().to_vec();
        manifests.push(layer);
        let index = IndexBuilder::default()
            .schema_version(2_usize)
            .media_type(index.media_type().clone())
            .manifests(manifests)
            .build()
            .context(error::IndexSnafu)?;
        index.push(&target).await?;

        Ok(())
    }
}

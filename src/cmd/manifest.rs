use clap::Parser;
use ocilot::error;
use ocilot::manifest::Manifest as OciManifest;
use ocilot::models::Platform;
use ocilot::uri::Uri;
use snafu::{OptionExt, ResultExt};

use super::context::Ctx;

/// Inspect a manifest from a registry.
#[derive(Parser, Debug)]
#[command(version, about = "Get the manifest of an image", long_about = None)]
pub struct Manifest {
    url: String,
    #[arg(short, long)]
    platform: Option<String>,
    #[arg(short, long)]
    insecure: bool,
}

impl Manifest {
    pub async fn run(&self, _ctx: &Ctx) -> Result<(), error::Error> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        let platform: Option<Platform> = self.platform.clone().map(|x| x.parse()).transpose()?;

        // The reference may resolve to either an image index (manifest list)
        // or an image manifest. Dispatch on `mediaType` so we deserialize
        // into the right shape rather than blindly assuming an index.
        let image = match OciManifest::fetch(&uri).await? {
            OciManifest::Index(index) => index
                .fetch_image(&uri, platform)
                .await?
                .context(error::ImageNotFoundSnafu { uri: uri.clone() })?,
            OciManifest::Image(image) => image,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&image).context(error::SerializeSnafu)?
        );
        Ok(())
    }
}

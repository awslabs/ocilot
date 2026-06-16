use clap::Parser;
use ocilot::error;
use ocilot::manifest::Manifest;
use ocilot::uri::Uri;
use snafu::{OptionExt, ResultExt};
use std::path::PathBuf;

use super::context::Ctx;

/// Export filesystem of a container image as a tarball.
#[derive(Parser, Debug)]
#[command(version, about = "Export filesystem of a container image as a tarball", long_about = None)]
pub struct Export {
    url: String,
    output: PathBuf,
    #[arg(short, long)]
    insecure: bool,
    #[arg(short, long)]
    platform: Option<String>,
}

impl Export {
    pub async fn run(&self, ctx: &mut Ctx) -> Result<(), error::Error> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        let platform = self.platform.clone().map(|x| x.parse()).transpose()?;
        // The reference may resolve to either an image index or a single
        // image manifest; dispatch via `Manifest::fetch` to handle both.
        let image = match Manifest::fetch(&uri).await? {
            Manifest::Index(index) => index
                .fetch_image(&uri, platform)
                .await?
                .context(error::ImageNotFoundSnafu { uri: uri.clone() })?,
            Manifest::Image(image) => image,
        };

        let file = tokio::fs::File::create(&self.output)
            .await
            .context(error::FileSnafu)?;
        let progress = ctx.progress();
        image.filesystem(&uri, file, progress.as_ref()).await?;
        Ok(())
    }
}

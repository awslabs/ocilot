use clap::{Parser, ValueEnum};
use ocilot::index::Index;
use ocilot::uri::Uri;
use ocilot::{error, Result};
use snafu::{OptionExt, ResultExt};
use std::path::PathBuf;

use super::context::Ctx;

#[derive(Parser, Debug)]
#[command(version, about = "Pull remote images by reference and store their contents locally as an archive", long_about = None)]
pub struct Pull {
    url: String,
    output: PathBuf,
    #[arg(short, long)]
    insecure: bool,
    #[arg(short, long)]
    platform: Option<String>,
    #[arg(short, long)]
    format: Format,
}

#[derive(Default, PartialEq, Eq, Debug, Clone, ValueEnum)]
enum Format {
    #[default]
    Tarball,
    Oci,
}

impl Pull {
    pub async fn run(&self, ctx: &mut Ctx) -> Result<()> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        let index = Index::fetch(&uri).await?;
        let platform = self.platform.clone().map(|x| x.into());

        let output = tokio::fs::File::create(&self.output)
            .await
            .context(error::FileSnafu)?;
        let multi = ctx.get();
        match self.format {
            Format::Tarball => {
                let image = index
                    .fetch_image(&uri, platform.clone())
                    .await?
                    .context(error::ImageNotFoundSnafu { uri: uri.clone() })?;
                image.to_tarball_progress(&uri, output, multi).await?
            }
            Format::Oci => index.to_oci_progress(&uri, platform, output, multi).await?,
        }

        Ok(())
    }
}

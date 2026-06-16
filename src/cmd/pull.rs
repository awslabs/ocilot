use clap::{Parser, ValueEnum};
use ocilot::manifest::Manifest;
use ocilot::uri::Uri;
use ocilot::{Result, error};
use snafu::{OptionExt, ResultExt};
use std::path::PathBuf;

use super::context::Ctx;

/// Pull remote images and store locally as an archive.
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

/// Output archive format.
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
        let platform = self.platform.clone().map(|x| x.parse()).transpose()?;

        let output = tokio::fs::File::create(&self.output)
            .await
            .context(error::FileSnafu)?;
        let progress = ctx.progress();
        // The reference may resolve to either an image index or a single
        // image manifest. Dispatch via `Manifest::fetch` so a digest pointing
        // directly at an image manifest is handled correctly.
        match Manifest::fetch(&uri).await? {
            Manifest::Index(index) => match self.format {
                Format::Tarball => {
                    let image = index
                        .fetch_image(&uri, platform.clone())
                        .await?
                        .context(error::ImageNotFoundSnafu { uri: uri.clone() })?;
                    image.to_tarball(&uri, output, progress).await?
                }
                Format::Oci => index.to_oci(&uri, platform, output, progress).await?,
            },
            Manifest::Image(image) => match self.format {
                Format::Tarball => image.to_tarball(&uri, output, progress).await?,
                Format::Oci => {
                    return error::UnsupportedSnafu {
                        reason: format!(
                            "OCI archive format requires an image index but '{uri}' resolves to a single image manifest"
                        ),
                    }
                    .fail();
                }
            },
        }

        Ok(())
    }
}

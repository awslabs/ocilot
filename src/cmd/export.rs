use clap::Parser;
use ocilot::error;
use ocilot::index::Index;
use ocilot::uri::Uri;
use snafu::{OptionExt, ResultExt};
use std::path::PathBuf;

use super::context::Ctx;

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
        let index = Index::fetch(&uri).await?;
        let image = index
            .fetch_image(&uri, self.platform.clone().map(|x| x.into()))
            .await?
            .context(error::ImageNotFoundSnafu { uri: uri.clone() })?;

        let file = tokio::fs::File::create(&self.output)
            .await
            .context(error::FileSnafu)?;
        let multi = ctx.get();
        image.filesystem_progress(&uri, file, multi).await?;
        Ok(())
    }
}

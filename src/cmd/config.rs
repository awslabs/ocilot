use super::context::Ctx;
use clap::Parser;
use ocilot::error;
use ocilot::index::Index;
use ocilot::uri::Uri;
use snafu::{OptionExt, ResultExt};

#[derive(Parser, Debug)]
#[command(version, about = "Get the config of an image", long_about = None)]
pub struct Config {
    url: String,
    #[arg(short, long)]
    platform: Option<String>,
    #[arg(short, long)]
    insecure: bool,
}

impl Config {
    pub async fn run(&self, _ctx: &Ctx) -> Result<(), error::Error> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        let index = Index::fetch(&uri).await?;
        let platform = self.platform.clone().map(|x| x.parse()).transpose()?;
        let image = index
            .fetch_image(&uri, platform)
            .await?
            .context(error::ImageNotFoundSnafu { uri: uri.clone() })?;
        let config = image.fetch_config(&uri).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&config).context(error::SerializeSnafu)?
        );
        Ok(())
    }
}

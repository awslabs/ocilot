use clap::Parser;
use ocilot::error;
use ocilot::index::Index;
use ocilot::models::Platform;
use ocilot::uri::Uri;
use snafu::ResultExt;

use super::context::Ctx;

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
        let platform: Option<Platform> = self.platform.clone().map(|x| x.into());
        let index = Index::fetch(&uri).await?;
        let image = index.fetch_image(&uri, platform).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&image).context(error::SerializeSnafu)?
        );
        Ok(())
    }
}

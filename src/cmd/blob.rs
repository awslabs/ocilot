use std::path::PathBuf;

use clap::Parser;
use snafu::ResultExt;
use tokio::fs::File;

use ocilot::error;
use ocilot::layer::Layer;
use ocilot::uri::Uri;

use super::context::Ctx;

#[derive(Parser, Debug)]
#[command(version, about = "Read a blob from the registry", long_about = None)]
pub struct Blob {
    url: String,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(short, long)]
    insecure: bool,
}

impl Blob {
    pub async fn run(&self, _ctx: &Ctx) -> Result<(), error::Error> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);

        let mut reader = Layer::open_uri(&uri).await?;
        if let Some(output) = self.output.as_ref() {
            let mut file = File::create(output).await.context(error::FileSnafu)?;
            tokio::io::copy(&mut reader, &mut file)
                .await
                .context(error::LayerCopySnafu)?;
        } else {
            tokio::io::copy(&mut reader, &mut tokio::io::stdout())
                .await
                .context(error::LayerCopySnafu)?;
        };

        Ok(())
    }
}

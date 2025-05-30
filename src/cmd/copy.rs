use std::str::FromStr;

use super::context::Ctx;
use clap::Parser;
use futures::future::join_all;
use ocilot::error;
use ocilot::uri::UriBuilder;
use ocilot::{
    image::Image,
    index::Index,
    layer::Layer,
    uri::{Reference, Uri},
    Result,
};
use snafu::ResultExt;
use tokio::task::JoinHandle;

#[derive(Parser, Debug)]
#[command(version, about = "Efficiently copy a remote image from src to dst while retaining the digest value", long_about = None)]
pub struct Copy {
    source: String,
    target: String,
    #[arg(short, long)]
    source_insecure: bool,
    #[arg(short, long)]
    target_insecure: bool,
}

impl Copy {
    pub async fn run(&self, ctx: &mut Ctx) -> Result<()> {
        let mut source = Uri::new(self.source.as_str()).await?;
        source.set_secure(!self.source_insecure);
        let mut target = Uri::new(self.target.as_str()).await?;
        target.set_secure(!self.target_insecure);
        let index = Index::fetch(&source).await?;
        let multi = ctx.get();
        for manifest in index.manifests().iter() {
            let manifest_uri = UriBuilder::default()
                .registry(source.registry().clone())
                .repository(source.repository())
                .reference(Reference::from_str(manifest.digest())?)
                .build()
                .context(error::UriSnafu)?;
            let image = Image::fetch(&manifest_uri, manifest.platform().clone()).await?;
            // Copy the config over, note we do not use progress bars for the read
            let config_uri = UriBuilder::default()
                .registry(target.registry().clone())
                .repository(target.repository())
                .reference(Reference::from_str(image.config().digest())?)
                .build()
                .context(error::UriSnafu)?;
            let digest = &image.config().digest().strip_prefix("sha256:").unwrap()[0..9];
            let mut writer = Layer::create_progress(
                &config_uri,
                image.config().media_type(),
                format!("blob {digest}").as_str(),
                image.config().size() as u64,
                multi,
                Some(image.config().digest().to_string()),
            )
            .await?;
            if let Some(writer) = writer.as_mut() {
                let mut reader = image.config().open(&source).await?;
                Layer::copy(&mut reader, writer, image.config().size()).await?;
                writer.layer().await?;
            }
            // Now we are ready to copy the layers for this image
            let mut tasks: Vec<JoinHandle<Result<()>>> = Vec::new();
            for layer in image.layers().iter() {
                let source_uri = source.clone();
                let target_uri = target.clone();
                let layer = layer.clone();
                let mut multi = multi.clone();
                tasks.push(tokio::spawn(async move {
                    let digest = &layer.digest().strip_prefix("sha256:").unwrap()[0..9];
                    let mut writer = Layer::create_progress(
                        &target_uri,
                        layer.media_type(),
                        format!("blob {digest}").as_str(),
                        layer.size() as u64,
                        &mut multi,
                        Some(layer.digest().to_string()),
                    )
                    .await?;
                    if let Some(writer) = writer.as_mut() {
                        let mut reader = layer.open(&source_uri).await?;
                        Layer::copy(&mut reader, writer, layer.size()).await?;
                        writer.layer().await?;
                    }
                    Ok(())
                }));
            }
            join_all(tasks).await;
            let target_manifest_uri = UriBuilder::default()
                .registry(target.registry().clone())
                .repository(target.repository())
                .reference(Reference::from_str(manifest.digest())?)
                .build()
                .context(error::UriSnafu)?;
            image.push(&target_manifest_uri).await?;
        }
        // Now all images in index are copied push the index
        index.push(&target).await?;

        Ok(())
    }
}

use std::str::FromStr;

use super::context::Ctx;
use clap::Parser;
use futures::future::try_join_all;
use ocilot::{
    Result, error,
    image::Image,
    index::Index,
    layer::Layer,
    uri::{Reference, Uri},
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
        let progress = ctx.progress();
        for manifest in index.manifests().iter() {
            let manifest_uri = Uri::builder()
                .registry(source.registry().clone())
                .repository(source.repository())
                .reference(Reference::from_str(manifest.digest())?)
                .build();
            let image = Image::fetch(&manifest_uri, manifest.platform().clone()).await?;
            // Copy the config over
            let config_uri = Uri::builder()
                .registry(target.registry().clone())
                .repository(target.repository())
                .reference(Reference::from_str(image.config().digest())?)
                .build();
            let mut writer = Layer::create(
                &config_uri,
                image.config().media_type(),
                image.config().size(),
                Some(image.config().digest().to_string()),
                progress.as_ref(),
            )
            .await?;
            if let Some(writer) = writer.as_mut() {
                let mut reader = image.config().open(&source, progress.as_ref()).await?;
                Layer::copy(&mut reader, writer, image.config().size()).await?;
                writer.layer().await?;
            }
            // Now we are ready to copy the layers for this image
            let mut tasks: Vec<JoinHandle<Result<()>>> = Vec::new();
            for layer in image.layers().iter() {
                let source_uri = source.clone();
                let target_uri = target.clone();
                let layer = layer.clone();
                let progress = progress.clone();
                tasks.push(tokio::spawn(async move {
                    let mut writer = Layer::create(
                        &target_uri,
                        layer.media_type(),
                        layer.size(),
                        Some(layer.digest().to_string()),
                        progress.as_ref(),
                    )
                    .await?;
                    if let Some(writer) = writer.as_mut() {
                        let mut reader = layer.open(&source_uri, progress.as_ref()).await?;
                        Layer::copy(&mut reader, writer, layer.size()).await?;
                        writer.layer().await?;
                    }
                    Ok(())
                }));
            }
            // try_join_all aborts on first error and reports JoinErrors
            // through the typed Error::Join variant (#4 + #11).
            try_join_all(tasks)
                .await
                .context(error::JoinSnafu)?
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            let target_manifest_uri = Uri::builder()
                .registry(target.registry().clone())
                .repository(target.repository())
                .reference(Reference::from_str(manifest.digest())?)
                .build();
            image.push(&target_manifest_uri).await?;
        }
        // Now all images in index are copied push the index
        index.push(&target).await?;

        Ok(())
    }
}

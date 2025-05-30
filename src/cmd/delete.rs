use clap::Parser;
use ocilot::{
    error,
    layer::LayerBuilder,
    models::MediaType,
    repository::Repository,
    uri::{Reference, Uri},
};
use snafu::{ensure, ResultExt};

use super::context::Ctx;

#[derive(Parser, Debug)]
#[command(version, about = "Commands to delete objects in a registry", long_about = None)]
pub struct Delete {
    #[clap(subcommand)]
    command: DeleteCommands,
}

#[derive(Parser, Debug)]
pub enum DeleteCommands {
    Blob(DeleteBlob),
    Tag(DeleteTag),
}

impl Delete {
    pub async fn run(&self, _ctx: &Ctx) -> Result<(), error::Error> {
        match &self.command {
            DeleteCommands::Blob(cmd) => cmd.run().await,
            DeleteCommands::Tag(cmd) => cmd.run().await,
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "Delete tag in a repo", long_about = None)]
pub struct DeleteTag {
    url: String,
    #[arg(short, long)]
    insecure: bool,
}

impl DeleteTag {
    pub async fn run(&self) -> Result<(), error::Error> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        let repository = Repository::new(uri.registry(), uri.repository());
        match uri.reference() {
            Reference::Digest { .. } => error::DeleteTagDigestSnafu {}.fail(),
            Reference::Tag(tag) => repository.delete_tag(tag.as_str()).await,
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "Delete a blob in a repo", long_about = None)]
pub struct DeleteBlob {
    url: String,
    #[arg(short, long)]
    insecure: bool,
}

impl DeleteBlob {
    pub async fn run(&self) -> Result<(), error::Error> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        ensure!(
            matches!(uri.reference(), Reference::Digest { .. }),
            error::DeleteBlobNoDigestSnafu {}
        );
        let digest = uri.reference().to_string();
        let layer = LayerBuilder::default()
            .media_type(MediaType::Manifest)
            .digest(digest)
            .size(0_usize)
            .platform(None)
            .build()
            .context(error::LayerSnafu)?;
        layer.delete(&uri).await
    }
}

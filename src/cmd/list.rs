use std::str::FromStr;

use clap::Parser;

use ocilot::error;
use ocilot::registry::Registry;
use ocilot::repository::Repository;
use ocilot::uri::RegistryUri;

use super::context::Ctx;

#[derive(Parser, Debug)]
#[clap(version, about = "List the tags in a repo", long_about = None)]
pub struct List {
    url: String,
    #[arg(short, long)]
    insecure: bool,
}

impl List {
    pub async fn run(&self, _ctx: &Ctx) -> Result<(), error::Error> {
        let mut segments: Vec<_> = self.url.split("/").collect();
        let object = segments.pop().unwrap();
        let registry = segments.join("/");
        let mut registry_uri = RegistryUri::from_str(registry.as_str())?;
        if self.insecure {
            registry_uri.set_secure(false);
        }
        let registry = Registry::new(&registry_uri).await?;
        let repository = Repository::new(&registry, object);
        let tags = repository.tags().await?;
        println!("{}", tags.join("\n"));
        Ok(())
    }
}

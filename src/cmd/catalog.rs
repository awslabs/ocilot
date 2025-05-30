use std::str::FromStr;

use clap::Parser;

use ocilot::error;
use ocilot::registry::Registry;
use ocilot::uri::RegistryUri;

use super::context::Ctx;

#[derive(Parser, Debug)]
#[clap(version, about = "List the repos in a registry", long_about = None)]
pub struct Catalog {
    url: String,
    #[arg(short, long)]
    insecure: bool,
}

impl Catalog {
    pub async fn run(&self, _ctx: &Ctx) -> Result<(), error::Error> {
        let mut registry_uri = RegistryUri::from_str(self.url.as_str())?;
        if self.insecure {
            registry_uri.set_secure(false);
        }
        let registry = Registry::new(&registry_uri).await?;
        let repos = registry.catalog().await?;
        println!("{}", repos.join("\n"));
        Ok(())
    }
}

#[macro_use]
extern crate log;

use crate::cmd::export::Export;
use crate::cmd::pull::Pull;
use clap::Parser;
use cmd::{
    blob::Blob, catalog::Catalog, config::Config, context::Ctx, copy::Copy, delete::Delete,
    index::IndexCmd, list::List, manifest::Manifest, push::Push,
};

mod cmd;

#[derive(Parser, Debug)]
struct Args {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    Index(IndexCmd),
    Manifest(Manifest),
    Config(Config),
    Blob(Blob),
    List(List),
    Catalog(Catalog),
    Export(Export),
    Pull(Pull),
    Push(Push),
    Delete(Delete),
    Copy(Copy),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut ctx = Ctx::init()?;
    let args = Args::parse();

    match args.command {
        Commands::Index(cmd) => cmd.run(&mut ctx).await?,
        Commands::Manifest(cmd) => cmd.run(&ctx).await?,
        Commands::Config(cmd) => cmd.run(&ctx).await?,
        Commands::Blob(cmd) => cmd.run(&ctx).await?,
        Commands::List(cmd) => cmd.run(&ctx).await?,
        Commands::Catalog(cmd) => cmd.run(&ctx).await?,
        Commands::Export(cmd) => cmd.run(&mut ctx).await?,
        Commands::Pull(cmd) => cmd.run(&mut ctx).await?,
        Commands::Delete(cmd) => cmd.run(&ctx).await?,
        Commands::Push(cmd) => cmd.run(&mut ctx).await?,
        Commands::Copy(cmd) => cmd.run(&mut ctx).await?,
    }
    Ok(())
}

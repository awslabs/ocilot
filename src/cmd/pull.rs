use clap::{Parser, ValueEnum};
use ocilot::manifest::Manifest;
use ocilot::oci::OciOptions;
use ocilot::uri::Uri;
use ocilot::{Result, error};
use snafu::{OptionExt, ResultExt};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use super::context::Ctx;

/// Pull remote images and store locally as an archive.
#[derive(Parser, Debug)]
#[command(version, about = "Pull remote images by reference and store their contents locally as an archive", long_about = None)]
pub struct Pull {
    url: String,
    output: PathBuf,
    #[arg(short, long)]
    insecure: bool,
    #[arg(short, long)]
    platform: Option<String>,
    #[arg(short, long)]
    format: Format,
    /// Preserve the image reference used to pull as an annotation when used with --format=oci
    #[arg(long)]
    annotate_ref: bool,
}

/// Output archive format.
#[derive(Default, PartialEq, Eq, Debug, Clone, ValueEnum)]
enum Format {
    #[default]
    Tarball,
    Oci,
}

impl Pull {
    pub async fn run(&self, ctx: &mut Ctx) -> Result<()> {
        let mut uri = Uri::new(self.url.as_str()).await?;
        uri.set_secure(!self.insecure);
        let platform = self.platform.clone().map(|x| x.parse()).transpose()?;

        if self.annotate_ref && self.format != Format::Oci {
            return error::UnsupportedSnafu {
                reason: "--annotate-ref can only be used with --format=oci",
            }
            .fail();
        }

        let progress = ctx.progress();
        // The reference may resolve to either an image index or a single
        // image manifest. Dispatch via `Manifest::fetch` so a digest pointing
        // directly at an image manifest is handled correctly.
        let manifest = Manifest::fetch(&uri).await?;
        // Write to a temporary file beside the output and move it into place
        // on success, so a failed pull neither leaves a partial archive
        // behind nor clobbers an existing one.
        let staged = self.stage_output()?;
        let output =
            tokio::fs::File::from_std(staged.as_file().try_clone().context(error::FileSnafu)?);
        match self.format {
            Format::Tarball => {
                let image = match manifest {
                    Manifest::Index(index) => index
                        .fetch_image(&uri, platform)
                        .await?
                        .context(error::ImageNotFoundSnafu { uri: uri.clone() })?,
                    Manifest::Image(image) => image,
                };
                image.to_tarball(&uri, output, progress).await?
            }
            Format::Oci => {
                let options = OciOptions::builder()
                    .maybe_platform(platform)
                    .annotate_ref(self.annotate_ref)
                    .build();
                manifest.to_oci(&uri, output, progress, &options).await?
            }
        }

        staged
            .persist(&self.output)
            .map_err(|e| e.error)
            .context(error::FileSnafu)?;
        Ok(())
    }

    /// Create the temporary file the archive is written to before being
    /// renamed to the output path.
    fn stage_output(&self) -> Result<NamedTempFile> {
        let dir = match self.output.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let mut builder = tempfile::Builder::new();
        // Match `File::create`, which leaves the mode to the umask, rather
        // than the owner-only default of temporary files.
        #[cfg(unix)]
        builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
        builder.tempfile_in(dir).context(error::FileSnafu)
    }
}

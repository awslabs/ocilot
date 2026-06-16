//! Typed dispatch over OCI manifest documents.
//!
//! A digest reference can resolve to either an [`Index`] (image index /
//! manifest list) or an [`Image`] (image manifest). [`Manifest::fetch`]
//! inspects the `mediaType` field of the response and deserializes into the
//! correct variant rather than blindly assuming an index.

use serde::{Deserialize, Serialize};
use snafu::ResultExt;

use crate::image::Image;
use crate::index::Index;
use crate::models::MediaType;
use crate::uri::Uri;
use crate::{Result, error};

/// A manifest document fetched from a registry.
///
/// Use [`Manifest::fetch`] when the caller does not know up front whether the
/// target reference points at an index or a single-image manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Manifest {
    /// An image index / manifest list.
    Index(Index),
    /// A single image manifest.
    Image(Image),
}

impl Manifest {
    /// Fetch a manifest from a registry, dispatching on the `mediaType` field
    /// of the response so the correct variant is returned.
    ///
    /// Falls back to [`error::BodyDeserializeSnafu`] when the response is
    /// missing `mediaType` or contains an unrecognized value.
    pub async fn fetch(uri: &Uri) -> Result<Self> {
        let value = uri
            .registry()
            .fetch_manifest_value(uri.repository(), uri.reference().to_string().as_str())
            .await?;
        let media_type: Option<MediaType> = value
            .get("mediaType")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        match media_type {
            Some(MediaType::ImageIndex) | Some(MediaType::DockerManifestList) => {
                let index = serde_json::from_value(value).context(error::BodyDeserializeSnafu)?;
                Ok(Manifest::Index(index))
            }
            Some(MediaType::Manifest) | Some(MediaType::DockerManifest) => {
                let image = serde_json::from_value(value).context(error::BodyDeserializeSnafu)?;
                Ok(Manifest::Image(image))
            }
            // For anything else (including missing mediaType) try the index
            // shape first to preserve historical behaviour, then fall back to
            // the image shape so legacy / non-conforming registries still work.
            _ => {
                if value.get("manifests").is_some() {
                    let index =
                        serde_json::from_value(value).context(error::BodyDeserializeSnafu)?;
                    Ok(Manifest::Index(index))
                } else {
                    let image =
                        serde_json::from_value(value).context(error::BodyDeserializeSnafu)?;
                    Ok(Manifest::Image(image))
                }
            }
        }
    }

    /// The media type of the underlying manifest.
    pub fn media_type(&self) -> &MediaType {
        match self {
            Manifest::Index(i) => i.media_type(),
            Manifest::Image(i) => i.media_type(),
        }
    }
}

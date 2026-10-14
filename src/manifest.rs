//! Typed dispatch over OCI manifest documents.
//!
//! A digest reference can resolve to either an [`Index`] (image index /
//! manifest list) or an [`Image`] (image manifest). [`Manifest::fetch`]
//! inspects the `mediaType` field of the response and deserializes into the
//! correct variant rather than blindly assuming an index.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use tokio::io::AsyncWrite;

use crate::image::Image;
use crate::index::Index;
use crate::models::MediaType;
use crate::oci::OciOptions;
use crate::progress::SharedProgress;
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
        let bytes = uri
            .registry()
            .fetch_manifest_bytes(uri.repository(), uri.reference().to_string().as_str())
            .await?;
        Self::from_bytes(bytes).context(error::BodyDeserializeSnafu)
    }

    /// Parse a manifest document (e.g. a blob read from an OCI archive),
    /// keeping its original bytes so it can be pushed without altering its
    /// digest.
    pub fn from_slice(data: &[u8]) -> Result<Self> {
        Self::from_bytes(Bytes::copy_from_slice(data)).context(error::ImageInvalidManifestSnafu)
    }

    fn from_bytes(bytes: Bytes) -> serde_json::Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let mut manifest = Self::from_value(value)?;
        match &mut manifest {
            Manifest::Index(index) => index.set_raw(bytes),
            Manifest::Image(image) => image.set_raw(bytes),
        }
        Ok(manifest)
    }

    /// Dispatch a parsed manifest document into the matching variant.
    fn from_value(value: serde_json::Value) -> serde_json::Result<Self> {
        let media_type: Option<MediaType> = value
            .get("mediaType")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        match media_type {
            Some(MediaType::ImageIndex) | Some(MediaType::DockerManifestList) => {
                let index = serde_json::from_value(value)?;
                Ok(Manifest::Index(index))
            }
            Some(MediaType::Manifest) | Some(MediaType::DockerManifest) => {
                let image = serde_json::from_value(value)?;
                Ok(Manifest::Image(image))
            }
            // For anything else (including missing mediaType) try the index
            // shape first to preserve historical behaviour, then fall back to
            // the image shape so legacy / non-conforming registries still work.
            _ => {
                if value.get("manifests").is_some() {
                    let index = serde_json::from_value(value)?;
                    Ok(Manifest::Index(index))
                } else {
                    let image = serde_json::from_value(value)?;
                    Ok(Manifest::Image(image))
                }
            }
        }
    }

    /// Write the manifest out as an OCI image layout tar archive. See
    /// [`Index::to_oci`] and [`Image::to_oci`] for the resulting layout.
    pub async fn to_oci<W>(
        &self,
        uri: &Uri,
        output: W,
        progress: SharedProgress,
        options: &OciOptions,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        match self {
            Manifest::Index(index) => index.to_oci(uri, output, progress, options).await,
            Manifest::Image(image) => image.to_oci(uri, output, progress, options).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_slice_preserves_original_bytes() {
        // Unusual key order, whitespace and an unmodelled field would all be
        // lost by re-serializing, changing the manifest digest.
        let image = br#"{ "mediaType": "application/vnd.oci.image.manifest.v1+json", "schemaVersion": 2,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "size": 2,
                "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"},
            "layers": [], "annotations": {"a": "b"} }"#;
        let manifest = Manifest::from_slice(image).unwrap();
        let Manifest::Image(parsed) = manifest else {
            panic!("expected an image manifest");
        };
        assert_eq!(parsed.manifest_bytes().unwrap().as_ref(), image.as_slice());

        let index = br#"{"schemaVersion":2,
            "mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
        let Manifest::Index(parsed) = Manifest::from_slice(index).unwrap() else {
            panic!("expected an index");
        };
        assert_eq!(parsed.manifest_bytes().unwrap().as_ref(), index.as_slice());
    }
}

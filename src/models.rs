use base64::Engine;
use bon::Builder;
use jiff::Timestamp;
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use snafu::{OptionExt, ResultExt};
use std::env::consts;
use std::str::FromStr;
use std::{collections::HashMap, fmt};

/// Handles all the supported media type enumerations by this tool.
/// Since OCI specification allows custom types this is rather limited currently
/// but should be expanded to treat any unrecognized MediaType as a Custom variant
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaType {
    ImageIndex,
    Manifest,
    Config,
    Layer(Compression),
    DockerManifestList,
    DockerManifest,
    DockerContainerImage,
    DockerImageRootfs(Compression),
}

impl MediaType {
    pub fn compression(&self) -> Compression {
        match self {
            Self::DockerImageRootfs(compression) => {
                if *compression == Compression::None {
                    Compression::Gzip
                } else {
                    compression.clone()
                }
            }
            Self::Layer(compression) => compression.clone(),
            _ => Compression::None,
        }
    }
}

impl Serialize for MediaType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let string = match self {
            Self::ImageIndex => "application/vnd.oci.image.index.v1+json".into(),
            Self::Manifest => "application/vnd.oci.image.manifest.v1+json".into(),
            Self::Config => "application/vnd.oci.image.config.v1+json".into(),
            Self::Layer(compression) => format!(
                "application/vnd.oci.image.layer.v1.tar{}",
                compression.to_oci_suffix()
            ),
            Self::DockerManifestList => {
                "application/vnd.docker.distribution.manifest.list.v2+json".into()
            }
            Self::DockerManifest => "application/vnd.docker.distribution.manifest.v2+json".into(),
            Self::DockerContainerImage => "application/vnd.docker.container.image.v1+json".into(),
            Self::DockerImageRootfs(compression) => format!(
                "application/vnd.docker.image.rootfs.diff.tar{}",
                compression.to_docker_suffix()
            ),
        };
        serializer.serialize_str(string.as_str())
    }
}

impl<'de> Deserialize<'de> for MediaType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let string = String::deserialize(deserializer)?;
        if string.starts_with("application/vnd.docker.image.rootfs.diff.tar") {
            let compression = Compression::new(string.as_str());
            Ok(MediaType::DockerImageRootfs(compression))
        } else if string.starts_with("application/vnd.oci.image.layer.v1.tar") {
            let compression = Compression::new(string.as_str());
            Ok(MediaType::Layer(compression))
        } else {
            match string.as_ref() {
                "application/vnd.docker.distribution.manifest.list.v2+json" => {
                    Ok(MediaType::DockerManifestList)
                }
                "application/vnd.docker.distribution.manifest.v2+json" => {
                    Ok(MediaType::DockerManifest)
                }
                "application/vnd.docker.container.image.v1+json" => {
                    Ok(MediaType::DockerContainerImage)
                }
                "application/vnd.oci.image.manifest.v1+json" => Ok(MediaType::Manifest),
                "application/vnd.oci.image.index.v1+json" => Ok(MediaType::ImageIndex),
                "application/vnd.oci.image.config.v1+json" => Ok(MediaType::Config),
                variant => Err(D::Error::unknown_variant(
                    variant,
                    &[
                        "application/vnd.docker.image.rootfs.diff.tar.*",
                        "application/vnd.docker.container.image.v1+json",
                        "application/vnd.docker.distribution.manifest.list.v2+json",
                        "application/vnd.docker.distribution.manifest.v2+json",
                        "application/vnd.oci.image.index.v1+json",
                        "application/vnd.oci.image.manifest.v1+json",
                        "application/vnd.oci.image.config.v1+json",
                    ],
                )),
            }
        }
    }
}

/// Helper enum to specify the compression algorithm used
/// with a layer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compression {
    Gzip,
    Bzip2,
    Lz4,
    Xz,
    Zstd,
    None,
}

impl Compression {
    /// Detect the compression algorithm encoded in a media type string.
    ///
    /// Real-world media types use two different suffix conventions:
    /// - OCI spec: `+gzip` / `+zstd` (e.g. `application/vnd.oci.image.layer.v1.tar+gzip`)
    /// - Docker schema2: `.gzip` (full word, e.g. `...rootfs.diff.tar.gzip`)
    ///
    /// We accept both, plus the short dotted forms (`.gz`/`.zst`) for
    /// leniency with non-standard producers.
    pub fn new(string: &str) -> Self {
        if string.ends_with("+gzip") || string.ends_with(".gzip") || string.ends_with(".gz") {
            Compression::Gzip
        } else if string.ends_with("+zstd") || string.ends_with(".zstd") || string.ends_with(".zst")
        {
            Compression::Zstd
        } else if string.ends_with("+xz") || string.ends_with(".xz") {
            Compression::Xz
        } else if string.ends_with("+lz4") || string.ends_with(".lz4") {
            Compression::Lz4
        } else if string.ends_with("+bzip2")
            || string.ends_with(".bzip2")
            || string.ends_with(".bz2")
        {
            Compression::Bzip2
        } else {
            Compression::None
        }
    }

    /// File extension used for on-disk tarball filenames (e.g. `export`),
    /// independent of media-type serialization conventions.
    pub fn to_ext(&self) -> &str {
        match self {
            Self::Gzip => ".gz",
            Self::Bzip2 => ".bz2",
            Self::Lz4 => ".lz4",
            Self::Xz => ".xz",
            Self::Zstd => ".zst",
            Self::None => "",
        }
    }

    /// Suffix used when serializing an OCI `application/vnd.oci.image.layer.v1.tar*` media type.
    pub fn to_oci_suffix(&self) -> &str {
        match self {
            Self::Gzip => "+gzip",
            Self::Bzip2 => "+bzip2",
            Self::Lz4 => "+lz4",
            Self::Xz => "+xz",
            Self::Zstd => "+zstd",
            Self::None => "",
        }
    }

    /// Suffix used when serializing a Docker `application/vnd.docker.image.rootfs.diff.tar*` media type.
    pub fn to_docker_suffix(&self) -> &str {
        match self {
            Self::Gzip => ".gzip",
            Self::Bzip2 => ".bzip2",
            Self::Lz4 => ".lz4",
            Self::Xz => ".xz",
            Self::Zstd => ".zstd",
            Self::None => "",
        }
    }
}

/// This defines the format of a manifest.json file in a tarball representation of
/// an image that docker/podman/finch/nerdctl can use load on.
#[derive(Builder, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TarballManifest {
    #[builder(into)]
    pub config: String,
    #[builder(into)]
    pub repo_tags: Vec<String>,
    #[builder(into)]
    pub layers: Vec<String>,
}

/// Represents the frequently used platform identifiers both in json format and as the
/// commandline <os>/<architecture> format.
#[derive(Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Platform {
    #[builder(into)]
    pub architecture: String,
    #[builder(into)]
    pub os: String,
}

impl Default for Platform {
    fn default() -> Self {
        let arch = match consts::ARCH {
            "arm" | "aarch64" => "arm64",
            "loongarch64" => "loong64",
            _ => "amd64",
        };
        Self {
            os: "linux".to_string(),
            architecture: arch.to_string(),
        }
    }
}

impl FromStr for Platform {
    type Err = crate::error::Error;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (os, architecture) =
            value
                .split_once('/')
                .context(crate::error::InvalidPlatformFormatSnafu {
                    value: value.to_string(),
                })?;
        snafu::ensure!(
            !os.is_empty() && !architecture.is_empty(),
            crate::error::InvalidPlatformEmptySnafu {
                value: value.to_string(),
            }
        );
        Ok(Self {
            architecture: architecture.to_string(),
            os: os.to_string(),
        })
    }
}

impl TryFrom<String> for Platform {
    type Error = crate::error::Error;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        value.parse()
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}/{}", self.os, self.architecture))
    }
}

/// Represents the config block inside of an image config and frequently utilized fields
#[derive(Builder, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Config {
    #[builder(into)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[builder(into)]
    #[serde(default)]
    pub env: Vec<String>,
    #[builder(into)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[builder(into)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[builder(into)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_build: Option<String>,
    #[builder(into)]
    #[serde(default)]
    pub args_escaped: bool,
    #[builder(into)]
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Represents a history log entry in an image config
#[derive(Builder, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct History {
    #[builder(into)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<Timestamp>,
    #[builder(into)]
    #[serde(default)]
    pub created_by: String,
    #[builder(into)]
    #[serde(default)]
    pub comment: String,
    #[builder(into)]
    #[serde(default)]
    pub empty_layer: bool,
}

/// Represents the shape of an image configuration blob
#[derive(Builder, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfig {
    #[builder(into)]
    pub architecture: String,
    #[builder(into)]
    pub config: Config,
    #[builder(into)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<Timestamp>,
    #[builder(into)]
    pub history: Vec<History>,
    #[builder(into)]
    pub os: String,
}

/// Helper structure that represents the response type of a
/// list tags operation on an oci registry.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TagList {
    pub name: String,
    pub tags: Vec<String>,
}

/// Helper structure that represents the response type of a
/// catalog operation on an oci registry
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepositoryList {
    pub repositories: Vec<String>,
}

/// The officially supported error codes as defined by the OCI
/// distribution specification.
#[derive(Serialize, Deserialize, Eq, PartialEq, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// Blob unknown to registry.
    BlobUnknown,
    /// Blob upload invalid.
    BlobUploadInvalid,
    /// Blob upload unknown to registry.
    BlobUploadUnknown,
    /// Provided digest did not match uploaded content.
    DigestInvalid,
    /// Blob unknown to registry.
    ManifestBlobUnknown,
    /// Manifest invalid.
    ManifestInvalid,
    /// Manifest unknown.
    ManifestUnknown,
    /// Invalid repository name.
    NameInvalid,
    /// Repository name not known to registry.
    NameUnknown,
    /// Provided length did not match content length.
    SizeInvalid,
    /// Authentication required.
    Unauthorized,
    /// Requested access to the resource is denied.
    Denied,
    /// The operation is unsupported.
    Unsupported,
    /// Too many requests.
    #[serde(rename = "TOOMANYREQUESTS")]
    TooManyRequests,
}

/// The standard specification of an error returned from an OCI registry.
#[derive(Serialize, Deserialize, Debug)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = if let Some(message) = self.message.as_ref() {
            if let Some(detail) = self.detail.as_ref() {
                format!("{message}: {detail}")
            } else {
                message.clone()
            }
        } else if let Some(detail) = self.detail.as_ref() {
            detail.clone()
        } else {
            "unknown error occurred".to_string()
        };
        let code = match self.code {
            ErrorCode::BlobUnknown => "blob unknown",
            ErrorCode::BlobUploadInvalid => "blob upload invalid",
            ErrorCode::BlobUploadUnknown => "blob upload unknown",
            ErrorCode::Denied => "denied",
            ErrorCode::DigestInvalid => "digest invalid",
            ErrorCode::ManifestBlobUnknown => "manifest blob unknown",
            ErrorCode::ManifestInvalid => "manifest invalid",
            ErrorCode::ManifestUnknown => "manifest unknown",
            ErrorCode::NameInvalid => "name invalid",
            ErrorCode::NameUnknown => "name unknown",
            ErrorCode::SizeInvalid => "size invalid",
            ErrorCode::TooManyRequests => "too many requests",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::Unsupported => "unsupported",
        };
        f.write_fmt(format_args!("[{code}] {message}"))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ErrorResponse {
    pub errors: Vec<ErrorInfo>,
}

impl fmt::Display for ErrorResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "{}",
            self.errors
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

/// Represents an authorization token
#[derive(Clone)]
pub enum Token {
    Bearer(String),
    Basic { username: String, password: String },
}

impl fmt::Debug for Token {
    /// Manual `Debug` impl that redacts the secret so that any accidental
    /// `{:?}` logging (directly, or transitively through `Registry`/`Uri`/
    /// `Error`) never leaks credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Token::Basic { username, .. } => {
                write!(
                    f,
                    "Basic {{ username: {username:?}, password: <redacted> }}"
                )
            }
        }
    }
}

impl Token {
    /// Build a token from a parsed docker auth file entry.
    ///
    /// Returns `Ok(None)` when the entry has neither an identity token nor
    /// a basic auth blob. Returns `Err(InvalidAuth)` when the basic auth
    /// blob is present but cannot be decoded as `username:password`. This
    /// replaces the previous `unwrap`-based implementation that would
    /// panic on malformed config files (#9).
    pub fn parse(value: DockerAuth) -> Result<Option<Self>, crate::error::Error> {
        if let Some(identitytoken) = value.identitytoken {
            return Ok(Some(Self::Bearer(identitytoken)));
        }
        let Some(auth) = value.auth else {
            return Ok(None);
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth)
            .context(crate::error::AuthBase64DecodeSnafu {
                context: "docker auth",
            })?;
        let decoded = std::str::from_utf8(&decoded).context(crate::error::AuthUtf8Snafu {
            context: "docker auth",
        })?;
        let (username, password) =
            decoded
                .split_once(':')
                .context(crate::error::AuthMissingSeparatorSnafu {
                    context: "docker auth",
                })?;
        Ok(Some(Self::Basic {
            username: username.to_string(),
            password: password.to_string(),
        }))
    }
}

/// View model for the common docker/finch config for finding authorizations
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct DockerConfig {
    #[serde(default)]
    pub auths: HashMap<String, DockerAuth>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct DockerAuth {
    pub auth: Option<String>,
    pub identitytoken: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_parses_valid() {
        let p: Platform = "linux/amd64".parse().expect("should parse");
        assert_eq!(p.os, "linux");
        assert_eq!(p.architecture, "amd64");
    }

    #[test]
    fn platform_rejects_missing_separator() {
        let err = "linux".parse::<Platform>().expect_err("should fail");
        assert!(matches!(
            err,
            crate::error::Error::InvalidPlatformFormat { .. }
        ));
    }

    #[test]
    fn platform_rejects_empty_components() {
        assert!(matches!(
            "/amd64".parse::<Platform>(),
            Err(crate::error::Error::InvalidPlatformEmpty { .. })
        ));
        assert!(matches!(
            "linux/".parse::<Platform>(),
            Err(crate::error::Error::InvalidPlatformEmpty { .. })
        ));
    }

    #[test]
    fn platform_try_from_string_round_trips_display() {
        let p = Platform::try_from("linux/arm64".to_string()).unwrap();
        assert_eq!(p.to_string(), "linux/arm64");
    }

    #[test]
    fn compression_detects_real_oci_and_docker_media_types() {
        // OCI spec uses `+gzip`/`+zstd` suffixes.
        assert_eq!(
            Compression::new("application/vnd.oci.image.layer.v1.tar+gzip"),
            Compression::Gzip
        );
        assert_eq!(
            Compression::new("application/vnd.oci.image.layer.v1.tar+zstd"),
            Compression::Zstd
        );
        // Docker schema2 uses the full word with a dot separator.
        assert_eq!(
            Compression::new("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            Compression::Gzip
        );
        // Uncompressed layers should not be misdetected.
        assert_eq!(
            Compression::new("application/vnd.oci.image.layer.v1.tar"),
            Compression::None
        );
    }

    #[test]
    fn media_type_layer_round_trips_through_json() {
        for compression in [Compression::Gzip, Compression::Zstd, Compression::None] {
            let media = MediaType::Layer(compression.clone());
            let json = serde_json::to_string(&media).unwrap();
            let parsed: MediaType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, media, "round trip failed for {json}");
        }
        // Spot-check the actual wire format matches the OCI spec.
        let json = serde_json::to_string(&MediaType::Layer(Compression::Gzip)).unwrap();
        assert_eq!(json, "\"application/vnd.oci.image.layer.v1.tar+gzip\"");
    }

    #[test]
    fn token_debug_redacts_secrets() {
        let bearer = Token::Bearer("super-secret-token".to_string());
        assert!(!format!("{bearer:?}").contains("super-secret-token"));
        let basic = Token::Basic {
            username: "AWS".to_string(),
            password: "super-secret-password".to_string(),
        };
        let debug = format!("{basic:?}");
        assert!(!debug.contains("super-secret-password"));
        assert!(debug.contains("AWS"));
    }
}

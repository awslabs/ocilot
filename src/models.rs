use base64::Engine;
use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::env::consts;
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
                compression.to_ext()
            ),
            Self::DockerManifestList => {
                "application/vnd.docker.distribution.manifest.list.v2+json".into()
            }
            Self::DockerManifest => "application/vnd.docker.distribution.manifest.v2+json".into(),
            Self::DockerContainerImage => "application/vnd.docker.container.image.v1+json".into(),
            Self::DockerImageRootfs(compression) => format!(
                "application/vnd.docker.image.rootfs.diff.tar{}",
                compression.to_ext()
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
    pub fn new(string: &str) -> Self {
        if string.ends_with(".gz") || string.ends_with(".gzip2") {
            Compression::Gzip
        } else if string.ends_with(".xz") {
            Compression::Xz
        } else if string.ends_with(".lz4") {
            Compression::Lz4
        } else if string.ends_with(".zst") {
            Compression::Zstd
        } else if string.ends_with(".bz2") || string.ends_with(".bzip2") {
            Compression::Bzip2
        } else {
            Compression::None
        }
    }

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
}

/// This defines the format of a manifest.json file in a tarball representation of
/// an image that docker/podman/finch/nerdctl can use load on.
#[derive(Builder, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[builder(setter(into))]
pub struct TarballManifest {
    pub config: String,
    pub repo_tags: Vec<String>,
    pub layers: Vec<String>,
}

/// Represents the frequently used platform identifiers both in json format and as the
/// commandline <os>/<architecture> format.
#[derive(Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[builder(setter(into))]
pub struct Platform {
    pub architecture: String,
    pub os: String,
}

impl Default for Platform {
    fn default() -> Self {
        let arch = match consts::ARCH {
            "arm" | "aarch64" | "longaarch64" => "arm64",
            _ => "amd64",
        };
        Self {
            os: "linux".to_string(),
            architecture: arch.to_string(),
        }
    }
}

impl From<String> for Platform {
    fn from(value: String) -> Self {
        let (os, architecture) = value.split_once("/").unwrap();
        Self {
            architecture: architecture.to_string(),
            os: os.to_string(),
        }
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
#[builder(setter(into))]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_build: Option<String>,
    #[serde(default)]
    pub args_escaped: bool,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Represents a history log entry in an image config
#[derive(Builder, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[builder(setter(into))]
pub struct History {
    pub created: DateTime<Utc>,
    pub created_by: String,
    pub comment: String,
    #[serde(default)]
    pub empty_layer: bool,
}

/// Represents the shape of an image configuration blob
#[derive(Builder, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[builder(setter(into))]
pub struct ImageConfig {
    pub architecture: String,
    pub config: Config,
    pub created: DateTime<Utc>,
    pub history: Vec<History>,
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
            "unknown error occured".to_string()
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
#[derive(Debug, Clone)]
pub enum Token {
    Bearer(String),
    Basic { username: String, password: String },
}

impl Token {
    pub fn parse(value: DockerAuth) -> Option<Self> {
        if let Some(identitytoken) = value.identitytoken {
            Some(Self::Bearer(identitytoken))
        } else if let Some(auth) = value.auth {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth)
                .unwrap();
            let decoded = String::from_utf8_lossy(&decoded);
            let (username, password) = decoded.split_once(':').unwrap();
            Some(Self::Basic {
                username: username.to_string(),
                password: password.to_string(),
            })
        } else {
            None
        }
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

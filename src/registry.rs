use crate::client::RegistryClient;
use crate::layer::Layer;
use crate::models::{
    DockerConfig, ErrorResponse, MediaType, Platform, RepositoryList, TagList, Token,
};
use crate::uri::RegistryUri;
use crate::{Result, error};
#[cfg(feature = "aws")]
use aws_config::BehaviorVersion;
use base64::Engine;
use bytes::Bytes;
use cfg_if::cfg_if;
use futures::stream::{Stream, TryStreamExt};
use home::home_dir;
use keyring_core::Entry;
use reqwest::Response;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ResultExt, ensure};
use std::path::PathBuf;
use std::sync::OnceLock;
use url::Url;

const COMMON_AUTH_FILES: &[&str] = &[".finch/config.json", ".docker/config.json"];

/// Represents a client to a specific OCI registry.
///
/// Most requests will go through this structure. The inner `RegistryClient`
/// is `Send + Sync + Debug` by virtue of its trait bound, so no `unsafe impl`
/// is needed here.
#[derive(Clone, Debug)]
pub struct Registry {
    /// URI of the registry
    uri: RegistryUri,
    /// Registry client to use
    pub(crate) client: RegistryClient,
    #[cfg(feature = "aws")]
    is_ecr: bool,
}

impl Registry {
    /// Given a uri to a registry create a new registry client and gather
    /// the appropriate authorization.
    pub async fn new(uri: &RegistryUri) -> Result<Self> {
        let mut token: Option<Token> = None;
        #[cfg(feature = "aws")]
        let mut is_ecr = false;
        // Try AWS credential helpers first when the URI looks like ECR.
        cfg_if! {
            if #[cfg(feature = "aws")] {
                if uri.base().starts_with("public.ecr.aws") {
                    debug!(target: "registry", "using public ecr");
                    let sdk_config = aws_config::defaults(BehaviorVersion::latest()).region("us-east-1").load().await;
                    let client = aws_sdk_ecrpublic::Client::new(&sdk_config);
                    let ecr_response = client.get_authorization_token().send()
                        .await
                        .inspect_err(|e| error!("public ecr: {:?}", e))
                        .context(error::EcrPublicAuthSnafu)?;
                    trace!(target: "registry", "public ecr authorization response: {:?}", ecr_response);
                    is_ecr = true;
                    token = ecr_response.authorization_data()
                        .and_then(|x| x.authorization_token.clone()
                        .map(Token::Bearer));
                } else if uri.base().contains("ecr") {
                    debug!(target: "registry", "using private ecr");
                    let sdk_config = aws_config::load_defaults(BehaviorVersion::latest()).await;
                    let ecr_client = aws_sdk_ecr::Client::new(&sdk_config);
                    is_ecr = true;
                    let ecr_response = ecr_client.get_authorization_token()
                        .send()
                        .await
                        .context(error::EcrPrivateAuthSnafu)?;
                    trace!(target: "registry", "private ecr authorization response: {:?}", ecr_response);
                    if let Some(authorization_token) = ecr_response.authorization_data().first().and_then(|x| x.authorization_token()) {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(authorization_token)
                            .context(error::AuthBase64DecodeSnafu {
                                context: "ecr authorization token",
                            })?;
                        let decoded_str = std::str::from_utf8(&decoded)
                            .context(error::AuthUtf8Snafu {
                                context: "ecr authorization token",
                            })?;
                        let (_user, password) = decoded_str
                            .split_once(':')
                            .context(error::AuthMissingSeparatorSnafu {
                                context: "ecr authorization token",
                            })?;
                        token = Some(Token::Basic {
                            username: "AWS".to_string(),
                            password: password.to_string(),
                        });
                    }
                }
            }
        }
        // Then try config files in priority order. break on first hit so we
        // never overwrite an earlier-resolved token with a later None value.
        if token.is_none()
            && let Some(home) = home_dir()
        {
            for file in COMMON_AUTH_FILES {
                let path: PathBuf = home.join(file);
                if !path.exists() {
                    continue;
                }
                if let Some(found) = read_auth_file(&path, uri.base()).await? {
                    token = Some(found);
                    break;
                }
            }
        }
        Ok(Self {
            client: RegistryClient::new(token),
            uri: uri.clone(),
            #[cfg(feature = "aws")]
            is_ecr,
        })
    }

    /// Change the security of the registry connection
    pub fn set_secure(&mut self, flag: bool) {
        self.uri.set_secure(flag);
    }

    /// Return the registry uri for this client
    pub fn uri(&self) -> &RegistryUri {
        &self.uri
    }

    /// Convert the registry uri into the url to call
    pub fn url(&self) -> crate::Result<Url> {
        self.uri.clone().try_into()
    }

    /// Get a ecr correct repository name
    pub(crate) fn repository_name(&self, repository: &str) -> String {
        cfg_if! {
            if #[cfg(feature = "aws")] {
                if self.is_ecr {
                    if let Some(precursor) = self.uri().base().split_once('/').map(|x| x.1) {
                        format!("{}/{}", precursor, repository)
                    } else {
                        repository.to_string()
                    }
                } else {
                    repository.to_string()
                }
            } else {
                repository.to_string()
            }
        }
    }

    // Fetch the catalog of repositories in the registry
    pub async fn catalog(&self) -> crate::Result<Vec<String>> {
        let response = self.client.catalog(self.url()?).await?;
        trace!(target: "registry", "catalog: {:?}", response);
        ensure!(
            response.status().is_success(),
            error::ListReposSnafu {
                reason: response
                    .json::<ErrorResponse>()
                    .await
                    .context(error::ErrorDeserializeSnafu)?
            }
        );
        let list: RepositoryList = Self::body(response).await?;
        Ok(list.repositories)
    }

    /// Check for the existence of a blob in the registry
    pub(crate) async fn check_blob(&self, repository: &str, digest: &str) -> Result<bool> {
        let repository = self.repository_name(repository);
        let response = self
            .client
            .head_blob(self.url()?, repository, digest.into())
            .await?;
        trace!(target: "registry", "head_blob: {:?}", response);
        Ok(response.status().is_success())
    }

    /// Fetch a blob from the registry
    pub(crate) async fn fetch_blob(
        &self,
        repository: &str,
        digest: &str,
    ) -> Result<(
        impl Stream<Item = std::result::Result<Bytes, std::io::Error>> + use<>,
        u64,
    )> {
        let repository = self.repository_name(repository);
        let response = self
            .client
            .get_blob(self.url()?, repository, digest.into())
            .await?;
        trace!(target: "registry", "get_blob: {:?}", response);
        ensure!(
            response.status().is_success(),
            error::FetchBlobSnafu {
                reason: response
                    .json::<ErrorResponse>()
                    .await
                    .context(error::ErrorDeserializeSnafu)?
            }
        );
        let size: u64 = response
            .headers()
            .clone()
            .get("Content-Length")
            .context(error::ContentLengthMissingSnafu)?
            .to_str()
            .context(error::ImproperHeaderSnafu)?
            .parse()
            .context(error::ContentLengthNotNumberSnafu)?;
        Ok((response.bytes_stream().map_err(std::io::Error::other), size))
    }

    /// Delete a blob from the registry
    pub(crate) async fn delete_blob(&self, repository: &str, digest: &str) -> Result<()> {
        let repository = self.repository_name(repository);
        let response = self
            .client
            .del_blob(self.url()?, repository, digest.into())
            .await?;
        trace!(target: "registry", "del_blob: {:?}", response);
        ensure!(
            response.status().is_success(),
            error::DeleteBlobSnafu {
                digest,
                reason: response
                    .json::<ErrorResponse>()
                    .await
                    .context(error::ErrorDeserializeSnafu)?
            }
        );
        Ok(())
    }

    /// Check for the existence of a manifest in the registry
    pub(crate) async fn check_manifest(&self, repository: &str, reference: &str) -> Result<bool> {
        let repository = self.repository_name(repository);
        let response = self
            .client
            .head_manifest(self.url()?, repository, reference.into())
            .await?;
        trace!(target: "registry", "head_manifest: {:?}", response);
        Ok(response.status().is_success())
    }

    /// Fetch a manifest from the registry, this could be an Image Index or an Image manifest
    pub(crate) async fn fetch_manifest<T>(&self, repository: &str, reference: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let repository = self.repository_name(repository);
        let response = self
            .client
            .get_manifest(self.url()?, repository, reference.into())
            .await?;
        trace!(target: "registry", "get_manifest: {:?}", response);
        ensure!(
            response.status().is_success(),
            error::FetchManifestSnafu {
                reason: response
                    .json::<ErrorResponse>()
                    .await
                    .context(error::ErrorDeserializeSnafu)?
            }
        );
        Self::body(response).await
    }

    /// Push a manifest to the oci registtry
    pub(crate) async fn push_manifest<T>(
        &self,
        media_type: &MediaType,
        repository: &str,
        reference: &str,
        manifest: &T,
        platform: Option<Platform>,
    ) -> Result<Layer>
    where
        T: Serialize,
    {
        let repository = self.repository_name(repository);
        let bytes = serde_json::to_vec(manifest).context(error::SerializeSnafu)?;
        let size = bytes.len();
        let hash = Sha256::digest(bytes.as_slice());
        let digest = format!("sha256:{}", base16::encode_lower(hash.as_slice()));
        let response = self
            .client
            .put_manifest(
                self.url()?,
                repository,
                reference.into(),
                Bytes::from_owner(bytes),
            )
            .await?;
        trace!(target: "registry", "put_manifest: {:?}", response);
        ensure!(
            response.status().is_success(),
            error::PushImageSnafu {
                uri: self.url()?.clone(),
                reason: response
                    .json::<ErrorResponse>()
                    .await
                    .context(error::ErrorDeserializeSnafu)?
            }
        );
        Ok(Layer::builder()
            .digest(digest.clone())
            .media_type(media_type.clone())
            .size(size)
            .maybe_platform(platform)
            .build())
    }

    /// Get the list of tags in a repository on this registry
    pub(crate) async fn get_tags(&self, repository: &str) -> Result<Vec<String>> {
        let repository_name = self.repository_name(repository);
        let response = self
            .client
            .get_tags(&self.url()?, repository_name.as_str())
            .await?;
        trace!(target: "registry", "get_tags: {:?}", response);
        ensure!(
            response.status().is_success(),
            error::ListTagsSnafu {
                reason: response
                    .json::<ErrorResponse>()
                    .await
                    .context(error::ErrorDeserializeSnafu)?
            }
        );
        let taglist: TagList = Self::body(response).await?;
        let mut tags = taglist.tags.clone();
        tags.sort();
        Ok(tags)
    }

    /// Delete a tag in the registry in the given repository
    pub(crate) async fn delete_tag(&self, repository: &str, tag: &str) -> Result<()> {
        let repository = self.repository_name(repository);
        let response = self
            .client
            .del_manifest(self.url()?, repository, tag.into())
            .await?;
        trace!(target: "registry", "del_tag: {:?}", response);
        ensure!(
            response.status().is_success(),
            error::DeleteTagSnafu {
                tag: tag.to_string(),
                reason: response
                    .json::<ErrorResponse>()
                    .await
                    .context(error::ErrorDeserializeSnafu)?
            }
        );

        Ok(())
    }

    /// Handles deserialization of responses with proper logging
    pub(crate) async fn body<T>(response: Response) -> crate::Result<T>
    where
        T: DeserializeOwned,
    {
        let value: serde_json::Value = response
            .json()
            .await
            .context(error::ResponseDeserializeSnafu)?;
        if tracing::enabled!(tracing::Level::TRACE)
            && let Ok(pretty) = serde_json::to_string_pretty(&value)
        {
            trace!(target: "registry", "RESPONSE BODY: {}", pretty);
        }
        serde_json::from_value(value).context(error::BodyDeserializeSnafu)
    }
}

/// Initialize the platform-native keyring store on first use.
///
/// `keyring` 4.x requires an explicit default credential store. We pick the
/// OS-native one (Keychain on macOS, Credential Manager on Windows, keyutils
/// on Linux, etc.) the first time the auth-file fallback path needs it.
/// Returns true when a store is available; false when initialization fails,
/// in which case callers should treat the keyring as unavailable.
fn ensure_keyring_store() -> bool {
    static INIT: OnceLock<bool> = OnceLock::new();
    *INIT.get_or_init(|| keyring::use_native_store(false).is_ok())
}

/// Read a single docker/finch config file and resolve any matching auth
/// for the given registry base. Returns Some(token) only when the file
/// produced a token; never overwrites caller state with None.
async fn read_auth_file(path: &std::path::Path, registry_base: &str) -> Result<Option<Token>> {
    let auth = tokio::fs::read_to_string(path)
        .await
        .context(error::FileSnafu)?;
    let config: DockerConfig =
        serde_json::from_str(&auth).context(error::ConfigDeserializeSnafu)?;
    let Some(entry) = config.auths.get(registry_base) else {
        return Ok(None);
    };
    if entry.auth.is_none() && entry.identitytoken.is_none() {
        // Fall back to the system keyring (docker-credential-helpers).
        if !ensure_keyring_store() {
            return Ok(None);
        }
        let Ok(keyring_entry) = Entry::new("docker-credential-helpers", registry_base) else {
            return Ok(None);
        };
        let Ok(password) = keyring_entry.get_password() else {
            return Ok(None);
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&password)
            .context(error::AuthBase64DecodeSnafu {
                context: "keyring credential",
            })?;
        let decoded_str = std::str::from_utf8(&decoded).context(error::AuthUtf8Snafu {
            context: "keyring credential",
        })?;
        if let Some((username, password)) = decoded_str.split_once(':') {
            return Ok(Some(Token::Basic {
                username: username.to_string(),
                password: password.to_string(),
            }));
        }
        return Ok(Some(Token::Bearer(decoded_str.to_string())));
    }
    Token::parse(entry.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_auth_file_ignores_missing_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        tokio::fs::write(&path, r#"{"auths":{"other.io":{"auth":"dXNlcjpwYXNz"}}}"#)
            .await
            .unwrap();
        let res = read_auth_file(&path, "absent.io").await.unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn read_auth_file_decodes_basic_auth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // base64("user:pass") = "dXNlcjpwYXNz"
        tokio::fs::write(&path, r#"{"auths":{"present.io":{"auth":"dXNlcjpwYXNz"}}}"#)
            .await
            .unwrap();
        let res = read_auth_file(&path, "present.io").await.unwrap();
        match res {
            Some(Token::Basic { username, password }) => {
                assert_eq!(username, "user");
                assert_eq!(password, "pass");
            }
            other => panic!("expected basic auth, got {other:?}"),
        }
    }
}

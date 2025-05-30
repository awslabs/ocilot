use crate::client::RegistryClient;
use crate::layer::{Layer, LayerBuilder};
use crate::models::{
    DockerConfig, ErrorResponse, MediaType, Platform, RepositoryList, TagList, Token,
};
use crate::uri::RegistryUri;
use crate::{error, Result};
#[cfg(feature = "aws")]
use aws_config::BehaviorVersion;
use base64::Engine;
use bytes::Bytes;
use cfg_if::cfg_if;
use futures::stream::{Stream, TryStreamExt};
use homedir::my_home;
use keyring::Entry;
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use snafu::{ensure, OptionExt, ResultExt};
use url::Url;

const COMMON_AUTH_FILES: &[&str] = &[".finch/config.json", ".docker/config.json"];

/// Represents a client to a specific OCI registry.
/// Most requests will go through this structure
#[derive(Clone, Debug)]
pub struct Registry {
    /// URI of the registry
    uri: RegistryUri,
    /// Registry client to use
    pub(crate) client: RegistryClient,
}

unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

async fn discover_auth(registry: &RegistryUri) -> crate::Result<Option<Token>> {
    // First check our common auth files for an entry
    for file in COMMON_AUTH_FILES {
        if let Ok(Some(path)) = my_home() {
            let path = path.join(file);
            if path.exists() {
                let auth = tokio::fs::read_to_string(path)
                    .await
                    .context(error::FileSnafu)?;
                let config: DockerConfig =
                    serde_json::from_str(&auth).context(error::ConfigDeserializeSnafu)?;
                if let Some(entry) = config.auths.get(registry.base()) {
                    // If both the auth and identity token are null then the password is probably stored in the system keychai
                    if entry.auth.is_none() && entry.identitytoken.is_none() {
                        if let Ok(entry) = Entry::new("docker-credential-helpers", registry.base())
                        {
                            if let Ok(password) = entry.get_password() {
                                let decoded = base64::engine::general_purpose::STANDARD
                                    .decode(password)
                                    .unwrap();
                                let decoded = String::from_utf8_lossy(decoded.as_slice());
                                if decoded.contains(':') {
                                    let (username, password) = decoded.split_once(':').unwrap();
                                    return Ok(Some(Token::Basic {
                                        username: username.to_string(),
                                        password: password.to_string(),
                                    }));
                                } else {
                                    return Ok(Some(Token::Bearer(decoded.to_string())));
                                }
                            } else {
                                return Ok(None);
                            }
                        }
                    }
                    return Ok(Token::parse(entry.clone()));
                }
            }
        }
    }
    // If we get here then we may want to try and utilize credential helpers for given registry types
    cfg_if! {
        if #[cfg(feature = "aws")] {
            if registry.base().starts_with("public.ecr.aws") {
                debug!(target: "registry", "using public ecr");
                // Public ecr
                let sdk_config = aws_config::defaults(BehaviorVersion::latest()).region("us-east-1").load().await;
                let ecr_client = aws_sdk_ecrpublic::Client::new(&sdk_config);
                let ecr_response = ecr_client.get_authorization_token().send()
                    .await
                    .map_err(|e| { error!("public ecr: {:?}", e); error::Error::Authorization { reason: e.to_string() } })?;
                trace!(target: "registry", "public ecr authorization response: {:?}", ecr_response);
                Ok(ecr_response.authorization_data()
                    .and_then(|x| x.authorization_token.clone()
                    .map(Token::Bearer)))
            } else if registry.base().contains("ecr") {
                debug!(target: "registry", "using private ecr");
                let sdk_config = aws_config::load_defaults(BehaviorVersion::latest()).await;
                let ecr_client = aws_sdk_ecr::Client::new(&sdk_config);
                let ecr_response = ecr_client.get_authorization_token()
                    .send()
                    .await
                    .map_err(|e| error::Error::Authorization { reason: e.to_string() })?;
                trace!(target: "registry", "private ecr authorization response: {:?}", ecr_response);
                Ok(ecr_response.authorization_data()
                    .first()
                    .and_then(|x| {
                        x.authorization_token().map(|y| {
                            let decoded = base64::engine::general_purpose::STANDARD.decode(y).unwrap();
                            Token::Basic { username: "AWS".to_string(), password: String::from_utf8_lossy(decoded.as_slice()).strip_prefix("AWS:").unwrap().to_string() }
                        })
                    }))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }
}

impl Registry {
    /// Given a uri to a registry create a new registry client and gather
    /// the appropriate authorization.
    pub async fn new(uri: &RegistryUri) -> Result<Self> {
        let token = discover_auth(uri).await?;
        Ok(Self {
            client: RegistryClient::new(token),
            uri: uri.clone(),
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

    // Fetch the catalog of repositories in the registry
    pub async fn catalog(&self) -> crate::Result<Vec<String>> {
        let response = self.client.clone().catalog(self.url()?).await?;
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
        let response = self
            .client
            .clone()
            .head_blob(self.url()?, repository.into(), digest.into())
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
        impl Stream<Item = std::result::Result<Bytes, std::io::Error>>,
        u64,
    )> {
        let response = self
            .client
            .clone()
            .get_blob(self.url()?, repository.into(), digest.into())
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
        let response = self
            .client
            .del_blob(self.url()?, repository.into(), digest.into())
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
        let response = self
            .client
            .head_manifest(self.url()?, repository.into(), reference.into())
            .await?;
        trace!(target: "registry", "head_manifest: {:?}", response);
        Ok(response.status().is_success())
    }

    /// Fetch a manifest from the registry, this could be an Image Index or an Image manifest
    pub(crate) async fn fetch_manifest<T>(&self, repository: &str, reference: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .client
            .get_manifest(self.url()?, repository.into(), reference.into())
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
        let bytes = serde_json::to_vec(manifest).context(error::SerializeSnafu)?;
        let size = bytes.len();
        let hash = Sha256::digest(bytes.as_slice());
        let digest = format!("sha256:{}", base16::encode_lower(hash.as_slice()));
        let response = self
            .client
            .put_manifest(
                self.url()?,
                repository.into(),
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
        LayerBuilder::default()
            .digest(digest.clone())
            .media_type(media_type.clone())
            .size(size)
            .platform(platform)
            .build()
            .context(error::LayerSnafu)
    }

    /// Get the list of tags in a repository on this registry
    pub(crate) async fn get_tags(&self, repository: &str) -> Result<Vec<String>> {
        let response = self.client.get_tags(&self.url()?, repository).await?;
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
        Ok(taglist.tags)
    }

    /// Delete a tag in the registry in the given repository
    pub(crate) async fn delete_tag(&self, repository: &str, tag: &str) -> Result<()> {
        let response = self
            .client
            .del_manifest(self.url()?, repository.into(), tag.into())
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
        trace!(target: "registry", "RESPONSE BODY: {}", serde_json::to_string_pretty(&value).unwrap());
        serde_json::from_value(value).context(error::BodyDeserializeSnafu)
    }
}

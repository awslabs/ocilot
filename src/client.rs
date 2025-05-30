use std::fmt::Debug;
use std::sync::Arc;

use crate::models::Token;
use crate::{error, Result};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{RequestBuilder, Response};
use snafu::ResultExt;
use url::Url;

/// A trait for a client implementing requests to an oci registry. This is primarily implemented
/// to allow for ease of unittesting this crate.
#[async_trait]
pub(crate) trait RegistryClientImpl: Send + Sync + Debug {
    /// GET {uri}/v2/_catalog
    async fn catalog(&self, uri: &Url) -> Result<Response>;
    /// GET {uri}/v2/{repository}/tags/list
    async fn get_tags(&self, uri: &Url, repository: &str) -> Result<Response>;
    /// HEAD {uri}/v2/{repository}/blobs/{digest}
    async fn head_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response>;
    /// GET {uri}/v2/{repository}/blobs/{digest}
    async fn get_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response>;
    /// DELETE {uri}/v2/{repository}/blobs/{digest}
    async fn del_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response>;
    /// POST {url}/v2/{repository}/blobs/uploads/
    async fn post_blob(
        &self,
        uri: &Url,
        repository: &str,
        data: Bytes,
        digest: &str,
    ) -> Result<Response>;
    /// POST {url}/v2/{repository}/blobs/uploads/ START chunked upload
    async fn start_upload(&self, uri: &Url, repository: &str) -> Result<Response>;
    /// PATCH {url}/v2/{upload_url}
    async fn upload_part(
        &self,
        uri: &Url,
        upload: &str,
        data: Bytes,
        start: usize,
        end: usize,
    ) -> Result<Response>;
    /// PUT {url}/v2/{upload_url}
    async fn finish_blob_upload(
        &self,
        uri: &Url,
        upload: &str,
        data: Bytes,
        digest: &str,
        start: usize,
        end: usize,
    ) -> Result<Response>;
    /// HEAD {uri}/v2/{repository}/manifests/{reference}
    async fn head_manifest(&self, uri: &Url, repository: &str, reference: &str)
        -> Result<Response>;
    /// GET {uri}/v2/{repository}/manifests/{reference}
    async fn get_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response>;
    /// PUT {uri}/v2/{repository}/manifests/{reference}
    async fn put_manifest(
        &self,
        uri: &Url,
        repository: &str,
        reference: &str,
        body: Bytes,
    ) -> Result<Response>;
    /// DELETE {uri}/v2/{repository}/manifests/{reference}
    async fn del_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response>;
}

/// Implements a simple registry client using reqwest
#[derive(Debug)]
pub struct SimpleRegistryClient {
    client: reqwest::Client,
    auth: Option<Token>,
}

impl SimpleRegistryClient {
    pub fn new(auth: Option<Token>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth,
        }
    }

    pub(crate) fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(token) = self.auth.as_ref() {
            match token {
                Token::Bearer(t) => request.bearer_auth(t),
                Token::Basic { username, password } => request.basic_auth(username, Some(password)),
            }
        } else {
            request
        }
    }
}

#[async_trait]
impl RegistryClientImpl for SimpleRegistryClient {
    async fn catalog(&self, uri: &Url) -> Result<Response> {
        let request = self
            .client
            .get(uri.join("/v2/_catalog").context(error::UrlSnafu)?);
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn head_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self.client.head(
            uri.join(&format!("/v2/{}/blobs/{}", repository, digest))
                .context(error::UrlSnafu)?,
        );
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn get_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self.client.get(
            uri.join(&format!("/v2/{}/blobs/{}", repository, digest))
                .context(error::UrlSnafu)?,
        );
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn del_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self.client.delete(
            uri.join(&format!("/v2/{}/blobs/{}", repository, digest))
                .context(error::UrlSnafu)?,
        );
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn get_tags(&self, uri: &Url, repository: &str) -> Result<Response> {
        let request = self.client.get(
            uri.join(&format!("/v2/{}/tags/list", repository))
                .context(error::UrlSnafu)?,
        );
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn post_blob(
        &self,
        uri: &Url,
        repository: &str,
        data: Bytes,
        digest: &str,
    ) -> Result<Response> {
        let mut uri = uri
            .join(&format!("/v2/{}/blobs/uploads/", repository))
            .context(error::UrlSnafu)?;
        uri.set_query(Some(format!("digest={digest}").as_str()));
        let request = self.client.post(uri);
        self.auth(request)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", data.len())
            .body(data)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn start_upload(&self, uri: &Url, repository: &str) -> Result<Response> {
        let request = self.client.post(
            uri.join(&format!("/v2/{}/blobs/uploads/", repository))
                .context(error::UrlSnafu)?,
        );
        self.auth(request)
            .header("Content-Length", 0)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn upload_part(
        &self,
        uri: &Url,
        upload: &str,
        data: Bytes,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        let request = self.client.patch(
            uri.join(&format!("/v2/{}/blobs/uploads/{}", upload, upload))
                .context(error::UrlSnafu)?,
        );
        self.auth(request)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", data.len())
            .header("Content-Range", format!("{}-{}", start, end))
            .body(data)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn finish_blob_upload(
        &self,
        uri: &Url,
        upload: &str,
        data: Bytes,
        digest: &str,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        let mut uri = uri
            .join(&format!("/v2/{}/blobs/uploads/{}", upload, upload))
            .context(error::UrlSnafu)?;
        uri.set_query(Some(format!("digest={digest}").as_str()));
        let request = self.client.put(uri);
        self.auth(request)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", data.len())
            .header("Content-Range", format!("{}-{}", start, end))
            .body(data)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn head_manifest(
        &self,
        uri: &Url,
        repository: &str,
        reference: &str,
    ) -> Result<Response> {
        let request = self.client.head(
            uri.join(&format!("/v2/{}/manifests/{}", repository, reference))
                .context(error::UrlSnafu)?,
        );
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn get_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response> {
        let request = self.client.get(
            uri.join(&format!("/v2/{}/manifests/{}", repository, reference))
                .context(error::UrlSnafu)?,
        );
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn put_manifest(
        &self,
        uri: &Url,
        repository: &str,
        reference: &str,
        body: Bytes,
    ) -> Result<Response> {
        let request = self.client.put(
            uri.join(&format!("/v2/{}/manifests/{}", repository, reference))
                .context(error::UrlSnafu)?,
        );
        self.auth(request)
            .body(body)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn del_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response> {
        let request = self.client.delete(
            uri.join(&format!("/v2/{}/manifests/{}", repository, reference))
                .context(error::UrlSnafu)?,
        );
        self.auth(request).send().await.context(error::RequestSnafu)
    }
}

/// Handle to a registry client. This primarily is utilized as an intercept point for unittesting
#[derive(Clone, Debug)]
pub struct RegistryClient {
    client: Arc<dyn RegistryClientImpl>,
}

impl RegistryClient {
    pub fn new(auth: Option<Token>) -> Self {
        Self {
            client: Arc::new(SimpleRegistryClient::new(auth)),
        }
    }

    pub async fn catalog(&self, uri: Url) -> Result<Response> {
        self.client.catalog(&uri).await
    }

    pub async fn head_blob(
        &self,
        uri: Url,
        repository: String,
        digest: String,
    ) -> Result<Response> {
        self.client
            .head_blob(&uri, repository.as_str(), digest.as_str())
            .await
    }

    pub async fn get_blob(&self, uri: Url, repository: String, digest: String) -> Result<Response> {
        self.client
            .get_blob(&uri, repository.as_str(), digest.as_str())
            .await
    }

    pub async fn del_blob(&self, uri: Url, repository: String, digest: String) -> Result<Response> {
        self.client
            .del_blob(&uri, repository.as_str(), digest.as_str())
            .await
    }

    pub async fn get_tags(&self, uri: &Url, repository: &str) -> Result<Response> {
        self.client.get_tags(uri, repository).await
    }

    pub async fn post_blob(
        self,
        uri: Url,
        repository: String,
        data: Bytes,
        digest: String,
    ) -> Result<Response> {
        self.client
            .as_ref()
            .post_blob(&uri, repository.as_str(), data, digest.as_str())
            .await
    }

    pub async fn start_upload(self, uri: Url, repository: String) -> Result<Response> {
        self.client
            .as_ref()
            .start_upload(&uri, repository.as_str())
            .await
    }

    pub async fn upload_part(
        self,
        uri: Url,
        upload: String,
        data: Bytes,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        self.client
            .as_ref()
            .upload_part(&uri, upload.as_str(), data, start, end)
            .await
    }

    pub async fn finish_blob_upload(
        self,
        uri: Url,
        upload: String,
        data: Bytes,
        digest: String,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        self.client
            .as_ref()
            .finish_blob_upload(&uri, upload.as_str(), data, digest.as_str(), start, end)
            .await
    }

    pub async fn head_manifest(
        &self,
        uri: Url,
        repository: String,
        reference: String,
    ) -> Result<Response> {
        self.client
            .head_manifest(&uri, repository.as_str(), reference.as_str())
            .await
    }

    pub async fn get_manifest(
        &self,
        uri: Url,
        repository: String,
        reference: String,
    ) -> Result<Response> {
        self.client
            .get_manifest(&uri, repository.as_str(), reference.as_str())
            .await
    }

    pub async fn put_manifest(
        &self,
        uri: Url,
        repository: String,
        reference: String,
        body: Bytes,
    ) -> Result<Response> {
        self.client
            .put_manifest(&uri, repository.as_str(), reference.as_str(), body)
            .await
    }

    pub async fn del_manifest(
        &self,
        uri: Url,
        repository: String,
        reference: String,
    ) -> Result<Response> {
        self.client
            .del_manifest(&uri, repository.as_str(), reference.as_str())
            .await
    }
}

unsafe impl Send for RegistryClient {}
unsafe impl Sync for RegistryClient {}

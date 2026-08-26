use std::fmt::Debug;
use std::sync::Arc;

use crate::models::Token;
use crate::{Result, error};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::LOCATION;
use reqwest::{RequestBuilder, Response};
use snafu::{OptionExt, ResultExt};
use url::Url;

/// Accept header sent on manifest requests so the registry returns the
/// canonical manifest representation rather than an arbitrary default.
/// Includes both OCI and Docker media types for maximum compatibility.
pub(crate) const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.docker.distribution.manifest.v2+json",
);

/// A trait for a client implementing requests to an OCI registry.
///
/// This is primarily implemented to allow for ease of unit testing this crate.
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
    /// POST {url}/v2/{repository}/blobs/uploads/?digest={digest} (monolithic upload)
    async fn post_blob(
        &self,
        uri: &Url,
        repository: &str,
        data: Bytes,
        digest: &str,
    ) -> Result<Response>;
    /// POST {url}/v2/{repository}/blobs/uploads/ to start a chunked upload.
    async fn start_upload(&self, uri: &Url, repository: &str) -> Result<Response>;
    /// PATCH against an upload URL returned by the registry.
    async fn upload_part(
        &self,
        upload_url: &Url,
        data: Bytes,
        start: usize,
        end: usize,
    ) -> Result<Response>;
    /// PUT against an upload URL with the final digest query parameter.
    async fn finish_blob_upload(
        &self,
        upload_url: &Url,
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
        content_type: &str,
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

/// Build a `/v2/...` URL from literal + dynamic path segments.
///
/// Unlike `format!("/v2/{repo}/blobs/{digest}")` followed by `Url::join`,
/// this percent-encodes each segment independently via `path_segments_mut`
/// and never interprets `/`, `..`, `?`, or `#` inside a segment specially.
/// That matters because `repository`/`digest`/`reference` values can
/// originate from untrusted registry responses (e.g. a manifest digest read
/// back from a third-party registry during `copy`); without this, such a
/// value could inject extra path segments or otherwise redirect the request
/// to an unintended endpoint on the same host.
fn v2_url<'a>(base: &Url, segments: impl IntoIterator<Item = &'a str>) -> Result<Url> {
    let mut url = base.join("/v2/").context(error::UrlSnafu)?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| error::Error::Internal {
                context: "registry url cannot be used as a base for path segments",
            })?;
        path.pop_if_empty();
        path.extend(segments);
    }
    Ok(url)
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
        let request = self
            .client
            .head(v2_url(uri, repository.split('/').chain(["blobs", digest]))?);
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn get_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self
            .client
            .get(v2_url(uri, repository.split('/').chain(["blobs", digest]))?);
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn del_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self
            .client
            .delete(v2_url(uri, repository.split('/').chain(["blobs", digest]))?);
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn get_tags(&self, uri: &Url, repository: &str) -> Result<Response> {
        let request = self
            .client
            .get(v2_url(uri, repository.split('/').chain(["tags", "list"]))?);
        self.auth(request).send().await.context(error::RequestSnafu)
    }

    async fn post_blob(
        &self,
        uri: &Url,
        repository: &str,
        data: Bytes,
        digest: &str,
    ) -> Result<Response> {
        let mut uri = v2_url(uri, repository.split('/').chain(["blobs", "uploads", ""]))?;
        uri.query_pairs_mut().append_pair("digest", digest);
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
        let request = self.client.post(v2_url(
            uri,
            repository.split('/').chain(["blobs", "uploads", ""]),
        )?);
        self.auth(request)
            .header("Content-Length", 0)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn upload_part(
        &self,
        upload_url: &Url,
        data: Bytes,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        // Content-Range uses inclusive endpoints per RFC 7233; callers pass the
        // exclusive end (`start + len`) so subtract 1 for the wire format.
        let last = end.saturating_sub(1);
        let request = self.client.patch(upload_url.clone());
        self.auth(request)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", data.len())
            .header("Content-Range", format!("{}-{}", start, last))
            .body(data)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn finish_blob_upload(
        &self,
        upload_url: &Url,
        data: Bytes,
        digest: &str,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        let mut uri = upload_url.clone();
        // Append digest while preserving any pre-existing query params placed
        // there by the registry (some, like ECR, embed signature material).
        let new_query = match uri.query() {
            Some(existing) if !existing.is_empty() => format!("{existing}&digest={digest}"),
            _ => format!("digest={digest}"),
        };
        uri.set_query(Some(&new_query));
        let request = self.client.put(uri);
        let request = if data.is_empty() {
            self.auth(request).header("Content-Length", 0)
        } else {
            // Inclusive end byte per RFC 7233 / OCI distribution spec.
            let last = end.saturating_sub(1);
            self.auth(request)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", data.len())
                .header("Content-Range", format!("{}-{}", start, last))
                .body(data)
        };
        request.send().await.context(error::RequestSnafu)
    }

    async fn head_manifest(
        &self,
        uri: &Url,
        repository: &str,
        reference: &str,
    ) -> Result<Response> {
        let request = self.client.head(v2_url(
            uri,
            repository.split('/').chain(["manifests", reference]),
        )?);
        self.auth(request)
            .header("Accept", MANIFEST_ACCEPT)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn get_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response> {
        let request = self.client.get(v2_url(
            uri,
            repository.split('/').chain(["manifests", reference]),
        )?);
        self.auth(request)
            .header("Accept", MANIFEST_ACCEPT)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn put_manifest(
        &self,
        uri: &Url,
        repository: &str,
        reference: &str,
        content_type: &str,
        body: Bytes,
    ) -> Result<Response> {
        let request = self.client.put(v2_url(
            uri,
            repository.split('/').chain(["manifests", reference]),
        )?);
        self.auth(request)
            .header("Content-Type", content_type)
            .header("Content-Length", body.len())
            .body(body)
            .send()
            .await
            .context(error::RequestSnafu)
    }

    async fn del_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response> {
        let request = self.client.delete(v2_url(
            uri,
            repository.split('/').chain(["manifests", reference]),
        )?);
        self.auth(request).send().await.context(error::RequestSnafu)
    }
}

/// Handle to OCI registry HTTP operations.
///
/// Wraps the underlying client implementation to enable dependency injection
/// and unit testing. The inner trait already requires `Send + Sync + Debug`,
/// so `Arc<dyn ...>` provides safe sharing without any `unsafe impl`.
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
        &self,
        uri: Url,
        repository: String,
        data: Bytes,
        digest: String,
    ) -> Result<Response> {
        self.client
            .post_blob(&uri, repository.as_str(), data, digest.as_str())
            .await
    }

    pub async fn start_upload(&self, uri: Url, repository: String) -> Result<Response> {
        self.client.start_upload(&uri, repository.as_str()).await
    }

    pub async fn upload_part(
        &self,
        upload_url: Url,
        data: Bytes,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        self.client.upload_part(&upload_url, data, start, end).await
    }

    pub async fn finish_blob_upload(
        &self,
        upload_url: Url,
        data: Bytes,
        digest: String,
        start: usize,
        end: usize,
    ) -> Result<Response> {
        self.client
            .finish_blob_upload(&upload_url, data, digest.as_str(), start, end)
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
        content_type: &str,
        body: Bytes,
    ) -> Result<Response> {
        self.client
            .put_manifest(
                &uri,
                repository.as_str(),
                reference.as_str(),
                content_type,
                body,
            )
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

/// Extract the `Location` header from a response and resolve it against the
/// request base URL. Registries are allowed to return either an absolute URL
/// (ECR commonly does this with a different host) or a relative path.
pub(crate) fn extract_location(response: &Response, base: &Url) -> crate::Result<Url> {
    let header = response
        .headers()
        .get(LOCATION)
        .context(error::StartBlobNoLocationSnafu)?
        .to_str()
        .context(error::ImproperHeaderSnafu)?;
    base.join(header).context(error::UrlSnafu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_url_builds_expected_path() {
        let base = Url::parse("https://registry.example.com").unwrap();
        let url = v2_url(&base, "org/app".split('/').chain(["blobs", "sha256:abc"])).unwrap();
        assert_eq!(
            url.as_str(),
            "https://registry.example.com/v2/org/app/blobs/sha256:abc"
        );
    }

    #[test]
    fn v2_url_percent_encodes_untrusted_segments() {
        // A malicious/compromised registry could return a digest-shaped
        // value containing path-traversal or path-injection characters.
        // Each segment must stay confined to a single percent-encoded path
        // component rather than being able to introduce extra segments.
        let base = Url::parse("https://registry.example.com").unwrap();
        let malicious_digest = "sha256:../../../etc/passwd";
        let url = v2_url(
            &base,
            "org/app".split('/').chain(["blobs", malicious_digest]),
        )
        .unwrap();
        // The traversal characters are percent-encoded within the final
        // path segment, so the request cannot escape /v2/org/app/blobs/.
        assert!(url.path().starts_with("/v2/org/app/blobs/"));
        assert_eq!(url.path_segments().unwrap().count(), 5);
        assert!(!url.path().contains("/etc/passwd"));
    }
}

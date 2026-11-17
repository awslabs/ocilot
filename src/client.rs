use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::models::Token;
use crate::{Result, error};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::{LOCATION, WWW_AUTHENTICATE};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use snafu::{OptionExt, ResultExt};
use url::{Origin, Url};

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
    /// Tokens from answering bearer challenges, keyed by the origin that
    /// issued the challenge so they are never sent anywhere else. `Token`'s
    /// `Debug` keeps them redacted.
    challenge_tokens: Mutex<HashMap<Origin, Token>>,
}

/// Cap on a token endpoint's response; real ones are a few KiB.
const MAX_TOKEN_RESPONSE: usize = 1 << 20;

impl SimpleRegistryClient {
    pub fn new(auth: Option<Token>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth,
            challenge_tokens: Mutex::new(HashMap::new()),
        }
    }

    fn tokens(&self) -> MutexGuard<'_, HashMap<Origin, Token>> {
        self.challenge_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Attach a token `origin` issued, else the configured credentials.
    fn auth(&self, request: RequestBuilder, origin: &Origin) -> RequestBuilder {
        let issued = self.tokens().get(origin).cloned();
        match issued.as_ref().or(self.auth.as_ref()) {
            Some(Token::Bearer(t)) => request.bearer_auth(t),
            Some(Token::Basic { username, password }) => {
                request.basic_auth(username, Some(password))
            }
            None => request,
        }
    }

    /// Send `request` with credentials. Registries such as Docker Hub and GHCR
    /// answer with a 401 bearer challenge; it is answered once and the request
    /// retried with the issued token, which later requests to that origin reuse.
    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        let (client, request) = request.build_split();
        let request = request.context(error::RequestSnafu)?;
        let url = request.url().clone();
        let origin = url.origin();
        let retry = request.try_clone();
        let response = self
            .auth(RequestBuilder::from_parts(client.clone(), request), &origin)
            .send()
            .await
            .context(error::RequestSnafu)?;
        // Only the addressed origin may challenge: a host reached by redirect
        // (a blob CDN, say) must never be handed the registry's credentials.
        // A configured bearer token has no exchange, so its 401 stands.
        if response.status() != StatusCode::UNAUTHORIZED
            || response.url().origin() != origin
            || matches!(self.auth, Some(Token::Bearer(_)))
        {
            return Ok(response);
        }
        let challenge = response
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(BearerChallenge::parse);
        let (Some(retry), Some(challenge)) = (retry, challenge) else {
            return Ok(response);
        };
        let token = self.fetch_token(&url, &challenge).await?;
        let response = RequestBuilder::from_parts(client, retry)
            .bearer_auth(&token)
            .send()
            .await
            .context(error::RequestSnafu)?;
        // Cache only an accepted token, so a useless one can't replace a good one.
        if response.status() != StatusCode::UNAUTHORIZED {
            self.tokens().insert(origin, Token::Bearer(token));
        }
        Ok(response)
    }

    /// Get a token from the challenge's realm, presenting basic credentials
    /// when configured (anonymous otherwise).
    async fn fetch_token(&self, registry: &Url, challenge: &BearerChallenge) -> Result<String> {
        let fail = |reason: String| error::Error::AuthToken {
            realm: challenge.realm.clone(),
            reason,
        };
        let mut realm = Url::parse(&challenge.realm).map_err(|e| fail(e.to_string()))?;
        if !realm_allowed(&realm, registry) {
            return Err(fail("realm must use https".to_string()));
        }
        if challenge.service.is_some() || challenge.scope.is_some() {
            let mut query = realm.query_pairs_mut();
            if let Some(service) = &challenge.service {
                query.append_pair("service", service);
            }
            for scope in challenge.scope.iter().flat_map(|s| s.split_whitespace()) {
                query.append_pair("scope", scope);
            }
        }
        let mut request = self.client.get(realm);
        if let Some(Token::Basic { username, password }) = &self.auth {
            request = request.basic_auth(username, Some(password));
        }
        let mut response = request.send().await.context(error::RequestSnafu)?;
        if !response.status().is_success() {
            return Err(fail(format!("HTTP {}", response.status())));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.context(error::RequestSnafu)? {
            if body.len() + chunk.len() > MAX_TOKEN_RESPONSE {
                return Err(fail("response too large".to_string()));
            }
            body.extend_from_slice(&chunk);
        }
        let body: TokenResponse = serde_json::from_slice(&body).map_err(|e| fail(e.to_string()))?;
        body.token
            .or(body.access_token)
            .ok_or_else(|| fail("response has no token".to_string()))
    }
}

/// Credentials are sent to the realm, which the registry chooses; refuse plain
/// http unless the registry itself is addressed over http.
fn realm_allowed(realm: &Url, registry: &Url) -> bool {
    realm.scheme() == "https" || registry.scheme() == "http"
}

/// Token endpoint response; registries use either field name.
#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// A `WWW-Authenticate: Bearer ...` challenge.
#[derive(Debug, PartialEq, Eq)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl BearerChallenge {
    /// Parse `Bearer realm="...",service="...",scope="..."`. Quoted values may
    /// contain commas (`scope="repository:a:pull,push"`) and escaped quotes.
    fn parse(header: &str) -> Option<Self> {
        let (scheme, mut rest) = header.trim().split_once(char::is_whitespace)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let (mut realm, mut service, mut scope) = (None, None, None);
        loop {
            rest = rest.trim_start_matches([' ', '\t', ',']);
            if rest.is_empty() {
                break;
            }
            let (key, after) = rest.split_once('=')?;
            let after = after.trim_start();
            let (value, remaining) = match after.strip_prefix('"') {
                Some(quoted) => {
                    let mut value = String::new();
                    let mut chars = quoted.char_indices();
                    let end = loop {
                        match chars.next()? {
                            (_, '\\') => value.push(chars.next()?.1),
                            (i, '"') => break i + 1,
                            (_, c) => value.push(c),
                        }
                    };
                    // Skip anything between the closing quote and the next comma.
                    let after_quote = &quoted[end..];
                    let next = after_quote.find(',').unwrap_or(after_quote.len());
                    (value, &after_quote[next..])
                }
                None => {
                    let end = after.find(',').unwrap_or(after.len());
                    (after[..end].trim().to_string(), &after[end..])
                }
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "realm" => realm = Some(value),
                "service" => service = Some(value),
                "scope" => scope = Some(value),
                _ => {}
            }
            rest = remaining;
        }
        Some(Self {
            realm: realm?,
            service,
            scope,
        })
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
        self.send(request).await
    }

    async fn head_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self
            .client
            .head(v2_url(uri, repository.split('/').chain(["blobs", digest]))?);
        self.send(request).await
    }

    async fn get_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self
            .client
            .get(v2_url(uri, repository.split('/').chain(["blobs", digest]))?);
        self.send(request).await
    }

    async fn del_blob(&self, uri: &Url, repository: &str, digest: &str) -> Result<Response> {
        let request = self
            .client
            .delete(v2_url(uri, repository.split('/').chain(["blobs", digest]))?);
        self.send(request).await
    }

    async fn get_tags(&self, uri: &Url, repository: &str) -> Result<Response> {
        let request = self
            .client
            .get(v2_url(uri, repository.split('/').chain(["tags", "list"]))?);
        self.send(request).await
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
        self.send(
            request
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", data.len())
                .body(data),
        )
        .await
    }

    async fn start_upload(&self, uri: &Url, repository: &str) -> Result<Response> {
        let request = self.client.post(v2_url(
            uri,
            repository.split('/').chain(["blobs", "uploads", ""]),
        )?);
        self.send(request.header("Content-Length", 0)).await
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
        self.send(
            request
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", data.len())
                .header("Content-Range", format!("{}-{}", start, last))
                .body(data),
        )
        .await
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
            request.header("Content-Length", 0)
        } else {
            // Inclusive end byte per RFC 7233 / OCI distribution spec.
            let last = end.saturating_sub(1);
            request
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", data.len())
                .header("Content-Range", format!("{}-{}", start, last))
                .body(data)
        };
        self.send(request).await
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
        self.send(request.header("Accept", MANIFEST_ACCEPT)).await
    }

    async fn get_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response> {
        let request = self.client.get(v2_url(
            uri,
            repository.split('/').chain(["manifests", reference]),
        )?);
        self.send(request.header("Accept", MANIFEST_ACCEPT)).await
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
        self.send(
            request
                .header("Content-Type", content_type)
                .header("Content-Length", body.len())
                .body(body),
        )
        .await
    }

    async fn del_manifest(&self, uri: &Url, repository: &str, reference: &str) -> Result<Response> {
        let request = self.client.delete(v2_url(
            uri,
            repository.split('/').chain(["manifests", reference]),
        )?);
        self.send(request).await
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

    #[test]
    fn parses_bearer_challenges() {
        let parsed = BearerChallenge::parse(concat!(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io","#,
            r#"scope="repository:library/alpine:pull,push""#,
        ));
        assert_eq!(
            parsed,
            Some(BearerChallenge {
                realm: "https://auth.docker.io/token".into(),
                service: Some("registry.docker.io".into()),
                scope: Some("repository:library/alpine:pull,push".into()),
            })
        );
        // Case-insensitive scheme, spaced separators, bare and escaped values.
        let parsed =
            BearerChallenge::parse(r#"bearer realm=https://r.example/t , service="a \"b\"""#)
                .unwrap();
        assert_eq!(parsed.realm, "https://r.example/t");
        assert_eq!(parsed.service.as_deref(), Some(r#"a "b""#));
        assert_eq!(parsed.scope, None);
        assert_eq!(BearerChallenge::parse(r#"Basic realm="registry""#), None);
        assert_eq!(BearerChallenge::parse(r#"Bearer service="no-realm""#), None);
        assert_eq!(
            BearerChallenge::parse(r#"Bearer realm="unterminated"#),
            None
        );
        // Junk after a quoted value doesn't swallow the next parameter.
        let parsed = BearerChallenge::parse(r#"Bearer realm="a"x,service="s""#).unwrap();
        assert_eq!(parsed.service.as_deref(), Some("s"));
    }

    #[test]
    fn realm_must_be_https_for_https_registries() {
        let https = Url::parse("https://registry.example").unwrap();
        let http = Url::parse("http://localhost:5000").unwrap();
        let tls_realm = Url::parse("https://auth.example/token").unwrap();
        let plain_realm = Url::parse("http://auth.example/token").unwrap();
        assert!(realm_allowed(&tls_realm, &https));
        assert!(!realm_allowed(&plain_realm, &https));
        assert!(realm_allowed(&plain_realm, &http));
    }

    mod challenge {
        use super::*;
        use wiremock::matchers::{body_string, header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const MANIFEST: &str = "/v2/org/app/manifests/latest";

        /// 401 for anything not carrying the issued token, pointing at `/token`.
        async fn mount_challenge(server: &MockServer) {
            let challenge = format!(
                r#"Bearer realm="{}/token",service="test",scope="repository:org/app:pull""#,
                server.uri()
            );
            Mock::given(path(MANIFEST))
                .respond_with(
                    ResponseTemplate::new(401)
                        .insert_header("WWW-Authenticate", challenge.as_str()),
                )
                .with_priority(10)
                .mount(server)
                .await;
        }

        /// Token endpoint, expected to be hit exactly once (checked on drop).
        async fn mount_token(server: &MockServer, basic: Option<&str>) {
            let mut mock = Mock::given(method("GET"))
                .and(path("/token"))
                .and(query_param("service", "test"))
                .and(query_param("scope", "repository:org/app:pull"));
            if let Some(basic) = basic {
                mock = mock.and(header("authorization", basic));
            }
            mock.respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"issued"}"#))
                .expect(1)
                .mount(server)
                .await;
        }

        async fn mount_authorized_get(server: &MockServer) {
            Mock::given(method("GET"))
                .and(path(MANIFEST))
                .and(header("authorization", "Bearer issued"))
                .respond_with(ResponseTemplate::new(200))
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn answers_challenge_and_reuses_token() {
            let server = MockServer::start().await;
            mount_challenge(&server).await;
            mount_token(&server, None).await;
            mount_authorized_get(&server).await;
            let base = Url::parse(&server.uri()).unwrap();
            let client = SimpleRegistryClient::new(None);
            for _ in 0..2 {
                let response = client
                    .get_manifest(&base, "org/app", "latest")
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
            }
        }

        #[tokio::test]
        async fn exchanges_basic_credentials_for_token() {
            let server = MockServer::start().await;
            mount_challenge(&server).await;
            // base64("user:pass")
            mount_token(&server, Some("Basic dXNlcjpwYXNz")).await;
            mount_authorized_get(&server).await;
            let base = Url::parse(&server.uri()).unwrap();
            let client = SimpleRegistryClient::new(Some(Token::Basic {
                username: "user".into(),
                password: "pass".into(),
            }));
            let response = client
                .get_manifest(&base, "org/app", "latest")
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn retries_with_request_body() {
            let server = MockServer::start().await;
            mount_challenge(&server).await;
            mount_token(&server, None).await;
            Mock::given(method("PUT"))
                .and(path(MANIFEST))
                .and(header("authorization", "Bearer issued"))
                .and(body_string("manifest"))
                .respond_with(ResponseTemplate::new(201))
                .mount(&server)
                .await;
            let base = Url::parse(&server.uri()).unwrap();
            let client = SimpleRegistryClient::new(None);
            let body = Bytes::from_static(b"manifest");
            let response = client
                .put_manifest(&base, "org/app", "latest", "application/json", body)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
        }

        #[tokio::test]
        async fn passes_through_401_without_bearer_challenge() {
            let server = MockServer::start().await;
            Mock::given(path(MANIFEST))
                .respond_with(
                    ResponseTemplate::new(401)
                        .insert_header("WWW-Authenticate", r#"Basic realm="x""#),
                )
                .expect(1)
                .mount(&server)
                .await;
            let base = Url::parse(&server.uri()).unwrap();
            let client = SimpleRegistryClient::new(None);
            let response = client
                .get_manifest(&base, "org/app", "latest")
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn accepts_access_token_field() {
            let server = MockServer::start().await;
            mount_challenge(&server).await;
            Mock::given(path("/token"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(r#"{"access_token":"issued"}"#),
                )
                .mount(&server)
                .await;
            mount_authorized_get(&server).await;
            let base = Url::parse(&server.uri()).unwrap();
            let client = SimpleRegistryClient::new(None);
            let response = client
                .get_manifest(&base, "org/app", "latest")
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn ignores_challenge_from_redirect_target() {
            let registry = MockServer::start().await;
            let cdn = MockServer::start().await;
            Mock::given(path(MANIFEST))
                .respond_with(
                    ResponseTemplate::new(307)
                        .insert_header("Location", format!("{}/blob", cdn.uri()).as_str()),
                )
                .mount(&registry)
                .await;
            let challenge = format!(r#"Bearer realm="{}/token""#, cdn.uri());
            Mock::given(path("/blob"))
                .respond_with(
                    ResponseTemplate::new(401)
                        .insert_header("WWW-Authenticate", challenge.as_str()),
                )
                .mount(&cdn)
                .await;
            Mock::given(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"t"}"#))
                .expect(0)
                .mount(&cdn)
                .await;
            let base = Url::parse(&registry.uri()).unwrap();
            let client = SimpleRegistryClient::new(Some(Token::Basic {
                username: "user".into(),
                password: "pass".into(),
            }));
            let response = client
                .get_manifest(&base, "org/app", "latest")
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn issued_token_stays_with_its_origin() {
            let server = MockServer::start().await;
            let storage = MockServer::start().await;
            mount_challenge(&server).await;
            mount_token(&server, None).await;
            mount_authorized_get(&server).await;
            Mock::given(header("authorization", "Bearer issued"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&storage)
                .await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(202))
                .mount(&storage)
                .await;
            let base = Url::parse(&server.uri()).unwrap();
            let client = SimpleRegistryClient::new(None);
            client
                .get_manifest(&base, "org/app", "latest")
                .await
                .unwrap();
            let upload = Url::parse(&format!("{}/upload", storage.uri())).unwrap();
            let response = client
                .upload_part(&upload, Bytes::from_static(b"data"), 0, 4)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }

        #[tokio::test]
        async fn configured_bearer_token_is_not_replaced() {
            let server = MockServer::start().await;
            mount_challenge(&server).await;
            Mock::given(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"anon"}"#))
                .expect(0)
                .mount(&server)
                .await;
            Mock::given(path("/v2/other/manifests/latest"))
                .and(header("authorization", "Bearer configured"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
            let base = Url::parse(&server.uri()).unwrap();
            let client = SimpleRegistryClient::new(Some(Token::Bearer("configured".into())));
            let response = client
                .get_manifest(&base, "org/app", "latest")
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = client.get_manifest(&base, "other", "latest").await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }
}

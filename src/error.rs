use std::num::ParseIntError;

use reqwest::header::ToStrError;
use snafu::Snafu;
use tokio::task::JoinError;
use url::Url;

use crate::index::IndexBuilderError;
use crate::layer::LayerBuilderError;
use crate::models::{ErrorResponse, Platform, TarballManifestBuilderError};
use crate::uri::{Uri, UriBuilderError};

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("failed to interact with tar archive: {source}"))]
    Archive { source: std::io::Error },
    #[snafu(display("failed to authorize with oci registry: {reason}"))]
    Authorization { reason: String },
    #[snafu(display("blob with digest {digest} is missing from oci archive"))]
    BlobMissing { digest: String },
    #[snafu(display(
        "failed to deserialize image configuration received from registry: {source}"
    ))]
    ConfigDeserialize { source: serde_json::Error },
    #[snafu(display("oci registry did not return the content length"))]
    ContentLengthMissing,
    #[snafu(display("content-length was not a valid number: {source}"))]
    ContentLengthNotNumber { source: ParseIntError },
    #[snafu(display("oci registry did not return a proper header"))]
    ImproperHeader { source: ToStrError },
    #[snafu(display("failed to build image index: {source}"))]
    Index { source: IndexBuilderError },
    #[snafu(display("failed to deserialize response body: {source}"))]
    BodyDeserialize { source: serde_json::Error },
    #[snafu(display("failed to delete blob '{digest}': {reason}"))]
    DeleteBlob {
        digest: String,
        reason: ErrorResponse,
    },
    #[snafu(display("cannot delete a blob without a specific digest"))]
    DeleteBlobNoDigest,
    #[snafu(display("failed to delete tag '{tag}': {reason}"))]
    DeleteTag { tag: String, reason: ErrorResponse },
    #[snafu(display("cannot delete a tag via a sha256 digest"))]
    DeleteTagDigest,
    #[snafu(display("failed to perform operation with directory: {source}"))]
    Directory { source: std::io::Error },
    #[snafu(display("cannot read a blob without a specific digest uri (uri: {uri})"))]
    DirectLoadBlob { uri: Uri },
    #[snafu(display("cannot direct load an image without a specific digest uri (uri: {uri})"))]
    DirectLoadImage { uri: Uri },
    #[snafu(display("failed to deserialize error response from oci registry: {source}"))]
    ErrorDeserialize { source: reqwest::Error },
    #[snafu(display("failed to fetch blob: {reason}"))]
    FetchBlob { reason: ErrorResponse },
    #[snafu(display("failed to fetch index: {reason}"))]
    FetchIndex { reason: ErrorResponse },
    #[snafu(display("failed to fetch manifest: {reason}"))]
    FetchManifest { reason: ErrorResponse },
    #[snafu(display("failed to interact with local file: {source}"))]
    File { source: std::io::Error },
    #[snafu(display("failed to finish blob upload: {reason}"))]
    FinishBlob { reason: ErrorResponse },
    #[snafu(display("oci image archive has invalid index: {source}"))]
    ImageInvalidIndex { source: serde_json::Error },
    #[snafu(display("oci image archive does not have a valid manifest: {source}"))]
    ImageInvalidManifest { source: serde_json::Error },
    #[snafu(display("index does not contain an image for the platform: {platform}"))]
    IndexNoPlatform { platform: Platform },
    #[snafu(display("no image was found in oci registry matching: {uri}"))]
    ImageNotFound { uri: Uri },
    #[snafu(display("file is not a valid oci archive as it is missing index.json"))]
    ImageNotValid,
    #[snafu(display("invalid algorithm in digest: {algorithm}"))]
    InvalidAlgorithm { algorithm: String },
    #[snafu(display("invalid layer definition: {source}"))]
    Layer { source: LayerBuilderError },
    #[snafu(display("failed to unpack archive from layer: {source}"))]
    LayerArchive { source: std::io::Error },
    #[snafu(display("failed to copy from layer: {source}"))]
    LayerCopy { source: std::io::Error },
    #[snafu(display("failed to wait for layer operation: {source}"))]
    LayerWait { source: JoinError },
    #[snafu(display("failed to read from layer: {source}"))]
    LayerRead { source: std::io::Error },
    #[snafu(display("failed to write layer: {source}"))]
    LayerWrite { source: std::io::Error },
    #[snafu(display("failed to list repositories in registry: {reason}"))]
    ListRepos { reason: ErrorResponse },
    #[snafu(display("failed to list tags in repository: {reason}"))]
    ListTags { reason: ErrorResponse },
    #[snafu(display("malformed object uri provided: {reason}"))]
    MalformedUri { reason: String },
    #[snafu(display("no image index found at uri: {uri}"))]
    NoIndex { uri: Uri },
    #[snafu(display("failed to push image to '{uri}': {reason}"))]
    PushImage { uri: Url, reason: ErrorResponse },
    #[snafu(display("failed to make request to oci registry: {source}"))]
    Request { source: reqwest::Error },
    #[snafu(display("failed to parse response from oci registry: {source}"))]
    ResponseDeserialize { source: reqwest::Error },
    #[snafu(display("failed to serialize to json: {source}"))]
    Serialize { source: serde_json::Error },
    #[snafu(display("failed to start a blob upload: {reason}"))]
    StartBlobUpload { reason: ErrorResponse },
    #[snafu(display("registry did not provide an upload_url for blob upload"))]
    StartBlobNoLocation,
    #[snafu(display("failed to construct manifest for tarball export: {source}"))]
    TarballManifest { source: TarballManifestBuilderError },
    #[snafu(display("failed to create temporary directory: {source}"))]
    Temp { source: std::io::Error },
    #[snafu(display("upload of chunk for blob failed: {reason}"))]
    Upload { reason: ErrorResponse },
    #[snafu(display("invalid url detected: {source}"))]
    Url { source: url::ParseError },
    #[snafu(display("invalid object uri: {source}"))]
    Uri { source: UriBuilderError },
}

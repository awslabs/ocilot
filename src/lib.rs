#[macro_use]
extern crate tracing;

pub(crate) mod client;
/// Layer decompression utilities.
#[cfg(feature = "compression")]
pub mod compression;
/// Error types for the crate.
pub mod error;
/// Image manifest handling.
pub mod image;
/// Image index operations.
pub mod index;
/// Layer read/write operations.
pub mod layer;
/// OCI specification model types.
pub mod models;
/// Registry client and operations.
pub mod registry;
/// Repository operations.
pub mod repository;
/// URI parsing and representation.
pub mod uri;

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, error::Error>;

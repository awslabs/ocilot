use derive_builder::Builder;
use snafu::{OptionExt, ResultExt};
use std::fmt;
use std::fmt::Formatter;
use std::str::FromStr;
use url::Url;

use crate::error;
use crate::registry::Registry;

/// Represents a reference to an object in an oci
/// container.
#[derive(Debug, Clone, Builder)]
#[builder(setter(into))]
pub struct Uri {
    /// Registry this object is stored in
    registry: Registry,
    /// Repository this object is categorized under
    repository: String,
    /// Reference to the object usually a tag ':tag' or digest '@digest'
    reference: Reference,
}

/// Uri to a specific registry
#[derive(Debug, Clone, Builder)]
#[builder(setter(into))]
pub struct RegistryUri {
    /// Registry url
    base: String,
    /// Whether to connect with https or not
    is_secure: bool,
}

impl RegistryUri {
    pub fn base(&self) -> &String {
        &self.base
    }

    pub fn is_secure(&self) -> bool {
        self.is_secure
    }

    pub fn set_secure(&mut self, flag: bool) {
        self.is_secure = flag;
    }
}

impl FromStr for RegistryUri {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (registry_base, is_secure) = if s.starts_with("http://") {
            (s.strip_prefix("http://").unwrap(), false)
        } else if s.starts_with("https://") {
            (s.strip_prefix("https://").unwrap(), true)
        } else {
            (s, !(s.contains("localhost") || s.contains("127.0.0.1")))
        };
        Ok(Self {
            base: registry_base.to_string(),
            is_secure,
        })
    }
}

impl TryInto<Url> for RegistryUri {
    type Error = crate::error::Error;

    fn try_into(self) -> Result<Url, Self::Error> {
        Url::parse(&format!(
            "{}://{}",
            if self.is_secure { "https" } else { "http" },
            self.base
        ))
        .context(crate::error::UrlSnafu)
    }
}

impl Uri {
    /// Parse an object uri from a string and initialize a registry client
    pub async fn new(input: &str) -> crate::Result<Self> {
        let (registry, object) = input.split_once("/").context(error::MalformedUriSnafu {
            reason: "only a registry was provided in the uri",
        })?;
        let (repository, tag) = if object.contains('@') {
            let (repository, digest) = object.split_once('@').unwrap();
            let (algorithm, value) = digest.split_once(':').context(error::MalformedUriSnafu {
                reason: "no algorithm was provided for the digest",
            })?;
            (
                repository,
                Reference::Digest {
                    algorithm: Algorithm::from_str(algorithm)?,
                    value: value.to_string(),
                },
            )
        } else {
            let (repository, tag) = object.split_once(':').context(error::MalformedUriSnafu {
                reason: "no tag was provided for the object",
            })?;
            (repository, Reference::Tag(tag.to_string()))
        };
        Ok(Self {
            registry: Registry::new(&RegistryUri::from_str(registry)?).await?,
            repository: repository.into(),
            reference: tag,
        })
    }

    pub fn set_secure(&mut self, flag: bool) {
        self.registry.set_secure(flag);
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn repository(&self) -> &String {
        &self.repository
    }

    pub fn reference(&self) -> &Reference {
        &self.reference
    }
}

impl fmt::Display for Uri {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "{}/{}{}",
            self.registry.uri().base,
            self.repository,
            match &self.reference {
                Reference::Tag(tag) => format!(":{tag}"),
                Reference::Digest { algorithm, value } => format!("@{algorithm}:{value}"),
            }
        ))
    }
}

/// Represents a reference to a specific object via a tag or digest
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest { algorithm: Algorithm, value: String },
}

impl FromStr for Reference {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains(':') {
            let (algorithm, value) = s.split_once(':').unwrap();
            Ok(Self::Digest {
                algorithm: Algorithm::from_str(algorithm)?,
                value: value.to_string(),
            })
        } else {
            Ok(Self::Tag(s.to_string()))
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Algorithm {
    #[default]
    Sha256,
    Sha512,
}

impl FromStr for Algorithm {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha256" => Ok(Self::Sha256),
            "sha512" => Ok(Self::Sha512),
            _ => crate::error::InvalidAlgorithmSnafu {
                algorithm: s.to_string(),
            }
            .fail(),
        }
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256 => f.write_str("sha256"),
            Self::Sha512 => f.write_str("sha512"),
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag(tag) => f.write_str(tag),
            Self::Digest { algorithm, value } => {
                f.write_fmt(format_args!("{}:{}", algorithm, value))
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    #[test]
    fn test_registry_from_str() {
        let registry = super::RegistryUri::from_str("localhost:5000").unwrap();
        assert_eq!(registry.base, "localhost:5000");
        assert!(!registry.is_secure);
        let registry = super::RegistryUri::from_str("127.0.0.1").unwrap();
        assert_eq!(registry.base, "127.0.0.1");
        assert!(!registry.is_secure);
        let registry = super::RegistryUri::from_str("public.ecr.aws/bottlerocket").unwrap();
        assert_eq!(registry.base, "public.ecr.aws/bottlerocket");
        assert!(registry.is_secure);
        let registry = super::RegistryUri::from_str("http://public.ecr.aws").unwrap();
        assert_eq!(registry.base, "public.ecr.aws");
        assert!(!registry.is_secure);
        let registry = super::RegistryUri::from_str("https://XXXXXXXXXXXXXXXXXXXXXX").unwrap();
        assert_eq!(registry.base, "XXXXXXXXXXXXXXXXXXXXXX");
        assert!(registry.is_secure);
    }

    #[test]
    fn test_registry_into_url() {
        let registry = super::RegistryUri::from_str("localhost:5000").unwrap();
        let url: super::Url = registry.try_into().unwrap();
        assert_eq!(url.as_str(), "http://localhost:5000/");
        let registry = super::RegistryUri::from_str("public.ecr.aws/bottlerocket").unwrap();
        let url: super::Url = registry.try_into().unwrap();
        assert_eq!(url.as_str(), "https://public.ecr.aws/bottlerocket");
    }

    #[test]
    fn test_algorithm_from_str() {
        let algorithm = super::Algorithm::from_str("sha256").unwrap();
        assert_eq!(algorithm, super::Algorithm::Sha256);
        let algorithm = super::Algorithm::from_str("sha512").unwrap();
        assert_eq!(algorithm, super::Algorithm::Sha512);
    }

    #[test]
    fn test_reference_from_str() {
        let reference = super::Reference::from_str("latest").unwrap();
        assert_eq!(reference, super::Reference::Tag("latest".to_string()));
        let reference = super::Reference::from_str("sha256:1234567890abcdef").unwrap();
        assert_eq!(
            reference,
            super::Reference::Digest {
                algorithm: super::Algorithm::Sha256,
                value: "1234567890abcdef".to_string(),
            }
        );
    }

    #[test]
    fn test_reference_to_string() {
        let reference = super::Reference::Tag("latest".to_string());
        assert_eq!(reference.to_string(), "latest");
        let reference = super::Reference::Digest {
            algorithm: super::Algorithm::Sha256,
            value: "1234567890abcdef".to_string(),
        };
        assert_eq!(reference.to_string(), "sha256:1234567890abcdef");
    }

    #[tokio::test]
    async fn test_uri_new() {
        let uri = super::Uri::new("localhost:5000/bottlerocket-test:latest")
            .await
            .unwrap();
        assert_eq!(uri.registry.uri().base, "localhost:5000");
        assert_eq!(uri.repository, "bottlerocket-test");
        assert_eq!(uri.reference, super::Reference::Tag("latest".to_string()));
        assert_eq!(uri.to_string(), "localhost:5000/bottlerocket-test:latest");
        let uri = super::Uri::new("fake.io/bottlerocket/bottlerocket-test:latest")
            .await
            .unwrap();
        assert_eq!(uri.registry.uri().base, "fake.io");
        assert_eq!(uri.repository, "bottlerocket/bottlerocket-test");
        assert_eq!(uri.reference, super::Reference::Tag("latest".to_string()));
        assert_eq!(
            uri.to_string(),
            "fake.io/bottlerocket/bottlerocket-test:latest"
        );
        let uri = super::Uri::new("fake.io/bottlerocket/bottlerocket-test@sha256:1234567890abcdef")
            .await
            .unwrap();
        assert_eq!(uri.registry.uri().base, "fake.io");
        assert_eq!(uri.repository, "bottlerocket/bottlerocket-test");
        assert_eq!(
            uri.reference,
            super::Reference::Digest {
                algorithm: super::Algorithm::Sha256,
                value: "1234567890abcdef".to_string(),
            }
        );
        assert_eq!(
            uri.to_string(),
            "fake.io/bottlerocket/bottlerocket-test@sha256:1234567890abcdef"
        );
    }
}

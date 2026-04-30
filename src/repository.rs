use crate::registry::Registry;

/// Represents a single repository in a registry.
///
/// Handles all repository-wide operations.
pub struct Repository {
    registry: Registry,
    name: String,
}

impl Repository {
    /// Create a handler to a given repository in a registry.
    pub fn new(registry: &Registry, name: &str) -> Self {
        Self {
            registry: registry.clone(),
            name: name.to_string(),
        }
    }

    /// The registry this repository belongs to.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The name of this repository.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// List all the tags in this repository.
    pub async fn tags(&self) -> crate::Result<Vec<String>> {
        self.registry.get_tags(self.name.as_str()).await
    }

    /// Delete a tag in this repository.
    pub async fn delete_tag(&self, tag: &str) -> crate::Result<()> {
        self.registry.delete_tag(&self.name, tag).await
    }
}

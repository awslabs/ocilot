use crate::registry::Registry;

/// Represents a single repository in a registry and handles
/// all repository wide operations
pub struct Repository {
    pub registry: Registry,
    pub name: String,
}

impl Repository {
    /// Create a handler to a given repository in a registry
    pub fn new(registry: &Registry, name: &str) -> Self {
        Self {
            registry: registry.clone(),
            name: name.to_string(),
        }
    }

    /// List all the tags in this repository
    pub async fn tags(&self) -> crate::Result<Vec<String>> {
        self.registry.get_tags(self.name.as_str()).await
    }

    /// Delete a tag in this repository
    pub async fn delete_tag(&self, tag: &str) -> crate::Result<()> {
        self.registry.delete_tag(&self.name, tag).await
    }
}

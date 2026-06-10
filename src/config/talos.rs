//! Talos authentication configuration.

use serde_derive::{Deserialize, Serialize};

use crate::auth::talos::validate_talos_pub_keys;
use crate::errors::Error;

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug, Hash, Eq, Default)]
pub struct Talos {
    pub keys: Vec<String>,
    pub databases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_prefixes: Vec<String>,
}

impl Talos {
    pub async fn validate(&mut self) -> Result<(), Error> {
        let enabled = !self.keys.is_empty()
            || !self.databases.is_empty()
            || !self.resource_prefixes.is_empty();
        if enabled && self.keys.is_empty() {
            return Err(Error::BadConfig(
                "talos.keys must not be empty when Talos is configured".to_string(),
            ));
        }
        if enabled && self.databases.is_empty() {
            return Err(Error::BadConfig(
                "talos.databases must not be empty when Talos is configured".to_string(),
            ));
        }
        if enabled && self.resource_prefixes.is_empty() {
            return Err(Error::BadConfig(
                "talos.resource_prefixes must not be empty when Talos is configured".to_string(),
            ));
        }
        for database in &self.databases {
            if database.trim().is_empty() {
                return Err(Error::BadConfig(
                    "talos.databases must not contain empty database names".to_string(),
                ));
            }
            if database.as_bytes().contains(&b'\0') {
                return Err(Error::BadConfig(
                    "talos.databases must not contain null bytes".to_string(),
                ));
            }
        }
        for prefix in &self.resource_prefixes {
            if prefix.trim().is_empty() {
                return Err(Error::BadConfig(
                    "talos.resource_prefixes must not contain empty prefixes".to_string(),
                ));
            }
            if prefix.contains(':') {
                return Err(Error::BadConfig(
                    "talos.resource_prefixes entries must not contain ':'".to_string(),
                ));
            }
            if prefix.as_bytes().contains(&b'\0') {
                return Err(Error::BadConfig(
                    "talos.resource_prefixes must not contain null bytes".to_string(),
                ));
            }
        }
        validate_talos_pub_keys(&self.keys)?;
        Ok(())
    }
    pub fn empty() -> Self {
        Talos {
            keys: vec![],
            databases: vec![],
            resource_prefixes: vec![],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.databases.is_empty() && self.resource_prefixes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validate_requires_resource_prefixes_when_talos_is_configured() {
        let mut talos = Talos {
            keys: vec!["/tmp/missing-key.pem".to_string()],
            databases: vec!["billing".to_string()],
            resource_prefixes: vec![],
        };

        let err = talos.validate().await.unwrap_err();

        assert!(
            err.to_string().contains("talos.resource_prefixes"),
            "unexpected validation error: {err}"
        );
    }
}

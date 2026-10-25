use scion_types::{Error, Result};
use std::time::Duration;

/// Connection and resource limits. Credentials come from the official SDK's
/// Application Default Credentials provider, never from this configuration.
#[derive(Clone, Debug)]
pub struct SpannerConfig {
    /// `projects/PROJECT/instances/INSTANCE/databases/DATABASE`.
    pub database: String,
    /// Optional Spanner fine-grained access-control database role.
    pub database_role: Option<String>,
    /// Deadline for a complete operation, including transaction retries.
    pub operation_timeout: Duration,
    /// Maximum outstanding operations; excess calls return a resource error.
    pub max_inflight: usize,
}

impl SpannerConfig {
    pub fn new(database: impl Into<String>) -> Self {
        Self {
            database: database.into(),
            database_role: None,
            operation_timeout: Duration::from_secs(60),
            max_inflight: 64,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let parts: Vec<_> = self.database.split('/').collect();
        if parts.len() != 6
            || parts[0] != "projects"
            || parts[2] != "instances"
            || parts[4] != "databases"
            || [parts[1], parts[3], parts[5]].iter().any(|part| {
                part.is_empty()
                    || part.len() > 128
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_:.".contains(&byte))
            })
        {
            return Err(Error::Invalid(
                "invalid Spanner database resource name".into(),
            ));
        }
        if self.database_role.as_ref().is_some_and(|role| {
            role.is_empty()
                || role.len() > 128
                || !role
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
            return Err(Error::Invalid("invalid Spanner database role".into()));
        }
        if !(1..=1024).contains(&self.max_inflight)
            || self.operation_timeout.is_zero()
            || self.operation_timeout > Duration::from_secs(300)
        {
            return Err(Error::Invalid("invalid Spanner operation limits".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_names_and_bounds_are_checked_without_network_access() {
        SpannerConfig::new("projects/test/instances/test/databases/scion")
            .validate()
            .unwrap();
        for database in [
            "",
            "https://example.com/db",
            "projects/p/instances/i/databases/",
            "projects/p/instances/i/databases/d/extra",
        ] {
            assert!(SpannerConfig::new(database).validate().is_err());
        }
        let mut config = SpannerConfig::new("projects/p/instances/i/databases/d");
        config.max_inflight = 0;
        assert!(config.validate().is_err());
        config.max_inflight = 1;
        config.operation_timeout = Duration::from_secs(301);
        assert!(config.validate().is_err());
    }
}

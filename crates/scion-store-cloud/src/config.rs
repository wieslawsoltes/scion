use crate::CloudLimits;
use object_store::ObjectStore;
use scion_types::{Error, Result};
use std::{str::FromStr, sync::Arc};

/// Cloud provider. Credentials are supplied through the provider SDK's chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudProvider {
    S3,
    Gcs,
    Azure,
}

impl FromStr for CloudProvider {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "s3" => Ok(Self::S3),
            "gcs" | "gs" => Ok(Self::Gcs),
            "azure" => Ok(Self::Azure),
            _ => Err(Error::Invalid(
                "cloud provider must be s3, gcs, or azure".into(),
            )),
        }
    }
}

/// Non-secret deployment configuration. Secret values belong to SDK credential
/// providers, not a serialized Scion configuration or repository record.
#[derive(Debug, Clone)]
pub struct CloudConfig {
    pub provider: CloudProvider,
    /// S3/GCS bucket or Azure container, which must already exist.
    pub bucket: String,
    /// Persistent object namespace, independent of repository authorization.
    pub prefix: String,
    /// Explicit region overrides AWS_REGION/AWS_DEFAULT_REGION when present.
    pub region: Option<String>,
    /// Explicit Azure account overrides the provider environment when present.
    pub account: Option<String>,
    /// Optional S3-compatible, GCS, or Azure endpoint; credentials are forbidden
    /// in its user-info, query, or fragment components.
    pub endpoint: Option<String>,
    /// Permit plaintext HTTP for an explicitly configured private endpoint.
    pub allow_http: bool,
    /// S3 path-style requests are the default; enable for a compatible endpoint.
    pub virtual_hosted_style: bool,
    /// Skip GCS/Azure signing only for an explicitly configured loopback emulator.
    pub anonymous_emulator: bool,
    pub limits: CloudLimits,
}

impl CloudConfig {
    pub fn new(provider: CloudProvider, bucket: impl Into<String>) -> Self {
        Self {
            provider,
            bucket: bucket.into(),
            prefix: "scion".into(),
            region: None,
            account: None,
            endpoint: None,
            allow_http: false,
            virtual_hosted_style: false,
            anonymous_emulator: false,
            limits: CloudLimits::default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        validate_prefix(&self.prefix)?;
        if self.bucket.is_empty()
            || self.bucket.len() > 255
            || !self
                .bucket
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(Error::Invalid("invalid cloud bucket/container name".into()));
        }
        let endpoint = self.endpoint.as_ref().map(|endpoint| {
            let url = url::Url::parse(endpoint)
                .map_err(|_| Error::Invalid("invalid cloud endpoint URL".into()))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::Invalid("cloud endpoint must be an HTTP(S) URL without credentials, query, or fragment".into()));
            }
            if url.scheme() == "http" && !self.allow_http {
                return Err(Error::Invalid("plaintext cloud endpoint requires allow_http".into()));
            }
            Ok(url)
        }).transpose()?;
        if self.allow_http && endpoint.is_none() {
            return Err(Error::Invalid(
                "allow_http requires an explicit cloud endpoint".into(),
            ));
        }
        if self.anonymous_emulator {
            let loopback = endpoint
                .as_ref()
                .and_then(|url| url.host())
                .is_some_and(|host| match host {
                    url::Host::Ipv4(address) => address.is_loopback(),
                    url::Host::Ipv6(address) => address.is_loopback(),
                    url::Host::Domain(name) => name == "localhost",
                });
            if !loopback || self.provider == CloudProvider::S3 {
                return Err(Error::Invalid(
                    "anonymous emulation requires a loopback GCS/Azure endpoint".into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn build(&self) -> Result<Arc<dyn ObjectStore>> {
        self.validate()?;
        #[cfg(any(feature = "aws", feature = "gcp", feature = "azure"))]
        let options = object_store::ClientOptions::new()
            .with_allow_http(self.allow_http)
            .with_timeout(self.limits.operation_timeout)
            .with_connect_timeout(
                std::time::Duration::from_secs(10).min(self.limits.operation_timeout),
            );
        #[cfg(any(feature = "aws", feature = "gcp", feature = "azure"))]
        let retry = object_store::RetryConfig {
            max_retries: 3,
            retry_timeout: self.limits.operation_timeout,
            ..Default::default()
        };
        match self.provider {
            #[cfg(feature = "aws")]
            CloudProvider::S3 => {
                let mut builder = object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(&self.bucket)
                    .with_client_options(options)
                    .with_retry(retry)
                    .with_skip_signature(false)
                    .with_conditional_put(object_store::aws::S3ConditionalPut::ETagMatch)
                    .with_virtual_hosted_style_request(self.virtual_hosted_style);
                if let Some(region) = &self.region {
                    builder = builder.with_region(region);
                }
                if let Some(endpoint) = &self.endpoint {
                    builder = builder
                        .with_config(object_store::aws::AmazonS3ConfigKey::S3Endpoint, endpoint);
                }
                builder.build().map(|store| Arc::new(store) as Arc<dyn ObjectStore>)
                    .map_err(|_| Error::Invalid("could not configure S3 client; check bucket, region, endpoint, and credential provider".into()))
            }
            #[cfg(feature = "gcp")]
            CloudProvider::Gcs => {
                let mut builder = object_store::gcp::GoogleCloudStorageBuilder::from_env()
                    .with_bucket_name(&self.bucket)
                    .with_client_options(options)
                    .with_retry(retry)
                    .with_skip_signature(self.anonymous_emulator);
                if let Some(endpoint) = &self.endpoint {
                    builder = builder.with_base_url(endpoint);
                }
                builder.build().map(|store| Arc::new(store) as Arc<dyn ObjectStore>)
                    .map_err(|_| Error::Invalid("could not configure GCS client; check bucket, endpoint, and application credentials".into()))
            }
            #[cfg(feature = "azure")]
            CloudProvider::Azure => {
                let mut builder = object_store::azure::MicrosoftAzureBuilder::from_env()
                    .with_container_name(&self.bucket)
                    .with_client_options(options)
                    .with_retry(retry)
                    .with_skip_signature(self.anonymous_emulator);
                if let Some(account) = &self.account {
                    builder = builder.with_account(account);
                }
                if let Some(endpoint) = &self.endpoint {
                    builder = builder.with_endpoint(endpoint.clone());
                }
                builder.build().map(|store| Arc::new(store) as Arc<dyn ObjectStore>)
                    .map_err(|_| Error::Invalid("could not configure Azure client; check account, container, endpoint, and credential provider".into()))
            }
            #[allow(unreachable_patterns)]
            _ => Err(Error::Invalid(
                "requested cloud provider was not enabled when compiling scion-store-cloud".into(),
            )),
        }
    }
}

pub(crate) fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.len() > 512
        || prefix.is_empty()
        || prefix.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
    {
        return Err(Error::Invalid(
            "cloud prefix must contain safe, nonempty relative components".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_namespaces_and_credential_urls_are_rejected_without_echoing_them() {
        let mut config = CloudConfig::new(CloudProvider::S3, "scion-test");
        for prefix in ["", "../escape", "/absolute", "a//b", "a/", "a%2fb", "a\\b"] {
            config.prefix = prefix.into();
            assert!(config.validate().is_err());
        }
        config.prefix = "tenant-a/scion".into();
        config.endpoint = Some("https://username:secret@example.test/?token=secret".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(!error.contains("secret"));
        assert!(!error.contains("username"));
        config.endpoint = Some("http://127.0.0.1:9000".into());
        assert!(config.validate().is_err());
        config.allow_http = true;
        assert!(config.validate().is_ok());
    }
}

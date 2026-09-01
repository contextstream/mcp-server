use std::sync::{Arc, OnceLock};

use mcp_types::acceleration_layer::{
    AnalyticsProvider, ArchiveProvider, JobProvider, McpAccelerationLayer, ScheduledJobProvider,
    SearchAccelerationProvider, SignalProvider, VectorAccelerationProvider, WarmCacheProvider,
};
use tracing::{debug, info, warn};

use crate::{
    analytics::ContextStreamAnalyticsProvider,
    archive::ContextStreamArchiveProvider,
    jobs::ContextStreamJobProvider,
    signals::ContextStreamSignalProvider,
    warm_cache::{normalize_acceleration_api_url, ContextStreamWarmCacheProvider},
};

pub const MASTER_ENABLE_ENV: &str = "CONTEXTSTREAM_ACCELERATION_ENABLED";

#[derive(Debug, Clone, Default)]
pub struct AccelerationConfig {
    pub enabled: bool,
    pub signals_enabled: bool,
    pub analytics_enabled: bool,
    pub region: Option<String>,
    pub api_url: Option<String>,
    pub job_api_token: Option<String>,
}

impl AccelerationConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var(MASTER_ENABLE_ENV)
            .ok()
            .map(|value| parse_bool_flag(&value))
            .unwrap_or(false);
        let region = env_nonempty("CONTEXTSTREAM_ACCELERATION_REGION")
            .or_else(|| env_nonempty("WORKER_REGION"));
        let signals_enabled = env_nonempty("CONTEXTSTREAM_ACCELERATION_SIGNALS_ENABLED")
            .map(|value| parse_bool_flag(&value))
            .unwrap_or(true);
        let analytics_enabled = env_nonempty("CONTEXTSTREAM_ACCELERATION_ANALYTICS_ENABLED")
            .map(|value| parse_bool_flag(&value))
            .unwrap_or(false);
        let api_url = env_nonempty("CONTEXTSTREAM_ACCELERATION_API_URL")
            .or_else(|| env_nonempty("CONTEXTSTREAM_JOB_API_URL"))
            .map(|url| normalize_acceleration_api_url(&url));
        let job_api_token = env_nonempty("CONTEXTSTREAM_JOB_API_TOKEN");

        Self {
            enabled,
            signals_enabled,
            analytics_enabled,
            region,
            api_url,
            job_api_token,
        }
    }

    pub fn has_connection(&self) -> bool {
        self.api_url.is_some() && self.job_api_token.is_some()
    }
}

pub struct McpRemoteAccelerationLayer {
    config: AccelerationConfig,
    warm_cache_provider: OnceLock<Option<Arc<dyn WarmCacheProvider>>>,
    job_provider: OnceLock<Option<Arc<dyn JobProvider>>>,
    archive_provider: OnceLock<Option<Arc<dyn ArchiveProvider>>>,
    signal_provider: OnceLock<Option<Arc<dyn SignalProvider>>>,
    analytics_provider: OnceLock<Option<Arc<dyn AnalyticsProvider>>>,
}

impl std::fmt::Debug for McpRemoteAccelerationLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRemoteAccelerationLayer")
            .field("enabled", &self.config.enabled)
            .field("region", &self.config.region)
            .field("api_url", &self.config.api_url)
            .field("has_job_api_token", &self.config.job_api_token.is_some())
            .field(
                "warm_cache_provider_built",
                &self.warm_cache_provider.get().is_some(),
            )
            .field("job_provider_built", &self.job_provider.get().is_some())
            .field(
                "archive_provider_built",
                &self.archive_provider.get().is_some(),
            )
            .field(
                "signal_provider_built",
                &self.signal_provider.get().is_some(),
            )
            .field(
                "analytics_provider_built",
                &self.analytics_provider.get().is_some(),
            )
            .finish()
    }
}

impl McpRemoteAccelerationLayer {
    pub fn from_env() -> Arc<Self> {
        Self::new(AccelerationConfig::from_env())
    }

    pub fn new(config: AccelerationConfig) -> Arc<Self> {
        match (config.has_connection(), config.enabled) {
            (true, true) => info!(
                region = config.region.as_deref().unwrap_or("<unset>"),
                "acceleration-products: McpRemoteAccelerationLayer enabled"
            ),
            (true, false) => debug!(
                "acceleration-products: configured but disabled by CONTEXTSTREAM_ACCELERATION_ENABLED"
            ),
            (false, true) => warn!(
                "acceleration-products: enabled but CONTEXTSTREAM_ACCELERATION_API_URL/CONTEXTSTREAM_JOB_API_URL or CONTEXTSTREAM_JOB_API_TOKEN is missing"
            ),
            (false, false) => debug!("acceleration-products: not configured"),
        }

        Arc::new(Self {
            config,
            warm_cache_provider: OnceLock::new(),
            job_provider: OnceLock::new(),
            archive_provider: OnceLock::new(),
            signal_provider: OnceLock::new(),
            analytics_provider: OnceLock::new(),
        })
    }
}

impl McpAccelerationLayer for McpRemoteAccelerationLayer {
    fn is_enabled(&self) -> bool {
        if !self.has_connection() {
            return false;
        }
        if let Some(override_cfg) = mcp_client::get_task_config_override() {
            if let Some(force) = override_cfg.effective_acceleration_enabled() {
                return force;
            }
        }
        self.config.enabled
    }

    fn has_connection(&self) -> bool {
        self.config.has_connection()
    }

    fn search(&self) -> Option<Arc<dyn SearchAccelerationProvider>> {
        None
    }

    fn vector(&self) -> Option<Arc<dyn VectorAccelerationProvider>> {
        None
    }

    fn signals(&self) -> Option<Arc<dyn SignalProvider>> {
        if !self.is_enabled() || !self.config.signals_enabled {
            return None;
        }
        self.signal_provider
            .get_or_init(|| {
                let api_url = self.config.api_url.clone()?;
                let token = self.config.job_api_token.clone()?;
                Some(Arc::new(ContextStreamSignalProvider::new(api_url, token))
                    as Arc<dyn SignalProvider>)
            })
            .clone()
    }

    fn scheduled_jobs(&self) -> Option<Arc<dyn ScheduledJobProvider>> {
        None
    }

    fn archive(&self) -> Option<Arc<dyn ArchiveProvider>> {
        if !self.is_enabled() {
            return None;
        }
        self.archive_provider
            .get_or_init(|| {
                let api_url = self.config.api_url.clone()?;
                let token = self.config.job_api_token.clone()?;
                Some(Arc::new(ContextStreamArchiveProvider::new(api_url, token))
                    as Arc<dyn ArchiveProvider>)
            })
            .clone()
    }

    fn warm_cache(&self) -> Option<Arc<dyn WarmCacheProvider>> {
        if !self.is_enabled() {
            return None;
        }
        self.warm_cache_provider
            .get_or_init(|| {
                let api_url = self.config.api_url.clone()?;
                let token = self.config.job_api_token.clone()?;
                Some(
                    Arc::new(ContextStreamWarmCacheProvider::new(api_url, token))
                        as Arc<dyn WarmCacheProvider>,
                )
            })
            .clone()
    }

    fn analytics(&self) -> Option<Arc<dyn AnalyticsProvider>> {
        if !self.has_connection() || !self.config.analytics_enabled {
            return None;
        }
        self.analytics_provider
            .get_or_init(|| {
                let api_url = self.config.api_url.clone()?;
                let token = self.config.job_api_token.clone()?;
                Some(
                    Arc::new(ContextStreamAnalyticsProvider::new(api_url, token))
                        as Arc<dyn AnalyticsProvider>,
                )
            })
            .clone()
    }

    fn jobs(&self) -> Option<Arc<dyn JobProvider>> {
        if !self.is_enabled() {
            return None;
        }
        self.job_provider
            .get_or_init(|| {
                let api_url = self.config.api_url.clone()?;
                let token = self.config.job_api_token.clone()?;
                Some(Arc::new(ContextStreamJobProvider::new(api_url, token)) as Arc<dyn JobProvider>)
            })
            .clone()
    }
}

pub fn parse_bool_flag(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_connection_requires_url_and_token() {
        let mut config = AccelerationConfig {
            enabled: true,
            signals_enabled: true,
            analytics_enabled: false,
            region: None,
            api_url: Some("https://api.contextstream.io/v1/acceleration".to_string()),
            job_api_token: None,
        };
        assert!(!config.has_connection());

        config.job_api_token = Some("token".to_string());
        assert!(config.has_connection());
    }

    #[test]
    fn layer_enabled_requires_connection_and_master_flag() {
        let layer = McpRemoteAccelerationLayer::new(AccelerationConfig {
            enabled: true,
            signals_enabled: true,
            analytics_enabled: true,
            region: None,
            api_url: Some("https://api.contextstream.io/v1/acceleration".to_string()),
            job_api_token: Some("token".to_string()),
        });
        assert!(layer.has_connection());
        assert!(layer.is_enabled());
        assert!(layer.warm_cache().is_some());
        assert!(layer.jobs().is_some());
        assert!(layer.archive().is_some());
        assert!(layer.signals().is_some());
        assert!(layer.analytics().is_some());
    }

    #[test]
    fn signal_provider_can_be_disabled_independently() {
        let layer = McpRemoteAccelerationLayer::new(AccelerationConfig {
            enabled: true,
            signals_enabled: false,
            analytics_enabled: false,
            region: None,
            api_url: Some("https://api.contextstream.io/v1/acceleration".to_string()),
            job_api_token: Some("token".to_string()),
        });
        assert!(layer.has_connection());
        assert!(layer.is_enabled());
        assert!(layer.signals().is_none());
        assert!(layer.warm_cache().is_some());
    }

    #[test]
    fn analytics_provider_defaults_to_disabled() {
        let layer = McpRemoteAccelerationLayer::new(AccelerationConfig {
            enabled: true,
            signals_enabled: true,
            analytics_enabled: false,
            region: None,
            api_url: Some("https://api.contextstream.io/v1/acceleration".to_string()),
            job_api_token: Some("token".to_string()),
        });
        assert!(layer.has_connection());
        assert!(layer.analytics().is_none());
    }

    #[test]
    fn parse_bool_flag_recognizes_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", "  true  "] {
            assert!(parse_bool_flag(value));
        }
        for value in ["", "0", "false", "off", "nope"] {
            assert!(!parse_bool_flag(value));
        }
    }
}

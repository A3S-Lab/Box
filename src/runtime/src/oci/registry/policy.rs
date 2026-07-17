use std::time::Duration;

const MAX_ATTEMPTS_ENV: &str = "A3S_REGISTRY_PULL_MAX_ATTEMPTS";
const RETRY_INITIAL_MS_ENV: &str = "A3S_REGISTRY_PULL_RETRY_INITIAL_MS";
const RETRY_MAX_MS_ENV: &str = "A3S_REGISTRY_PULL_RETRY_MAX_MS";
const NO_PROGRESS_TIMEOUT_SECS_ENV: &str = "A3S_REGISTRY_PULL_NO_PROGRESS_TIMEOUT_SECS";
const MAX_CONCURRENT_DOWNLOADS_ENV: &str = "A3S_REGISTRY_PULL_MAX_CONCURRENT";

/// Bounded transfer settings for registry config and layer downloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryPullPolicy {
    max_attempts: usize,
    retry_initial: Duration,
    retry_max: Duration,
    no_progress_timeout: Duration,
    max_concurrent_downloads: usize,
}

impl Default for RegistryPullPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            retry_initial: Duration::from_millis(250),
            retry_max: Duration::from_secs(4),
            no_progress_timeout: Duration::from_secs(30),
            max_concurrent_downloads: 4,
        }
    }
}

impl RegistryPullPolicy {
    /// Construct a validated transfer policy.
    pub fn try_new(
        max_attempts: usize,
        retry_initial: Duration,
        retry_max: Duration,
        no_progress_timeout: Duration,
        max_concurrent_downloads: usize,
    ) -> Result<Self, String> {
        if max_attempts == 0 {
            return Err("Registry pull max attempts must be at least 1".to_string());
        }
        if retry_initial.is_zero() {
            return Err("Registry pull initial retry delay must be greater than zero".to_string());
        }
        if retry_max < retry_initial {
            return Err(
                "Registry pull maximum retry delay must not be less than the initial delay"
                    .to_string(),
            );
        }
        if no_progress_timeout.is_zero() {
            return Err("Registry pull no-progress timeout must be greater than zero".to_string());
        }
        if max_concurrent_downloads == 0 {
            return Err("Registry pull concurrency must be at least 1".to_string());
        }
        Ok(Self {
            max_attempts,
            retry_initial,
            retry_max,
            no_progress_timeout,
            max_concurrent_downloads,
        })
    }

    /// Load optional process-level overrides, retaining safe defaults for
    /// absent or invalid values.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        let max_attempts = positive_usize_env(MAX_ATTEMPTS_ENV, defaults.max_attempts);
        let retry_initial = Duration::from_millis(positive_u64_env(
            RETRY_INITIAL_MS_ENV,
            duration_millis(defaults.retry_initial),
        ));
        let retry_max = Duration::from_millis(positive_u64_env(
            RETRY_MAX_MS_ENV,
            duration_millis(defaults.retry_max),
        ));
        let no_progress_timeout = Duration::from_secs(positive_u64_env(
            NO_PROGRESS_TIMEOUT_SECS_ENV,
            defaults.no_progress_timeout.as_secs(),
        ));
        let max_concurrent_downloads = positive_usize_env(
            MAX_CONCURRENT_DOWNLOADS_ENV,
            defaults.max_concurrent_downloads,
        );

        match Self::try_new(
            max_attempts,
            retry_initial,
            retry_max,
            no_progress_timeout,
            max_concurrent_downloads,
        ) {
            Ok(policy) => policy,
            Err(error) => {
                tracing::warn!(%error, "Invalid registry pull policy override; using defaults");
                defaults
            }
        }
    }

    pub fn max_attempts(&self) -> usize {
        self.max_attempts
    }

    pub fn retry_initial(&self) -> Duration {
        self.retry_initial
    }

    pub fn retry_max(&self) -> Duration {
        self.retry_max
    }

    pub fn no_progress_timeout(&self) -> Duration {
        self.no_progress_timeout
    }

    pub fn max_concurrent_downloads(&self) -> usize {
        self.max_concurrent_downloads
    }

    pub(super) fn retry_delay(&self, failed_attempt: usize) -> Duration {
        let exponent = failed_attempt.saturating_sub(1).min(31) as u32;
        self.retry_initial
            .saturating_mul(1_u32 << exponent)
            .min(self.retry_max)
    }
}

fn positive_usize_env(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => match value.parse::<usize>() {
            Ok(parsed) if parsed > 0 => parsed,
            _ => {
                tracing::warn!(variable = name, value, default, "Ignoring invalid registry pull setting");
                default
            }
        },
        Err(_) => default,
    }
}

fn positive_u64_env(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => match value.parse::<u64>() {
            Ok(parsed) if parsed > 0 => parsed,
            _ => {
                tracing::warn!(variable = name, value, default, "Ignoring invalid registry pull setting");
                default
            }
        },
        Err(_) => default,
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_unbounded_or_zero_values() {
        let defaults = RegistryPullPolicy::default();
        assert!(RegistryPullPolicy::try_new(
            0,
            defaults.retry_initial,
            defaults.retry_max,
            defaults.no_progress_timeout,
            defaults.max_concurrent_downloads,
        )
        .is_err());
        assert!(RegistryPullPolicy::try_new(
            defaults.max_attempts,
            defaults.retry_initial,
            defaults.retry_max,
            Duration::ZERO,
            defaults.max_concurrent_downloads,
        )
        .is_err());
        assert!(RegistryPullPolicy::try_new(
            defaults.max_attempts,
            defaults.retry_initial,
            defaults.retry_max,
            defaults.no_progress_timeout,
            0,
        )
        .is_err());
    }

    #[test]
    fn retry_delay_is_exponential_and_capped() {
        let policy = RegistryPullPolicy::try_new(
            8,
            Duration::from_millis(10),
            Duration::from_millis(40),
            Duration::from_secs(1),
            2,
        )
        .unwrap();

        assert_eq!(policy.retry_delay(1), Duration::from_millis(10));
        assert_eq!(policy.retry_delay(2), Duration::from_millis(20));
        assert_eq!(policy.retry_delay(3), Duration::from_millis(40));
        assert_eq!(policy.retry_delay(8), Duration::from_millis(40));
    }
}

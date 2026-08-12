use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use a3s_box_core::EgressPolicyLimits;
use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum EgressDnsError {
    #[error("egress DNS query budget exhausted")]
    QueryBudgetExhausted,
    #[error("egress DNS cache budget exhausted")]
    CacheBudgetExhausted,
    #[error("egress DNS answer exceeded the configured address limit")]
    AnswerBudgetExceeded,
    #[error("egress DNS resolution timed out")]
    Timeout,
    #[error("egress DNS resolution returned no addresses")]
    NoAddresses,
    #[error("egress DNS resolution failed: {0}")]
    Resolve(String),
}

#[async_trait]
pub trait EgressDnsResolver: Send + Sync {
    async fn resolve(&self, hostname: &str, port: u16) -> io::Result<Vec<IpAddr>>;
}

#[derive(Debug, Default)]
pub struct SystemEgressDnsResolver;

#[async_trait]
impl EgressDnsResolver for SystemEgressDnsResolver {
    async fn resolve(&self, hostname: &str, port: u16) -> io::Result<Vec<IpAddr>> {
        tokio::net::lookup_host((hostname, port))
            .await
            .map(|addresses| addresses.map(|address| address.ip()).collect())
    }
}

#[derive(Debug, Clone)]
enum CachedAnswer {
    Positive(Vec<IpAddr>),
    Negative,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    answer: CachedAnswer,
    expires_at: Instant,
}

#[derive(Debug, Default)]
struct ResolverState {
    cache: HashMap<(String, u16), CacheEntry>,
    query_times: VecDeque<Instant>,
}

/// Per-generation DNS cache and query limiter.
pub struct BoundedEgressDnsResolver {
    resolver: Arc<dyn EgressDnsResolver>,
    limits: EgressPolicyLimits,
    state: Mutex<ResolverState>,
}

impl BoundedEgressDnsResolver {
    pub fn new(resolver: Arc<dyn EgressDnsResolver>, limits: EgressPolicyLimits) -> Self {
        Self {
            resolver,
            limits,
            state: Mutex::new(ResolverState::default()),
        }
    }

    pub async fn resolve(&self, hostname: &str, port: u16) -> Result<Vec<IpAddr>, EgressDnsError> {
        let key = (hostname.to_string(), port);
        let now = Instant::now();
        {
            let mut state = self.state.lock().await;
            state.cache.retain(|_, entry| entry.expires_at > now);
            if let Some(entry) = state.cache.get(&key) {
                return match &entry.answer {
                    CachedAnswer::Positive(addresses) => Ok(addresses.clone()),
                    CachedAnswer::Negative => Err(EgressDnsError::NoAddresses),
                };
            }

            let window_start = now.checked_sub(Duration::from_secs(60)).unwrap_or(now);
            while state
                .query_times
                .front()
                .is_some_and(|timestamp| *timestamp <= window_start)
            {
                state.query_times.pop_front();
            }
            if state.query_times.len() >= self.limits.max_dns_queries_per_minute as usize {
                return Err(EgressDnsError::QueryBudgetExhausted);
            }
            state.query_times.push_back(now);
        }

        let timeout = Duration::from_millis(u64::from(self.limits.dns_timeout_ms));
        let resolved = tokio::time::timeout(timeout, self.resolver.resolve(hostname, port))
            .await
            .map_err(|_| EgressDnsError::Timeout)?
            .map_err(|error| EgressDnsError::Resolve(error.to_string()))?;
        let mut addresses = resolved;
        addresses.sort();
        addresses.dedup();
        if addresses.len() > self.limits.max_dns_answers_per_query as usize {
            return Err(EgressDnsError::AnswerBudgetExceeded);
        }

        let (answer, ttl) = if addresses.is_empty() {
            (
                CachedAnswer::Negative,
                self.limits.max_dns_negative_ttl_seconds,
            )
        } else {
            (
                CachedAnswer::Positive(addresses.clone()),
                self.limits.max_dns_ttl_seconds,
            )
        };
        let expires_at = Instant::now()
            .checked_add(Duration::from_secs(u64::from(ttl)))
            .unwrap_or_else(Instant::now);
        {
            let mut state = self.state.lock().await;
            state
                .cache
                .retain(|_, entry| entry.expires_at > Instant::now());
            if !state.cache.contains_key(&key)
                && state.cache.len() >= self.limits.max_dns_cache_entries as usize
            {
                return Err(EgressDnsError::CacheBudgetExhausted);
            }
            state.cache.insert(key, CacheEntry { answer, expires_at });
        }

        if addresses.is_empty() {
            Err(EgressDnsError::NoAddresses)
        } else {
            Ok(addresses)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FakeResolver {
        calls: AtomicUsize,
        addresses: Vec<IpAddr>,
    }

    #[async_trait]
    impl EgressDnsResolver for FakeResolver {
        async fn resolve(&self, _hostname: &str, _port: u16) -> io::Result<Vec<IpAddr>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.addresses.clone())
        }
    }

    #[tokio::test]
    async fn cache_is_generation_local_and_deduplicates_answers() {
        let resolver = Arc::new(FakeResolver {
            calls: AtomicUsize::new(0),
            addresses: vec![
                "93.184.216.34".parse().unwrap(),
                "93.184.216.34".parse().unwrap(),
            ],
        });
        let bounded = BoundedEgressDnsResolver::new(resolver.clone(), Default::default());

        assert_eq!(bounded.resolve("example.com", 443).await.unwrap().len(), 1);
        assert_eq!(bounded.resolve("example.com", 443).await.unwrap().len(), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn query_and_answer_budgets_fail_closed() {
        let resolver = Arc::new(FakeResolver {
            calls: AtomicUsize::new(0),
            addresses: vec!["93.184.216.34".parse().unwrap()],
        });
        let mut limits = EgressPolicyLimits::default();
        limits.max_dns_queries_per_minute = 1;
        let bounded = BoundedEgressDnsResolver::new(resolver, limits);
        bounded.resolve("first.example", 443).await.unwrap();
        assert!(matches!(
            bounded.resolve("second.example", 443).await,
            Err(EgressDnsError::QueryBudgetExhausted)
        ));

        let resolver = Arc::new(FakeResolver {
            calls: AtomicUsize::new(0),
            addresses: vec![
                "93.184.216.34".parse().unwrap(),
                "93.184.216.35".parse().unwrap(),
            ],
        });
        let mut limits = EgressPolicyLimits::default();
        limits.max_dns_answers_per_query = 1;
        let bounded = BoundedEgressDnsResolver::new(resolver, limits);
        assert!(matches!(
            bounded.resolve("example.com", 443).await,
            Err(EgressDnsError::AnswerBudgetExceeded)
        ));
    }
}

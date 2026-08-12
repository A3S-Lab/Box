//! Generation-scoped host egress proxy components.

mod decision_log;
mod http;
#[cfg(unix)]
mod policy_channel;
mod proxy;
#[cfg(test)]
mod proxy_tests;
mod resolver;
mod transport;

pub use decision_log::{
    EgressDecisionEvent, EgressDecisionLog, EgressDecisionLogError, EgressDecisionRecord,
    EgressRuntimeDecisionReason, EGRESS_DECISION_LOG_SCHEMA_V1,
};
pub use proxy::{EgressProxyConfig, EgressProxyError, EgressProxyHandle};
pub use resolver::{
    BoundedEgressDnsResolver, EgressDnsError, EgressDnsResolver, SystemEgressDnsResolver,
};

use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

#[derive(Debug, Error)]
pub(super) enum ReadyLifetimeError {
    #[error("sandbox timeout exceeds the supported duration")]
    TimeoutTooLarge,
    #[error("sandbox expiry exceeds the supported timestamp range")]
    ExpiryOverflow,
}

pub(super) fn ready_lifetime(
    observed_ready_at: DateTime<Utc>,
    runtime_started_at: DateTime<Utc>,
    timeout_seconds: u64,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ReadyLifetimeError> {
    let ready_at = observed_ready_at.max(runtime_started_at);
    let timeout_seconds =
        i64::try_from(timeout_seconds).map_err(|_| ReadyLifetimeError::TimeoutTooLarge)?;
    let expires_at = ready_at
        .checked_add_signed(Duration::seconds(timeout_seconds))
        .ok_or(ReadyLifetimeError::ExpiryOverflow)?;
    Ok((ready_at, expires_at))
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    #[test]
    fn usable_lifetime_starts_after_runtime_and_control_readiness() {
        let runtime_started_at = Utc.with_ymd_and_hms(2026, 7, 16, 1, 0, 0).unwrap();
        let observed_ready_at = runtime_started_at + Duration::seconds(7);

        let (ready_at, expires_at) =
            ready_lifetime(observed_ready_at, runtime_started_at, 60).unwrap();

        assert_eq!(ready_at, observed_ready_at);
        assert_eq!(expires_at, observed_ready_at + Duration::seconds(60));
    }
}

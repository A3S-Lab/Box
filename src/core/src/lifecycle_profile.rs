//! Opt-in machine-readable lifecycle profiling.
//!
//! The benchmark harness enables this through
//! `A3S_BOX_LIFECYCLE_PROFILE=1`. Normal CLI output and production behavior are
//! unchanged when the variable is absent. Events deliberately contain only a
//! stable phase name, elapsed time, and process identity; workload arguments,
//! paths, credentials, and other caller data are never emitted.

use std::time::Duration;

use serde::Serialize;

/// Environment variable that enables lifecycle JSONL events on stderr.
pub const LIFECYCLE_PROFILE_ENV: &str = "A3S_BOX_LIFECYCLE_PROFILE";

/// Prefix that makes profile events unambiguous in mixed CLI stderr.
pub const LIFECYCLE_PROFILE_PREFIX: &str = "A3S_BOX_LIFECYCLE ";

const LIFECYCLE_PROFILE_SCHEMA: &str = "a3s.box.lifecycle-profile.v1";

#[derive(Serialize)]
struct LifecycleProfileEvent<'a> {
    schema: &'static str,
    phase: &'a str,
    duration_ns: u64,
    pid: u32,
}

/// Emit one best-effort JSONL phase event when lifecycle profiling is enabled.
///
/// Profiling must never change lifecycle success or failure. Serialization and
/// stderr write failures are therefore intentionally ignored.
pub fn record_lifecycle_phase(phase: &str, duration: Duration) {
    if !lifecycle_profile_enabled(std::env::var_os(LIFECYCLE_PROFILE_ENV).as_deref()) {
        return;
    }
    if let Some(line) = lifecycle_profile_line(phase, duration, std::process::id()) {
        eprintln!("{LIFECYCLE_PROFILE_PREFIX}{line}");
    }
}

fn lifecycle_profile_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn lifecycle_profile_line(phase: &str, duration: Duration, pid: u32) -> Option<String> {
    let duration_ns = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    serde_json::to_string(&LifecycleProfileEvent {
        schema: LIFECYCLE_PROFILE_SCHEMA,
        phase,
        duration_ns,
        pid,
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_gate_accepts_only_explicit_true_values() {
        assert!(!lifecycle_profile_enabled(None));
        assert!(!lifecycle_profile_enabled(Some(std::ffi::OsStr::new("0"))));
        assert!(lifecycle_profile_enabled(Some(std::ffi::OsStr::new("1"))));
        assert!(lifecycle_profile_enabled(Some(std::ffi::OsStr::new(
            "TRUE"
        ))));
    }

    #[test]
    fn profile_event_is_stable_json_without_caller_data() {
        let line = lifecycle_profile_line("sandbox.layout", Duration::from_micros(1250), 42)
            .expect("profile event should serialize");
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["schema"], LIFECYCLE_PROFILE_SCHEMA);
        assert_eq!(event["phase"], "sandbox.layout");
        assert_eq!(event["duration_ns"], 1_250_000);
        assert_eq!(event["pid"], 42);
        assert_eq!(event.as_object().unwrap().len(), 4);
    }
}

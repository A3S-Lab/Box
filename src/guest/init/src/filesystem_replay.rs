//! Guest journals for keyed mutating filesystem requests.
//!
//! Unkeyed mutating ops stay single-shot. Keyed `MakeDir` / `Move` / `Remove`
//! store the exact response before the host write so an ambiguous transport
//! loss can replay one effect instead of double-applying.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use a3s_box_core::{FilesystemOp, FilesystemRequest, FilesystemResponse};
use sha2::{Digest, Sha256};

const FILESYSTEM_REPLAY_MAX_ENTRIES: usize = 128;
const FILESYSTEM_REPLAY_MAX_IN_FLIGHT: usize = 32;
const FILESYSTEM_REPLAY_WAIT: Duration = Duration::from_secs(30);

static FILESYSTEM_REPLAY_CACHE: OnceLock<FilesystemReplayCache> = OnceLock::new();

pub(crate) fn filesystem_replay_cache() -> &'static FilesystemReplayCache {
    FILESYSTEM_REPLAY_CACHE.get_or_init(FilesystemReplayCache::default)
}

pub(crate) fn is_mutating_filesystem_op(op: FilesystemOp) -> bool {
    matches!(
        op,
        FilesystemOp::MakeDir | FilesystemOp::Move | FilesystemOp::Remove
    )
}

pub(crate) fn filesystem_request_digest(request: &FilesystemRequest) -> Result<[u8; 32], String> {
    let payload = serde_json::to_vec(request)
        .map_err(|error| format!("could not encode filesystem request identity: {error}"))?;
    let digest = Sha256::digest(payload);
    let mut output = [0_u8; 32];
    output.copy_from_slice(&digest);
    Ok(output)
}

fn validate_filesystem_request_id(request_id: &str) -> Result<(), String> {
    if request_id.is_empty() || request_id.len() > 512 || request_id.contains('\0') {
        return Err("filesystem request ID is invalid".to_string());
    }
    Ok(())
}

enum ExistingReplay {
    InFlight([u8; 32]),
    Ready([u8; 32], Arc<FilesystemResponse>),
}

enum FilesystemReplayEntry {
    InFlight {
        digest: [u8; 32],
    },
    Ready {
        digest: [u8; 32],
        response: Arc<FilesystemResponse>,
    },
}

#[derive(Default)]
struct FilesystemReplayState {
    entries: HashMap<String, FilesystemReplayEntry>,
    completed_order: VecDeque<String>,
    in_flight: usize,
}

pub(crate) struct FilesystemReplayCache {
    state: Mutex<FilesystemReplayState>,
    changed: Condvar,
    max_entries: usize,
    max_in_flight: usize,
}

impl Default for FilesystemReplayCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(FilesystemReplayState::default()),
            changed: Condvar::new(),
            max_entries: FILESYSTEM_REPLAY_MAX_ENTRIES,
            max_in_flight: FILESYSTEM_REPLAY_MAX_IN_FLIGHT,
        }
    }
}

pub(crate) enum FilesystemReplayAcquire<'a> {
    Execute(FilesystemReplayClaim<'a>),
    Replay(Arc<FilesystemResponse>),
}

pub(crate) struct FilesystemReplayClaim<'a> {
    cache: &'a FilesystemReplayCache,
    request_id: String,
    digest: [u8; 32],
    completed: bool,
}

impl FilesystemReplayCache {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, FilesystemReplayState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn acquire(
        &self,
        request_id: &str,
        digest: [u8; 32],
    ) -> Result<FilesystemReplayAcquire<'_>, String> {
        validate_filesystem_request_id(request_id)?;
        let started = std::time::Instant::now();
        let mut state = self.lock_state();

        loop {
            let existing = state.entries.get(request_id).map(|entry| match entry {
                FilesystemReplayEntry::InFlight { digest } => ExistingReplay::InFlight(*digest),
                FilesystemReplayEntry::Ready { digest, response } => {
                    ExistingReplay::Ready(*digest, Arc::clone(response))
                }
            });
            match existing {
                Some(ExistingReplay::Ready(existing_digest, response)) => {
                    if existing_digest != digest {
                        return Err(format!(
                            "filesystem request ID {request_id:?} conflicts with cached content"
                        ));
                    }
                    state.completed_order.retain(|value| value != request_id);
                    state.completed_order.push_back(request_id.to_string());
                    return Ok(FilesystemReplayAcquire::Replay(response));
                }
                Some(ExistingReplay::InFlight(existing_digest)) => {
                    if existing_digest != digest {
                        return Err(format!(
                            "filesystem request ID {request_id:?} conflicts with in-flight content"
                        ));
                    }
                    let remaining = FILESYSTEM_REPLAY_WAIT
                        .checked_sub(started.elapsed())
                        .ok_or_else(|| {
                            format!(
                                "timed out waiting for filesystem request {request_id:?} to complete"
                            )
                        })?;
                    if remaining.is_zero() {
                        return Err(format!(
                            "timed out waiting for filesystem request {request_id:?} to complete"
                        ));
                    }
                    let waited = self
                        .changed
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state = waited.0;
                    if waited.1.timed_out() {
                        return Err(format!(
                            "timed out waiting for filesystem request {request_id:?} to complete"
                        ));
                    }
                }
                None => {
                    while state.entries.len() >= self.max_entries {
                        if !evict_oldest_completed(&mut state) {
                            return Err(
                                "filesystem replay cache is full of in-flight requests".to_string()
                            );
                        }
                    }
                    if state.in_flight >= self.max_in_flight {
                        return Err("filesystem replay in-flight limit reached".to_string());
                    }
                    state.entries.insert(
                        request_id.to_string(),
                        FilesystemReplayEntry::InFlight { digest },
                    );
                    state.in_flight += 1;
                    return Ok(FilesystemReplayAcquire::Execute(FilesystemReplayClaim {
                        cache: self,
                        request_id: request_id.to_string(),
                        digest,
                        completed: false,
                    }));
                }
            }
        }
    }

    fn complete(
        &self,
        request_id: &str,
        digest: [u8; 32],
        response: FilesystemResponse,
    ) -> Result<Arc<FilesystemResponse>, String> {
        let mut state = self.lock_state();
        match state.entries.get(request_id) {
            Some(FilesystemReplayEntry::InFlight {
                digest: existing_digest,
            }) if *existing_digest == digest => {}
            Some(_) => {
                return Err(format!(
                    "filesystem replay claim for {request_id:?} changed before completion"
                ));
            }
            None => {
                return Err(format!(
                    "filesystem replay claim for {request_id:?} disappeared before completion"
                ));
            }
        }
        let shared = Arc::new(response);
        state.entries.insert(
            request_id.to_string(),
            FilesystemReplayEntry::Ready {
                digest,
                response: Arc::clone(&shared),
            },
        );
        state.in_flight = state.in_flight.saturating_sub(1);
        state.completed_order.retain(|value| value != request_id);
        state.completed_order.push_back(request_id.to_string());
        self.changed.notify_all();
        Ok(shared)
    }

    fn abort(&self, request_id: &str, digest: [u8; 32]) {
        let mut state = self.lock_state();
        if matches!(
            state.entries.get(request_id),
            Some(FilesystemReplayEntry::InFlight {
                digest: existing_digest
            }) if *existing_digest == digest
        ) {
            state.entries.remove(request_id);
            state.in_flight = state.in_flight.saturating_sub(1);
            self.changed.notify_all();
        }
    }
}

impl Drop for FilesystemReplayClaim<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.cache.abort(&self.request_id, self.digest);
        }
    }
}

impl FilesystemReplayClaim<'_> {
    pub(crate) fn complete(
        mut self,
        response: FilesystemResponse,
    ) -> Result<Arc<FilesystemResponse>, String> {
        let shared = self
            .cache
            .complete(&self.request_id, self.digest, response)?;
        self.completed = true;
        Ok(shared)
    }
}

fn evict_oldest_completed(state: &mut FilesystemReplayState) -> bool {
    while let Some(request_id) = state.completed_order.pop_front() {
        if matches!(
            state.entries.get(&request_id),
            Some(FilesystemReplayEntry::Ready { .. })
        ) {
            state.entries.remove(&request_id);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdir_request(request_id: Option<&str>) -> FilesystemRequest {
        FilesystemRequest {
            op: FilesystemOp::MakeDir,
            path: "/tmp/journaled".into(),
            destination: None,
            depth: 0,
            user: None,
            request_id: request_id.map(str::to_string),
        }
    }

    #[test]
    fn keyed_mutation_replays_exact_response() {
        let cache = FilesystemReplayCache::default();
        let request = mkdir_request(Some("fs-1"));
        let digest = filesystem_request_digest(&request).unwrap();
        {
            let claim = match cache.acquire("fs-1", digest).unwrap() {
                FilesystemReplayAcquire::Execute(claim) => claim,
                FilesystemReplayAcquire::Replay(_) => panic!("first acquire must execute"),
            };
            let response = FilesystemResponse {
                success: true,
                entry: None,
                entries: Vec::new(),
                error: None,
            };
            claim.complete(response).unwrap();
        }

        let replayed = match cache.acquire("fs-1", digest).unwrap() {
            FilesystemReplayAcquire::Replay(replayed) => replayed,
            FilesystemReplayAcquire::Execute(_) => panic!("second acquire must replay"),
        };
        assert!(replayed.success);
        assert!(replayed.error.is_none());
    }

    #[test]
    fn conflicting_body_for_same_id_fails_closed() {
        let cache = FilesystemReplayCache::default();
        let first = mkdir_request(Some("fs-conflict"));
        let digest = filesystem_request_digest(&first).unwrap();
        {
            let claim = match cache.acquire("fs-conflict", digest).unwrap() {
                FilesystemReplayAcquire::Execute(claim) => claim,
                FilesystemReplayAcquire::Replay(_) => panic!("expected execute"),
            };
            claim
                .complete(FilesystemResponse {
                    success: true,
                    entry: None,
                    entries: Vec::new(),
                    error: None,
                })
                .unwrap();
        }

        let mut second = first;
        second.path = "/tmp/other".into();
        let other_digest = filesystem_request_digest(&second).unwrap();
        let error = match cache.acquire("fs-conflict", other_digest) {
            Ok(_) => panic!("expected conflict"),
            Err(error) => error,
        };
        assert!(error.contains("conflicts"));
    }
}

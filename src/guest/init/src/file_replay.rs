//! Guest journals for keyed file uploads.
//!
//! Unkeyed uploads stay single-shot. Keyed [`FileOp::Upload`] stores the exact
//! response so an ambiguous transport loss can replay one write instead of
//! truncating/double-writing. Downloads remain unkeyed (read-only / idempotent
//! at the host retry layer only when naturally safe).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use a3s_box_core::{FileOp, FileRequest, FileResponse};
use sha2::{Digest, Sha256};

const FILE_REPLAY_MAX_ENTRIES: usize = 128;
const FILE_REPLAY_MAX_IN_FLIGHT: usize = 32;
const FILE_REPLAY_WAIT: Duration = Duration::from_secs(30);

static FILE_REPLAY_CACHE: OnceLock<FileReplayCache> = OnceLock::new();

pub(crate) fn file_replay_cache() -> &'static FileReplayCache {
    FILE_REPLAY_CACHE.get_or_init(FileReplayCache::default)
}

pub(crate) fn is_keyed_upload(request: &FileRequest) -> bool {
    matches!(request.op, FileOp::Upload)
        && request
            .request_id
            .as_ref()
            .is_some_and(|request_id| !request_id.is_empty())
}

pub(crate) fn file_request_digest(request: &FileRequest) -> Result<[u8; 32], String> {
    let payload = serde_json::to_vec(request)
        .map_err(|error| format!("could not encode file request identity: {error}"))?;
    let digest = Sha256::digest(payload);
    let mut output = [0_u8; 32];
    output.copy_from_slice(&digest);
    Ok(output)
}

fn validate_file_request_id(request_id: &str) -> Result<(), String> {
    if request_id.is_empty() || request_id.len() > 512 || request_id.contains('\0') {
        return Err("file request ID is invalid".to_string());
    }
    Ok(())
}

enum ExistingReplay {
    InFlight([u8; 32]),
    Ready([u8; 32], Arc<FileResponse>),
}

enum FileReplayEntry {
    InFlight {
        digest: [u8; 32],
    },
    Ready {
        digest: [u8; 32],
        response: Arc<FileResponse>,
    },
}

#[derive(Default)]
struct FileReplayState {
    entries: HashMap<String, FileReplayEntry>,
    completed_order: VecDeque<String>,
    in_flight: usize,
}

pub(crate) struct FileReplayCache {
    state: Mutex<FileReplayState>,
    changed: Condvar,
    max_entries: usize,
    max_in_flight: usize,
}

impl Default for FileReplayCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(FileReplayState::default()),
            changed: Condvar::new(),
            max_entries: FILE_REPLAY_MAX_ENTRIES,
            max_in_flight: FILE_REPLAY_MAX_IN_FLIGHT,
        }
    }
}

pub(crate) enum FileReplayAcquire<'a> {
    Execute(FileReplayClaim<'a>),
    Replay(Arc<FileResponse>),
}

pub(crate) struct FileReplayClaim<'a> {
    cache: &'a FileReplayCache,
    request_id: String,
    digest: [u8; 32],
    completed: bool,
}

impl FileReplayCache {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, FileReplayState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn acquire(
        &self,
        request_id: &str,
        digest: [u8; 32],
    ) -> Result<FileReplayAcquire<'_>, String> {
        validate_file_request_id(request_id)?;
        let started = std::time::Instant::now();
        let mut state = self.lock_state();

        loop {
            let existing = state.entries.get(request_id).map(|entry| match entry {
                FileReplayEntry::InFlight { digest } => ExistingReplay::InFlight(*digest),
                FileReplayEntry::Ready { digest, response } => {
                    ExistingReplay::Ready(*digest, Arc::clone(response))
                }
            });
            match existing {
                Some(ExistingReplay::Ready(existing_digest, response)) => {
                    if existing_digest != digest {
                        return Err(format!(
                            "file request ID {request_id:?} conflicts with cached content"
                        ));
                    }
                    state.completed_order.retain(|value| value != request_id);
                    state.completed_order.push_back(request_id.to_string());
                    return Ok(FileReplayAcquire::Replay(response));
                }
                Some(ExistingReplay::InFlight(existing_digest)) => {
                    if existing_digest != digest {
                        return Err(format!(
                            "file request ID {request_id:?} conflicts with in-flight content"
                        ));
                    }
                    let remaining =
                        FILE_REPLAY_WAIT
                            .checked_sub(started.elapsed())
                            .ok_or_else(|| {
                                format!(
                                    "timed out waiting for file request {request_id:?} to complete"
                                )
                            })?;
                    if remaining.is_zero() {
                        return Err(format!(
                            "timed out waiting for file request {request_id:?} to complete"
                        ));
                    }
                    let waited = self
                        .changed
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state = waited.0;
                    if waited.1.timed_out() {
                        return Err(format!(
                            "timed out waiting for file request {request_id:?} to complete"
                        ));
                    }
                }
                None => {
                    while state.entries.len() >= self.max_entries {
                        if !evict_oldest_completed(&mut state) {
                            return Err(
                                "file replay cache is full of in-flight requests".to_string()
                            );
                        }
                    }
                    if state.in_flight >= self.max_in_flight {
                        return Err("file replay in-flight limit reached".to_string());
                    }
                    state
                        .entries
                        .insert(request_id.to_string(), FileReplayEntry::InFlight { digest });
                    state.in_flight += 1;
                    return Ok(FileReplayAcquire::Execute(FileReplayClaim {
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
        response: FileResponse,
    ) -> Result<Arc<FileResponse>, String> {
        let mut state = self.lock_state();
        match state.entries.get(request_id) {
            Some(FileReplayEntry::InFlight {
                digest: existing_digest,
            }) if *existing_digest == digest => {}
            Some(_) => {
                return Err(format!(
                    "file replay claim for {request_id:?} changed before completion"
                ));
            }
            None => {
                return Err(format!(
                    "file replay claim for {request_id:?} disappeared before completion"
                ));
            }
        }
        let shared = Arc::new(response);
        state.entries.insert(
            request_id.to_string(),
            FileReplayEntry::Ready {
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
            Some(FileReplayEntry::InFlight {
                digest: existing_digest
            }) if *existing_digest == digest
        ) {
            state.entries.remove(request_id);
            state.in_flight = state.in_flight.saturating_sub(1);
            self.changed.notify_all();
        }
    }
}

impl Drop for FileReplayClaim<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.cache.abort(&self.request_id, self.digest);
        }
    }
}

impl FileReplayClaim<'_> {
    pub(crate) fn complete(mut self, response: FileResponse) -> Result<Arc<FileResponse>, String> {
        let shared = self
            .cache
            .complete(&self.request_id, self.digest, response)?;
        self.completed = true;
        Ok(shared)
    }
}

fn evict_oldest_completed(state: &mut FileReplayState) -> bool {
    while let Some(request_id) = state.completed_order.pop_front() {
        if matches!(
            state.entries.get(&request_id),
            Some(FileReplayEntry::Ready { .. })
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

    fn upload_request(request_id: Option<&str>, data: &str) -> FileRequest {
        FileRequest {
            op: FileOp::Upload,
            guest_path: "/tmp/journaled.txt".into(),
            data: Some(data.into()),
            user: None,
            max_bytes: None,
            request_id: request_id.map(str::to_string),
        }
    }

    #[test]
    fn keyed_upload_replays_exact_response() {
        let cache = FileReplayCache::default();
        let request = upload_request(Some("file-1"), "aGVsbG8=");
        let digest = file_request_digest(&request).unwrap();
        {
            let claim = match cache.acquire("file-1", digest).unwrap() {
                FileReplayAcquire::Execute(claim) => claim,
                FileReplayAcquire::Replay(_) => panic!("first acquire must execute"),
            };
            let response = FileResponse {
                success: true,
                data: None,
                size: 5,
                error: None,
            };
            claim.complete(response).unwrap();
        }

        let replayed = match cache.acquire("file-1", digest).unwrap() {
            FileReplayAcquire::Replay(replayed) => replayed,
            FileReplayAcquire::Execute(_) => panic!("second acquire must replay"),
        };
        assert!(replayed.success);
        assert_eq!(replayed.size, 5);
    }

    #[test]
    fn conflicting_body_for_same_id_fails_closed() {
        let cache = FileReplayCache::default();
        let first = upload_request(Some("file-conflict"), "aGVsbG8=");
        let digest = file_request_digest(&first).unwrap();
        {
            let claim = match cache.acquire("file-conflict", digest).unwrap() {
                FileReplayAcquire::Execute(claim) => claim,
                FileReplayAcquire::Replay(_) => panic!("expected execute"),
            };
            claim
                .complete(FileResponse {
                    success: true,
                    data: None,
                    size: 5,
                    error: None,
                })
                .unwrap();
        }

        let second = upload_request(Some("file-conflict"), "d29ybGQ=");
        let other_digest = file_request_digest(&second).unwrap();
        let error = match cache.acquire("file-conflict", other_digest) {
            Ok(_) => panic!("expected conflict"),
            Err(error) => error,
        };
        assert!(error.contains("conflicts"));
    }
}

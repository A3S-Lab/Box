use std::pin::Pin;

use a3s_box_core::{
    ExecutionEventsRequest, ExecutionGeneration, ExecutionId, ExecutionRuntimeEvent,
    MAX_EXECUTION_EVENT_BATCH_ITEMS,
};
use futures::{stream, Stream};

use super::Sandbox;
use crate::{A3sBoxClient, ClientError, Result};

/// Default number of runtime events fetched by each streaming poll.
pub const DEFAULT_EVENT_STREAM_BATCH_ITEMS: u32 = 256;

/// Default duration of each cancellable event long poll.
pub const DEFAULT_EVENT_STREAM_WAIT_TIMEOUT_MS: u64 = 1_000;

/// Controls for one exact-generation continuous event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxEventStreamOptions {
    pub after_sequence: u64,
    pub batch_items: u32,
    pub wait_timeout_ms: u64,
}

impl SandboxEventStreamOptions {
    pub const fn after_sequence(mut self, after_sequence: u64) -> Self {
        self.after_sequence = after_sequence;
        self
    }

    pub const fn batch_items(mut self, batch_items: u32) -> Self {
        self.batch_items = batch_items;
        self
    }

    pub const fn wait_timeout_ms(mut self, wait_timeout_ms: u64) -> Self {
        self.wait_timeout_ms = wait_timeout_ms;
        self
    }

    fn validate(self) -> Result<Self> {
        if self.batch_items == 0 || self.batch_items > MAX_EXECUTION_EVENT_BATCH_ITEMS {
            return Err(ClientError::Validation(format!(
                "event stream batch size must be between 1 and {MAX_EXECUTION_EVENT_BATCH_ITEMS}"
            )));
        }
        if self.wait_timeout_ms == 0 {
            return Err(ClientError::Validation(
                "event stream wait timeout must be greater than zero".to_string(),
            ));
        }
        Ok(self)
    }
}

impl Default for SandboxEventStreamOptions {
    fn default() -> Self {
        Self {
            after_sequence: 0,
            batch_items: DEFAULT_EVENT_STREAM_BATCH_ITEMS,
            wait_timeout_ms: DEFAULT_EVENT_STREAM_WAIT_TIMEOUT_MS,
        }
    }
}

/// A cancellation-safe, backpressured stream of exact-generation runtime events.
///
/// Dropping the stream or the future currently returned by [`Stream`] cancels
/// the active long poll. An error is emitted once and then terminates the stream.
pub type SandboxEventStream =
    Pin<Box<dyn Stream<Item = Result<ExecutionRuntimeEvent>> + Send + 'static>>;

struct EventStreamState {
    client: A3sBoxClient,
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
    cursor: u64,
    batch_items: u32,
    wait_timeout_ms: u64,
    pending: std::vec::IntoIter<ExecutionRuntimeEvent>,
}

impl Sandbox {
    /// Continuously consume ordered events from the Sandbox generation visible
    /// when this method is called.
    ///
    /// The stream never follows a later restart generation. A generation change,
    /// target drift, or runtime failure is returned as the final stream item.
    pub fn stream_events(&self, options: SandboxEventStreamOptions) -> Result<SandboxEventStream> {
        let options = options.validate()?;
        let (execution_id, generation) = self.inner.observable_execution()?;
        let state = EventStreamState {
            client: self.inner.client.clone(),
            execution_id,
            generation,
            cursor: options.after_sequence,
            batch_items: options.batch_items,
            wait_timeout_ms: options.wait_timeout_ms,
            pending: Vec::new().into_iter(),
        };

        Ok(Box::pin(stream::unfold(Some(state), |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.next() {
                    return Some((Ok(event), Some(state)));
                }

                let request = ExecutionEventsRequest {
                    after_sequence: state.cursor,
                    limit: state.batch_items,
                    wait_timeout_ms: Some(state.wait_timeout_ms),
                };
                match state
                    .client
                    .execution_events(&state.execution_id, state.generation, request)
                    .await
                {
                    Ok(batch) => {
                        state.cursor = batch.next_sequence;
                        state.pending = batch.events.into_iter();
                        if state.pending.len() == 0 {
                            // A custom runtime may return before honoring the long-poll
                            // timeout. Yield here so such a runtime cannot monopolize an
                            // executor while the stream remains empty.
                            tokio::task::yield_now().await;
                        }
                    }
                    Err(error) => return Some((Err(error), None)),
                }
            }
        })))
    }
}

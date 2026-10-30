//! Asynchronous runtime infrastructure and task join utilities.
//!
//! Provides conversion traits and mapping utilities for asynchronous tasks executed via
//! [`tokio::task::spawn_blocking`]. Enforces the Microsoft Pragmatic Rust Guidelines
//! panic handling policies (**M-PANIC-ON-BUG**, **M-PANIC-IS-STOP**): worker thread panics
//! are resumed via [`std::panic::resume_unwind`] rather than silently converted into
//! recoverable I/O errors.

use std::panic;

/// Conversion trait for domain errors produced from Tokio task join failures.
///
/// Hardware drivers and runtime components implementing this trait allow [`map_join_result`]
/// to convert asynchronous task cancellation events into strongly-typed domain errors while
/// preserving worker panics.
///
/// # Cancellation Contract
///
/// Task cancellations represent expected termination events during graceful daemon shutdown
/// and service stopping. Implementations must construct strongly-typed domain cancellation
/// errors that callers and supervisor loops can identify via query predicates (e.g., `is_cancelled()`)
/// to prevent treating normal shutdown as hardware or bus malfunctions.
pub trait FromJoinError {
    /// Constructs a domain error representing an aborted or cancelled task with descriptive context.
    fn from_cancelled(message: impl Into<Box<str>>) -> Self;
}

/// Maps the outcome of an asynchronous blocking task (`tokio::task::JoinError`) with diagnostic task context.
///
/// # Panics
///
/// Propagates worker thread panics by resuming unwinding via [`std::panic::resume_unwind`]
/// rather than laundering programming bugs into recoverable errors (**M-PANIC-ON-BUG**).
///
/// # Errors
///
/// Returns `Err(E)` if the inner task computation returned an error or if the task was
/// cancelled by the Tokio runtime.
pub fn map_join_result<T, E: FromJoinError>(
    res: Result<Result<T, E>, tokio::task::JoinError>,
    task_context: &str,
) -> Result<T, E> {
    match res {
        Ok(result) => result,
        Err(join_err) if join_err.is_panic() => panic::resume_unwind(join_err.into_panic()),
        Err(join_err) if join_err.is_cancelled() => Err(E::from_cancelled(format!(
            "{task_context} task was cancelled"
        ))),
        Err(join_err) => {
            // In Tokio, JoinError is guaranteed to be either panic or cancellation.
            unreachable!("Tokio JoinError is guaranteed to be panic or cancelled: {join_err}");
        }
    }
}

/// Spawns a blocking closure on Tokio and maps the join outcome via [`map_join_result`].
///
/// # Panics
///
/// Resumes unwinding if `f` panics on the worker thread (**M-PANIC-ON-BUG**).
///
/// # Errors
///
/// Returns `Err(E)` if `f` returns an error or if the blocking task is cancelled.
pub async fn run_blocking<T, E, F>(task_context: &'static str, f: F) -> Result<T, E>
where
    T: Send + 'static,
    E: FromJoinError + Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    map_join_result(tokio::task::spawn_blocking(f).await, task_context)
}

/// Spawns an asynchronous task, immediately aborts it, and maps the resulting cancellation
/// into the typed domain error via [`FromJoinError`].
///
/// Intended for unit and integration testing of task cancellation handling across drivers.
pub async fn abort_and_map<E>(task_context: &'static str) -> E
where
    E: FromJoinError + Send + 'static,
{
    let task = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        Ok::<(), E>(())
    });
    task.abort();
    map_join_result(task.await, task_context).unwrap_err()
}

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
        Err(join_err) if join_err.is_cancelled() => {
            tracing::debug!(
                task_context,
                "blocking task was cancelled (expected during graceful shutdown)"
            );
            Err(E::from_cancelled(format!(
                "{task_context} task was cancelled"
            )))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum TestError {
        Cancelled(Box<str>),
    }

    impl FromJoinError for TestError {
        fn from_cancelled(message: impl Into<Box<str>>) -> Self {
            Self::Cancelled(message.into())
        }
    }

    impl TestError {
        fn is_cancelled(&self) -> bool {
            matches!(self, Self::Cancelled(_))
        }
    }

    #[tokio::test]
    async fn run_blocking_returns_inner_result() {
        let res: Result<u8, TestError> = run_blocking("happy_path", || Ok(7)).await;
        assert_eq!(res, Ok(7));
    }

    #[tokio::test]
    async fn run_blocking_maps_cancellation_with_context() {
        // Abort a long-running spawned task by mapping its join handle through
        // the production mapping path (no post-panic code per lint hygiene).
        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok::<u8, TestError>(0)
        });
        task.abort();
        let mapped = map_join_result(task.await, "drive_task");
        let err = mapped.unwrap_err();
        assert!(err.is_cancelled());
        match err {
            TestError::Cancelled(msg) => {
                assert!(msg.contains("drive_task"), "context missing: {msg}");
            }
        }
    }

    #[tokio::test]
    async fn abort_and_map_produces_contextual_cancellation() {
        let err: TestError = abort_and_map("light_sensor_task").await;
        assert!(err.is_cancelled());
        match err {
            TestError::Cancelled(msg) => assert!(msg.contains("light_sensor_task")),
        }
    }
}

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

/// Spawns a blocking closure on the Tokio threadpool and maps the join outcome via [`map_join_result`].
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct DummyError(Box<str>);

    impl FromJoinError for DummyError {
        fn from_cancelled(message: impl Into<Box<str>>) -> Self {
            Self(message.into())
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum TestDriverError {
        Cancelled(Box<str>),
    }

    impl FromJoinError for TestDriverError {
        fn from_cancelled(message: impl Into<Box<str>>) -> Self {
            Self::Cancelled(message.into())
        }
    }

    #[test]
    fn map_join_result_ok_and_err() {
        let ok_join: Result<Result<u32, DummyError>, tokio::task::JoinError> = Ok(Ok(42));
        assert_eq!(map_join_result(ok_join, "dummy computation").unwrap(), 42);

        let err_join: Result<Result<u32, DummyError>, tokio::task::JoinError> =
            Ok(Err(DummyError("inner failure".into())));
        assert_eq!(
            map_join_result(err_join, "dummy computation").unwrap_err(),
            DummyError("inner failure".into())
        );
    }

    #[tokio::test]
    async fn map_join_result_handles_cancellation() {
        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok::<u32, DummyError>(99)
        });

        task.abort();
        let join_res = task.await;
        assert!(join_res.as_ref().unwrap_err().is_cancelled());

        let err = map_join_result(join_res, "temperature polling").unwrap_err();
        assert_eq!(
            err,
            DummyError("temperature polling task was cancelled".into())
        );
    }

    #[tokio::test]
    async fn map_join_result_propagates_panic_payloads() {
        // Table-driven verification of panic unwinding without unreachable post-panic code
        let cases = [
            (
                "fatal static worker crash",
                tokio::spawn(async {
                    panic!("fatal static worker crash");
                }),
            ),
            (
                "formatted worker crash",
                tokio::spawn(async {
                    panic!("{}", "formatted worker crash");
                }),
            ),
        ];

        for (expected_substr, task) in cases {
            let join_res: Result<Result<u32, DummyError>, _> = task.await.map(Ok::<_, DummyError>);
            assert!(join_res.as_ref().unwrap_err().is_panic());

            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = map_join_result(join_res, "crash task");
            }));

            assert!(panic_result.is_err());
            let panic_payload = panic_result.unwrap_err();
            let msg = panic_payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or_default();
            assert!(
                msg.contains(expected_substr),
                "expected panic message to contain '{expected_substr}', got '{msg}'"
            );
        }
    }

    #[test]
    fn test_driver_error_cancellation_construction() {
        let err = TestDriverError::from_cancelled("sensor bus abort");
        assert_eq!(err, TestDriverError::Cancelled("sensor bus abort".into()));
    }
}

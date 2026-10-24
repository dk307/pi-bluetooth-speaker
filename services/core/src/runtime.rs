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
pub trait FromJoinError {
    /// Constructs a domain error representing an aborted or cancelled task.
    fn from_cancelled(message: impl Into<Box<str>>) -> Self;
}

/// Maps the outcome of an asynchronous blocking task (`tokio::task::JoinError`).
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
) -> Result<T, E> {
    match res {
        Ok(result) => result,
        Err(join_err) if join_err.is_panic() => panic::resume_unwind(join_err.into_panic()),
        Err(join_err) if join_err.is_cancelled() => {
            Err(E::from_cancelled("asynchronous task was cancelled"))
        }
        Err(join_err) => {
            // In Tokio, JoinError is either panic or cancellation.
            unreachable!("Tokio JoinError is guaranteed to be panic or cancelled: {join_err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[derive(Debug, PartialEq, Eq)]
    struct DummyError(Box<str>);

    impl FromJoinError for DummyError {
        fn from_cancelled(message: impl Into<Box<str>>) -> Self {
            Self(message.into())
        }
    }

    #[derive(Debug)]
    struct TestIoError(io::Error);

    impl FromJoinError for TestIoError {
        fn from_cancelled(message: impl Into<Box<str>>) -> Self {
            Self(io::Error::new(
                io::ErrorKind::Interrupted,
                message.into().to_string(),
            ))
        }
    }

    #[test]
    fn map_join_result_ok_and_err() {
        let ok_join: Result<Result<u32, DummyError>, tokio::task::JoinError> = Ok(Ok(42));
        assert_eq!(map_join_result(ok_join).unwrap(), 42);

        let err_join: Result<Result<u32, DummyError>, tokio::task::JoinError> =
            Ok(Err(DummyError("inner failure".into())));
        assert_eq!(
            map_join_result(err_join).unwrap_err(),
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

        let err = map_join_result(join_res).unwrap_err();
        assert_eq!(err, DummyError("asynchronous task was cancelled".into()));
    }

    #[tokio::test]
    async fn map_join_result_propagates_panic() {
        let task = tokio::spawn(async {
            panic!("fatal worker crash in blocking pool");
            #[expect(
                unreachable_code,
                reason = "code after panic is needed to satisfy return type"
            )]
            Ok::<u32, DummyError>(1)
        });

        let join_res = task.await;
        assert!(join_res.as_ref().unwrap_err().is_panic());

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = map_join_result(join_res);
        }));

        assert!(panic_result.is_err());
        let panic_payload = panic_result.unwrap_err();
        let msg = panic_payload
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or_default();
        assert!(msg.contains("fatal worker crash"));
    }

    #[tokio::test]
    async fn map_join_result_propagates_panic_string() {
        let task = tokio::spawn(async {
            panic!("{}", "formatted crash message");
            #[expect(
                unreachable_code,
                reason = "code after panic is needed to satisfy return type"
            )]
            Ok::<u32, DummyError>(1)
        });

        let join_res = task.await;
        assert!(join_res.as_ref().unwrap_err().is_panic());

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = map_join_result(join_res);
        }));

        assert!(panic_result.is_err());
        let panic_payload = panic_result.unwrap_err();
        let msg = panic_payload
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or_default();
        assert!(msg.contains("formatted crash message"));
    }

    #[test]
    fn test_io_error_cancellation_construction() {
        let err = TestIoError::from_cancelled("aborted");
        assert_eq!(err.0.kind(), io::ErrorKind::Interrupted);
    }
}

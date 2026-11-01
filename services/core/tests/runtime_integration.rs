//! Integration and asynchronous task join tests for runtime utilities.

use speaker_core::runtime::{FromJoinError, abort_and_map, map_join_result};

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

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

struct PanicHookGuard(Option<PanicHook>);

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            std::panic::set_hook(h);
        }
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
    let err: DummyError = abort_and_map("temperature polling").await;
    assert_eq!(
        err,
        DummyError("temperature polling task was cancelled".into())
    );
}

#[tokio::test]
async fn map_join_result_propagates_panic_payloads() {
    // Mutates the process-global panic hook for the duration of this test to silence
    // intentional worker crashes in test logs; scoped via PanicHookGuard (RAII restore).
    let _hook_guard = PanicHookGuard(Some(std::panic::take_hook()));
    std::panic::set_hook(Box::new(|_| {}));

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
        let join_res: Result<Result<(), DummyError>, _> = task.await.map(Ok::<(), DummyError>);
        assert!(join_res.as_ref().unwrap_err().is_panic());

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = map_join_result(join_res, "crash task");
        }));

        assert!(panic_result.is_err());
        let panic_payload = panic_result.unwrap_err();
        let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
            s.clone()
        } else {
            panic!("unexpected panic payload type: {panic_payload:?}");
        };
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

#[tokio::test]
async fn run_blocking_success_and_error() {
    use speaker_core::runtime::run_blocking;

    let res: Result<i32, DummyError> = run_blocking("compute", || Ok(123)).await;
    assert_eq!(res.unwrap(), 123);

    let err: Result<i32, DummyError> =
        run_blocking("compute", || Err(DummyError("inner failure".into()))).await;
    assert_eq!(err.unwrap_err(), DummyError("inner failure".into()));
}

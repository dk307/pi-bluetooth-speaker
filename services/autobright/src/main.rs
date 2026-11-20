//! Auto-brightness appliance service binary target.
//!
//! The executable is named `autobright` in `Cargo.toml`, matching the
//! `autobright.service` systemd unit. It installs the daemon tracing
//! subscriber, parses the CLI, builds a Tokio runtime for zbus, and delegates
//! to [`autobright::daemon::main`].

use std::process::ExitCode;

use speaker_common::diag::{env_args, resolve_cli_opts};

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    speaker_common::logging::init_daemon_logging();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!(panic = %info, "Panic occurred in autobright");
    }));

    let opts = match resolve_cli_opts(
        autobright::parse_cli_args_from(env_args()),
        "autobright",
        autobright::print_usage,
    ) {
        Ok(opts) => opts,
        Err(exit_code) => return exit_code,
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "failed to create Tokio runtime for zbus");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(autobright::daemon::main(opts))
}

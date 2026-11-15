//! Process exit codes for `coder`'s own failures, kept apart from the tool's exit code
//! that `coder run` passes through. Values follow established conventions (sysexits.h,
//! GNU `timeout`, shell signal codes) so they rarely collide with a tool's own codes.

use std::fmt;

/// Any other `coder` error.
pub const FAILURE: i32 = 1;
/// `coder run` needs confirmation but stdin is not a terminal and `--yes` wasn't
/// given (sysexits `EX_USAGE`).
pub const CONFIRMATION_REQUIRED: i32 = 64;
/// The sandbox guard refused the project directory, or the user declined the prompt
/// (sysexits `EX_NOPERM`).
pub const NOT_PERMITTED: i32 = 77;
/// `coder run --timeout` elapsed and the tool container was stopped (GNU `timeout`).
pub const TIMED_OUT: i32 = 124;
/// Interrupted by SIGINT, SIGTERM or SIGHUP (128 + SIGINT).
pub const INTERRUPTED: i32 = 130;

/// An error that ends the process with a specific exit code instead of [`FAILURE`].
#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub message: String,
}

impl ExitError {
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitError {}

/// The exit code for `err`: an [`ExitError`]'s own code anywhere in its chain, else
/// [`FAILURE`].
#[must_use]
pub fn code_for(err: &anyhow::Error) -> i32 {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<ExitError>())
        .map_or(FAILURE, |e| e.code)
}

/// The exit code to report for a finished child: its own code, or 128 + the signal
/// that killed it (the shell convention), or [`FAILURE`] if neither is known.
#[must_use]
pub fn from_status(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn plain_errors_map_to_failure() {
        assert_eq!(code_for(&anyhow::anyhow!("boom")), FAILURE);
    }

    #[test]
    fn exit_error_code_survives_added_context() {
        let err: anyhow::Result<()> = Err(ExitError::new(NOT_PERMITTED, "refused").into());
        let err = err.context("running tool").unwrap_err();
        assert_eq!(code_for(&err), NOT_PERMITTED);
    }

    #[cfg(unix)]
    #[test]
    fn signal_deaths_map_to_128_plus_signal() {
        use std::os::unix::process::ExitStatusExt;
        // raw wait status: low 7 bits are the terminating signal (9 = SIGKILL)
        assert_eq!(from_status(std::process::ExitStatus::from_raw(9)), 137);
        // exited normally with code 3: code lives in the high byte
        assert_eq!(from_status(std::process::ExitStatus::from_raw(3 << 8)), 3);
    }
}

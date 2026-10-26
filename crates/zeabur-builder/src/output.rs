//! The only place the binary writes to the terminal.

use std::io::Write as _;
use std::process::ExitCode;

/// Writes a line to stdout. A closed pipe (`| head`) is a failure and does not panic.
pub(crate) fn stdout_line(text: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{text}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Writes a line to stderr, ignoring failure: there is nowhere left to report it.
pub(crate) fn stderr_line(text: &str) {
    let mut err = std::io::stderr().lock();
    if writeln!(err, "{text}").is_err() {
        // Nothing else can be done.
    }
}

//! `erp-ui-compile`: reads a framed request on stdin (at most `MAX_REQUEST_BYTES`), writes the
//! canonical result on stdout (design note §11). Exit status 0 when the result has no error, 1 when
//! it carries errors (a rejected template, a set error, a failed compile; the result is still on
//! stdout), 2 for a malformed or oversized request or an I/O error (nothing on stdout).

use erp_ui_compiler_cli::{EXIT_MALFORMED, read_request, respond};
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let response = match read_request(std::io::stdin().lock()).and_then(|input| respond(&input)) {
        Ok(response) => response,
        Err(e) => {
            eprintln!("erp-ui-compile: {e}");
            return ExitCode::from(EXIT_MALFORMED);
        }
    };
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = stdout
        .write_all(response.bytes())
        .and_then(|()| stdout.flush())
    {
        eprintln!("erp-ui-compile: cannot write stdout: {e}");
        return ExitCode::from(EXIT_MALFORMED);
    }
    ExitCode::from(response.exit_status())
}

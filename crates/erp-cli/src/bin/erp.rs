use std::process::ExitCode;

fn main() -> ExitCode {
    erp_cli::run(std::env::args_os())
}

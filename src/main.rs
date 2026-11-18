use std::process::ExitCode;

fn main() -> ExitCode {
    match dots_cli::cli::run(std::env::args_os().collect()) {
        Ok(status) => status.into(),
        Err(error) => {
            eprintln!("dots: {error}");
            error.status.into()
        }
    }
}

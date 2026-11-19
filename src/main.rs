use std::process::ExitCode;

fn main() -> ExitCode {
    if let Some(status) = dots_cli::supervisor::terminal_observer(std::env::args_os()) {
        return status.into();
    }
    match dots_cli::cli::run(std::env::args_os().collect()) {
        Ok(status) => status.into(),
        Err(error) => {
            eprintln!("dots: {error}");
            error.status.into()
        }
    }
}

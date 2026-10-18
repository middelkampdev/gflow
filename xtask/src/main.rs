#[cfg(unix)]
mod check;
#[cfg(unix)]
mod fake;
mod render;
mod spec;

use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask render | cargo xtask check [--gflow PATH] [cycle ...]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(unix)]
    {
        let program = std::path::Path::new(&args[0]).file_name().unwrap_or_default().to_string_lossy();
        if let Some(code) = fake::dispatch(&program, &args[1..]) {
            return code;
        }
    }
    let result = match args.get(1).map(String::as_str) {
        Some("render") => render::run(),
        #[cfg(unix)]
        Some("check") => check::run(&args[2..]),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

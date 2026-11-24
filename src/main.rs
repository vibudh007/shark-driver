mod cli;

use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

fn main() -> ExitCode {
    let args: io::Result<Vec<String>> = env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "arguments must be valid UTF-8")
            })
        })
        .collect();
    let mut out = io::stdout().lock();
    let result = args.and_then(|args| {
        cli::run(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            &mut out,
        )
    });
    if let Err(error) = result {
        let _ = writeln!(io::stderr().lock(), "{}: {error}", env!("CARGO_PKG_NAME"));
        return if error.kind() == io::ErrorKind::InvalidInput {
            ExitCode::from(2)
        } else {
            ExitCode::FAILURE
        };
    }
    ExitCode::SUCCESS
}

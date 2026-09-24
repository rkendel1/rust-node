use rust_node::{Runtime, RuntimeError};
use std::{env, path::Path, process};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> Result<(), RuntimeError> {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| String::from("runtime"));
    let script_path = args.next().ok_or_else(|| RuntimeError::MissingEntryPoint {
        program: program.clone(),
    })?;

    if args.next().is_some() {
        return Err(RuntimeError::UnexpectedArguments { program });
    }

    let mut runtime = Runtime::new();
    runtime.execute_file(Path::new(&script_path))
}

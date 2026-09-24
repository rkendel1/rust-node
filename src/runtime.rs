use boa_engine::{Context, Source};
use boa_runtime::{Console, console::DefaultLogger};
use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub struct Runtime;

#[derive(Debug)]
pub enum RuntimeError {
    MissingEntryPoint { program: String },
    UnexpectedArguments { program: String },
    ReadScript { path: PathBuf, source: io::Error },
    InitializeConsole(String),
    Execute(String),
}

impl Runtime {
    pub fn new() -> Self {
        Self
    }

    pub fn execute(&self, source: &str) -> Result<(), RuntimeError> {
        let mut context = Context::default();

        Console::register_with_logger(DefaultLogger, &mut context)
            .map_err(|error| RuntimeError::InitializeConsole(error.to_string()))?;

        context
            .eval(Source::from_bytes(source))
            .map_err(|error| RuntimeError::Execute(error.to_string()))?;

        Ok(())
    }

    pub fn execute_file(&self, path: &Path) -> Result<(), RuntimeError> {
        let source = fs::read_to_string(path).map_err(|source| RuntimeError::ReadScript {
            path: path.to_path_buf(),
            source,
        })?;

        self.execute(&source)
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEntryPoint { program } => write!(f, "usage: {program} <file.js>"),
            Self::UnexpectedArguments { program } => {
                write!(f, "usage: {program} <file.js>")
            }
            Self::ReadScript { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::InitializeConsole(message) => {
                write!(f, "failed to initialize console: {message}")
            }
            Self::Execute(message) => write!(f, "javascript execution failed: {message}"),
        }
    }
}

impl Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::{Runtime, RuntimeError};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn executes_javascript_source() {
        let runtime = Runtime::new();

        runtime
            .execute(
                r#"
                const value = 40 + 2;
                if (value !== 42) {
                    throw new Error("wrong answer");
                }
                "#,
            )
            .expect("runtime should execute valid JavaScript");
    }

    #[test]
    fn reports_missing_script_files() {
        let runtime = Runtime::new();
        let missing_path = PathBuf::from("/tmp/rust-node-tests/does-not-exist.js");

        let error = runtime
            .execute_file(&missing_path)
            .expect_err("missing script should fail");

        assert!(matches!(
            error,
            RuntimeError::ReadScript { path, .. } if path == missing_path
        ));
    }

    #[test]
    fn reports_syntax_errors() {
        let runtime = Runtime::new();

        let error = runtime
            .execute("function () {")
            .expect_err("invalid JavaScript should fail");

        assert!(matches!(error, RuntimeError::Execute(_)));
    }

    #[test]
    fn executes_javascript_files() {
        let runtime = Runtime::new();
        let script_path = unique_temp_script_path();

        fs::create_dir_all(script_path.parent().expect("script should have a parent"))
            .expect("temp script directory should be created");
        fs::write(
            &script_path,
            "const answer = 21 * 2; if (answer !== 42) { throw new Error('wrong answer'); }",
        )
        .expect("script should be written");

        runtime
            .execute_file(&script_path)
            .expect("runtime should execute a JavaScript file");

        let _ = fs::remove_file(script_path);
    }

    fn unique_temp_script_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();

        std::env::temp_dir().join(format!("rust-node-tests-{nanos}.js"))
    }
}

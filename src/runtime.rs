use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source,
    Trace, js_string,
    object::{JsObject, ObjectInitializer, builtins::JsFunction},
    property::Attribute,
};
use boa_gc::Finalize;
use boa_runtime::{Console, console::DefaultLogger};
use std::{
    cell::RefCell,
    collections::VecDeque,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

const RUNTIME_STATE_KEY: &str = "__rustNodeRuntime";
const MODULE_CACHE_KEY: &str = "moduleCache";
const TIMER_CALLBACKS_KEY: &str = "timerCallbacks";

#[derive(Debug)]
pub struct Runtime {
    context: RuntimeContext,
}

#[derive(Debug)]
struct RuntimeContext {
    engine: Context,
    host_state: HostStateHandle,
}

#[derive(Debug)]
pub enum RuntimeError {
    MissingEntryPoint { program: String },
    UnexpectedArguments { program: String },
    ReadScript { path: PathBuf, source: io::Error },
    ResolvePath { path: PathBuf, source: io::Error },
    UnsupportedModuleSpecifier { specifier: String },
    Execute(String),
}

#[derive(Debug, Default)]
struct HostState {
    next_timer_id: u64,
    current_modules: Vec<PathBuf>,
    task_queue: TimerTaskQueue,
}

#[derive(Clone, Debug, Trace, Finalize)]
struct HostStateHandle {
    #[unsafe_ignore_trace]
    inner: Rc<RefCell<HostState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Timer,
}

#[derive(Debug, Clone, Copy)]
struct ScheduledTask {
    id: u64,
    kind: TaskKind,
    due_at: Instant,
}

trait TaskQueue {
    fn push(&mut self, task: ScheduledTask);
    fn cancel(&mut self, id: u64) -> bool;
    fn pop_due(&mut self, now: Instant) -> Option<ScheduledTask>;
    fn next_due(&self) -> Option<Instant>;
}

#[derive(Debug, Default)]
struct TimerTaskQueue {
    tasks: VecDeque<ScheduledTask>,
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            context: RuntimeContext::new(),
        }
    }

    pub fn execute(&mut self, source: &str) -> Result<(), RuntimeError> {
        self.context.execute_source(source)?;
        self.context.drain_task_queue()
    }

    pub fn execute_file(&mut self, path: &Path) -> Result<(), RuntimeError> {
        let canonical_path = canonicalize_path(path)?;
        self.context.execute_entry_module(&canonical_path)?;
        self.context.drain_task_queue()
    }
}

impl RuntimeContext {
    fn new() -> Self {
        let mut engine = Context::default();
        Console::register_with_logger(DefaultLogger, &mut engine)
            .expect("console should register in a fresh runtime context");

        let host_state = HostStateHandle {
            inner: Rc::new(RefCell::new(HostState {
                next_timer_id: 1,
                ..HostState::default()
            })),
        };

        let mut context = Self { engine, host_state };
        context
            .install_runtime_globals()
            .expect("runtime globals should register in a fresh runtime context");
        context
    }

    fn execute_source(&mut self, source: &str) -> Result<(), RuntimeError> {
        self.engine
            .eval(Source::from_bytes(source))
            .map_err(RuntimeError::from_js_error)?;
        self.engine
            .run_jobs()
            .map_err(RuntimeError::from_js_error)?;
        Ok(())
    }

    fn execute_entry_module(&mut self, path: &Path) -> Result<(), RuntimeError> {
        self.execute_commonjs_module(path, false).map(|_| ())
    }

    fn execute_commonjs_module(
        &mut self,
        path: &Path,
        use_cache: bool,
    ) -> Result<JsValue, RuntimeError> {
        let source = fs::read_to_string(path).map_err(|source| RuntimeError::ReadScript {
            path: path.to_path_buf(),
            source,
        })?;

        let exports = ObjectInitializer::new(&mut self.engine).build();
        let module = {
            let mut initializer = ObjectInitializer::new(&mut self.engine);
            initializer.property(js_string!("exports"), exports.clone(), Attribute::all());
            initializer.build()
        };

        if use_cache {
            self.cache_module_value(path, exports.clone().into())?;
        }

        let wrapper_source = commonjs_wrapper(&source);
        let wrapper_value = self
            .engine
            .eval(Source::from_bytes(wrapper_source.as_bytes()).with_path(path))
            .map_err(RuntimeError::from_js_error);

        let result = match wrapper_value {
            Ok(wrapper_value) => {
                let wrapper = wrapper_value
                    .as_object()
                    .and_then(JsFunction::from_object)
                    .ok_or_else(|| {
                        RuntimeError::Execute(String::from(
                            "module wrapper did not evaluate to a function",
                        ))
                    })?;
                let require = self.global_function(js_string!("require"))?;

                {
                    self.host_state
                        .inner
                        .borrow_mut()
                        .current_modules
                        .push(path.to_path_buf());
                }

                let file_name = path.to_string_lossy().into_owned();
                let directory_name = path
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let this_value: JsValue = exports.clone().into();
                let arguments = [
                    exports.into(),
                    require.into(),
                    module.clone().into(),
                    JsString::from(file_name.as_str()).into(),
                    JsString::from(directory_name.as_str()).into(),
                ];
                let call_result = wrapper
                    .call(&this_value, &arguments, &mut self.engine)
                    .map_err(RuntimeError::from_js_error);

                self.host_state.inner.borrow_mut().current_modules.pop();

                call_result?;
                self.engine
                    .run_jobs()
                    .map_err(RuntimeError::from_js_error)?;
                module
                    .get(js_string!("exports"), &mut self.engine)
                    .map_err(RuntimeError::from_js_error)
            }
            Err(error) => Err(error),
        };

        if result.is_err() && use_cache {
            self.remove_cached_module(path)?;
        }

        if let Ok(ref module_exports) = result
            && use_cache
        {
            self.cache_module_value(path, module_exports.clone())?;
        }

        result
    }

    fn drain_task_queue(&mut self) -> Result<(), RuntimeError> {
        loop {
            let next_due = self.host_state.inner.borrow().task_queue.next_due();
            let Some(next_due) = next_due else {
                return Ok(());
            };

            let now = Instant::now();
            if next_due > now {
                thread::sleep(next_due.saturating_duration_since(now));
            }

            let next_task = {
                self.host_state
                    .inner
                    .borrow_mut()
                    .task_queue
                    .pop_due(Instant::now())
            };

            let Some(task) = next_task else {
                continue;
            };

            match task.kind {
                TaskKind::Timer => self.invoke_timer(task.id)?,
            }
        }
    }

    fn invoke_timer(&mut self, timer_id: u64) -> Result<(), RuntimeError> {
        let callbacks = self.timer_callbacks_object()?;
        let key = JsString::from(timer_id.to_string().as_str());
        let callback = callbacks
            .get(key.clone(), &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;

        if callback.is_undefined() {
            return Ok(());
        }

        callbacks
            .delete_property_or_throw(key.clone(), &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;

        let callback = callback
            .as_object()
            .and_then(JsFunction::from_object)
            .ok_or_else(|| {
                RuntimeError::Execute(format!("timer {timer_id} callback is not callable"))
            })?;

        callback
            .call(&JsValue::undefined(), &[], &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;
        self.engine
            .run_jobs()
            .map_err(RuntimeError::from_js_error)?;
        Ok(())
    }

    fn install_runtime_globals(&mut self) -> JsResult<()> {
        let module_cache = ObjectInitializer::new(&mut self.engine).build();
        let timer_callbacks = ObjectInitializer::new(&mut self.engine).build();

        let runtime_state = {
            let mut initializer = ObjectInitializer::new(&mut self.engine);
            initializer.property(js_string!(MODULE_CACHE_KEY), module_cache, Attribute::all());
            initializer.property(
                js_string!(TIMER_CALLBACKS_KEY),
                timer_callbacks,
                Attribute::all(),
            );
            initializer.build()
        };

        self.engine.register_global_property(
            js_string!(RUNTIME_STATE_KEY),
            runtime_state,
            Attribute::empty(),
        )?;
        self.engine.register_global_builtin_callable(
            js_string!("require"),
            1,
            NativeFunction::from_copy_closure_with_captures(require_host, self.host_state.clone()),
        )?;
        self.engine.register_global_builtin_callable(
            js_string!("setTimeout"),
            2,
            NativeFunction::from_copy_closure_with_captures(
                set_timeout_host,
                self.host_state.clone(),
            ),
        )?;
        self.engine.register_global_builtin_callable(
            js_string!("clearTimeout"),
            1,
            NativeFunction::from_copy_closure_with_captures(
                clear_timeout_host,
                self.host_state.clone(),
            ),
        )?;
        Ok(())
    }

    fn runtime_state_object(&mut self) -> Result<JsObject, RuntimeError> {
        let value = self
            .engine
            .global_object()
            .get(js_string!(RUNTIME_STATE_KEY), &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;
        value
            .as_object()
            .ok_or_else(|| RuntimeError::Execute(String::from("runtime state object is missing")))
    }

    fn module_cache_object(&mut self) -> Result<JsObject, RuntimeError> {
        let runtime_state = self.runtime_state_object()?;
        let value = runtime_state
            .get(js_string!(MODULE_CACHE_KEY), &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;
        value
            .as_object()
            .ok_or_else(|| RuntimeError::Execute(String::from("module cache is missing")))
    }

    fn timer_callbacks_object(&mut self) -> Result<JsObject, RuntimeError> {
        let runtime_state = self.runtime_state_object()?;
        let value = runtime_state
            .get(js_string!(TIMER_CALLBACKS_KEY), &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;
        value
            .as_object()
            .ok_or_else(|| RuntimeError::Execute(String::from("timer callback store is missing")))
    }

    fn cache_module_value(&mut self, path: &Path, value: JsValue) -> Result<(), RuntimeError> {
        let cache = self.module_cache_object()?;
        cache
            .set(module_cache_key(path), value, true, &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;
        Ok(())
    }

    fn remove_cached_module(&mut self, path: &Path) -> Result<(), RuntimeError> {
        let cache = self.module_cache_object()?;
        let key = module_cache_key(path);
        if cache
            .has_property(key.clone(), &mut self.engine)
            .map_err(RuntimeError::from_js_error)?
        {
            cache
                .delete_property_or_throw(key, &mut self.engine)
                .map_err(RuntimeError::from_js_error)?;
        }
        Ok(())
    }

    fn global_function(&mut self, name: JsString) -> Result<JsFunction, RuntimeError> {
        let value = self
            .engine
            .global_object()
            .get(name.clone(), &mut self.engine)
            .map_err(RuntimeError::from_js_error)?;
        value
            .as_object()
            .and_then(JsFunction::from_object)
            .ok_or_else(|| {
                RuntimeError::Execute(format!("{} is not callable", name.to_std_string_escaped()))
            })
    }
}

impl RuntimeError {
    fn from_js_error(error: JsError) -> Self {
        Self::Execute(error.to_string())
    }
}

impl HostState {
    fn current_module_path(&self) -> Option<&Path> {
        self.current_modules.last().map(PathBuf::as_path)
    }

    fn schedule_timer(&mut self, delay_ms: u64) -> u64 {
        let timer_id = self.next_timer_id;
        self.next_timer_id = self.next_timer_id.saturating_add(1);
        self.task_queue.push(ScheduledTask {
            id: timer_id,
            kind: TaskKind::Timer,
            due_at: Instant::now() + Duration::from_millis(delay_ms),
        });
        timer_id
    }
}

impl TaskQueue for TimerTaskQueue {
    fn push(&mut self, task: ScheduledTask) {
        let insertion_index = self
            .tasks
            .iter()
            .position(|queued| queued.due_at > task.due_at)
            .unwrap_or(self.tasks.len());
        self.tasks.insert(insertion_index, task);
    }

    fn cancel(&mut self, id: u64) -> bool {
        if let Some(index) = self.tasks.iter().position(|task| task.id == id) {
            self.tasks.remove(index);
            return true;
        }
        false
    }

    fn pop_due(&mut self, now: Instant) -> Option<ScheduledTask> {
        match self.tasks.front().copied() {
            Some(task) if task.due_at <= now => self.tasks.pop_front(),
            _ => None,
        }
    }

    fn next_due(&self) -> Option<Instant> {
        self.tasks.front().map(|task| task.due_at)
    }
}

fn require_host(
    _: &JsValue,
    args: &[JsValue],
    host_state: &HostStateHandle,
    context: &mut Context,
) -> JsResult<JsValue> {
    let specifier = args
        .get_or_undefined(0)
        .to_string(context)?
        .to_std_string_escaped();
    if !is_local_module_specifier(&specifier) {
        return Err(if specifier.starts_with("node:") {
            JsNativeError::typ()
                .with_message("built-in Node modules are not supported yet")
                .into()
        } else {
            JsNativeError::typ()
                .with_message(format!(
                    "unsupported module specifier `{specifier}`; only local CommonJS modules are supported"
                ))
                .into()
        });
    }

    let resolved_path = {
        let borrowed_state = host_state.inner.borrow();
        let current_module = borrowed_state.current_module_path().ok_or_else(|| {
            JsNativeError::error()
                .with_message("require() is only available while executing a module")
        })?;
        resolve_module_path(current_module, &specifier)?
    };

    let cache = host_runtime_object(context, MODULE_CACHE_KEY)?;
    let cache_key = module_cache_key(&resolved_path);
    if cache.has_property(cache_key.clone(), context)? {
        return cache.get(cache_key, context);
    }

    execute_commonjs_module_from_host(&resolved_path, context, host_state)
}

fn set_timeout_host(
    _: &JsValue,
    args: &[JsValue],
    host_state: &HostStateHandle,
    context: &mut Context,
) -> JsResult<JsValue> {
    let callback = args.get_or_undefined(0).clone();
    let callback_object = callback.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message("setTimeout callback must be a function")
    })?;
    if JsFunction::from_object(callback_object).is_none() {
        return Err(JsNativeError::typ()
            .with_message("setTimeout callback must be a function")
            .into());
    }

    let delay_ms = coerce_delay_ms(args.get_or_undefined(1), context)?;
    let timer_id = host_state.inner.borrow_mut().schedule_timer(delay_ms);
    let callbacks = host_runtime_object(context, TIMER_CALLBACKS_KEY)?;
    callbacks.set(
        JsString::from(timer_id.to_string().as_str()),
        callback,
        true,
        context,
    )?;
    Ok(JsValue::from(timer_id as f64))
}

fn clear_timeout_host(
    _: &JsValue,
    args: &[JsValue],
    host_state: &HostStateHandle,
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(timer_id) = js_value_to_timer_id(args.get_or_undefined(0), context)? else {
        return Ok(JsValue::undefined());
    };

    host_state.inner.borrow_mut().task_queue.cancel(timer_id);
    let callbacks = host_runtime_object(context, TIMER_CALLBACKS_KEY)?;
    let key = JsString::from(timer_id.to_string().as_str());
    if callbacks.has_property(key.clone(), context)? {
        callbacks.delete_property_or_throw(key, context)?;
    }
    Ok(JsValue::undefined())
}

fn execute_commonjs_module_from_host(
    path: &Path,
    context: &mut Context,
    host_state: &HostStateHandle,
) -> JsResult<JsValue> {
    let source = fs::read_to_string(path).map_err(|error| {
        JsNativeError::error().with_message(format!("failed to read {}: {error}", path.display()))
    })?;

    let exports = ObjectInitializer::new(context).build();
    let module = {
        let mut initializer = ObjectInitializer::new(context);
        initializer.property(js_string!("exports"), exports.clone(), Attribute::all());
        initializer.build()
    };
    let cache = host_runtime_object(context, MODULE_CACHE_KEY)?;
    let cache_key = module_cache_key(path);
    cache.set(cache_key.clone(), exports.clone(), true, context)?;

    let wrapper_source = commonjs_wrapper(&source);
    let wrapper_value = context.eval(Source::from_bytes(wrapper_source.as_bytes()).with_path(path));

    let execution_result = match wrapper_value {
        Ok(wrapper_value) => {
            let wrapper = wrapper_value
                .as_object()
                .and_then(JsFunction::from_object)
                .ok_or_else(|| {
                    JsNativeError::error()
                        .with_message("module wrapper did not evaluate to a function")
                })?;
            let require = context
                .global_object()
                .get(js_string!("require"), context)?;
            let require = require
                .as_object()
                .and_then(JsFunction::from_object)
                .ok_or_else(|| JsNativeError::error().with_message("require is not callable"))?;

            host_state
                .inner
                .borrow_mut()
                .current_modules
                .push(path.to_path_buf());

            let file_name = path.to_string_lossy().into_owned();
            let directory_name = path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default();
            let this_value: JsValue = exports.clone().into();
            let arguments = [
                exports.into(),
                require.into(),
                module.clone().into(),
                JsString::from(file_name.as_str()).into(),
                JsString::from(directory_name.as_str()).into(),
            ];
            let call_result = wrapper.call(&this_value, &arguments, context);

            host_state.inner.borrow_mut().current_modules.pop();

            call_result?;
            context.run_jobs()?;
            module.get(js_string!("exports"), context)
        }
        Err(error) => Err(error),
    };

    match execution_result {
        Ok(module_exports) => {
            cache.set(cache_key, module_exports.clone(), true, context)?;
            Ok(module_exports)
        }
        Err(error) => {
            if cache.has_property(cache_key.clone(), context)? {
                cache.delete_property_or_throw(cache_key, context)?;
            }
            Err(error)
        }
    }
}

fn host_runtime_object(context: &mut Context, property_name: &str) -> JsResult<JsObject> {
    let runtime_state = context
        .global_object()
        .get(js_string!(RUNTIME_STATE_KEY), context)?;
    let runtime_state = runtime_state
        .as_object()
        .ok_or_else(|| JsNativeError::error().with_message("runtime state object is missing"))?;
    let property = runtime_state.get(JsString::from(property_name), context)?;
    property.as_object().ok_or_else(|| {
        JsNativeError::error()
            .with_message("runtime state property is missing")
            .into()
    })
}

fn canonicalize_path(path: &Path) -> Result<PathBuf, RuntimeError> {
    path.canonicalize()
        .map_err(|source| RuntimeError::ResolvePath {
            path: path.to_path_buf(),
            source,
        })
}

fn resolve_module_path(current_module: &Path, specifier: &str) -> JsResult<PathBuf> {
    if !is_local_module_specifier(specifier) {
        return Err(RuntimeError::UnsupportedModuleSpecifier {
            specifier: specifier.to_string(),
        }
        .into_js_error());
    }

    let parent = current_module.parent().ok_or_else(|| {
        JsNativeError::error().with_message(format!(
            "cannot resolve `{specifier}` relative to {}",
            current_module.display()
        ))
    })?;
    let candidate = parent.join(specifier);
    let resolved_candidate = match candidate.extension() {
        Some(_) if candidate.is_file() => candidate,
        Some(_) => candidate,
        None if candidate.is_file() => candidate,
        None => candidate.with_extension("js"),
    };

    resolved_candidate.canonicalize().map_err(|error| {
        JsNativeError::error()
            .with_message(format!(
                "failed to resolve `{specifier}` from {}: {error}",
                current_module.display()
            ))
            .into()
    })
}

fn is_local_module_specifier(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}

fn module_cache_key(path: &Path) -> JsString {
    JsString::from(path.to_string_lossy().into_owned())
}

fn commonjs_wrapper(source: &str) -> String {
    format!("(function (exports, require, module, __filename, __dirname) {{{source}\n}})")
}

fn coerce_delay_ms(value: &JsValue, context: &mut Context) -> JsResult<u64> {
    let numeric = value.to_number(context)?;
    if !numeric.is_finite() || numeric <= 0.0 {
        return Ok(0);
    }

    let milliseconds = numeric.floor();
    if milliseconds >= u64::MAX as f64 {
        Ok(u64::MAX)
    } else {
        Ok(milliseconds as u64)
    }
}

fn js_value_to_timer_id(value: &JsValue, context: &mut Context) -> JsResult<Option<u64>> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }

    let numeric = value.to_number(context)?;
    if !numeric.is_finite() || numeric < 0.0 {
        return Ok(None);
    }

    Ok(Some(numeric.floor() as u64))
}

impl RuntimeError {
    fn into_js_error(self) -> JsError {
        match self {
            Self::UnsupportedModuleSpecifier { specifier } => JsNativeError::typ()
                .with_message(format!(
                    "unsupported module specifier `{specifier}`; only local CommonJS modules are supported"
                ))
                .into(),
            other => JsNativeError::error().with_message(other.to_string()).into(),
        }
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
            Self::ResolvePath { path, source } => {
                write!(f, "failed to resolve {}: {source}", path.display())
            }
            Self::UnsupportedModuleSpecifier { specifier } => write!(
                f,
                "unsupported module specifier `{specifier}`; only local CommonJS modules are supported"
            ),
            Self::Execute(message) => f.write_str(message),
        }
    }
}

impl Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::Runtime;
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn executes_javascript_source() {
        let mut runtime = Runtime::new();

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
    fn reports_syntax_errors_with_message() {
        let mut runtime = Runtime::new();

        let error = runtime
            .execute("function () {")
            .expect_err("invalid JavaScript should fail");

        assert!(error.to_string().contains("SyntaxError"));
    }

    #[test]
    fn executes_javascript_files() {
        let mut runtime = Runtime::new();
        let script_path = unique_temp_directory("file-exec").join("script.js");

        write_file(
            &script_path,
            "const answer = 21 * 2; if (answer !== 42) { throw new Error('wrong answer'); }",
        );

        runtime
            .execute_file(&script_path)
            .expect("runtime should execute a JavaScript file");
    }

    #[test]
    fn resolves_relative_commonjs_modules() {
        let project_dir = unique_temp_directory("commonjs-resolution");
        let nested_dir = project_dir.join("nested");
        fs::create_dir_all(&nested_dir).expect("nested directory should exist");

        write_file(&project_dir.join("answer.js"), "module.exports = 42;");
        write_file(
            &project_dir.join("parent.js"),
            "exports.parent = 'from parent';",
        );
        write_file(
            &nested_dir.join("main.js"),
            r#"
            const answer = require("../answer");
            const parent = require("../parent");

            if (answer !== 42) {
              throw new Error("wrong answer");
            }

            if (parent.parent !== "from parent") {
              throw new Error("wrong parent export");
            }
            "#,
        );

        let mut runtime = Runtime::new();
        runtime
            .execute_file(&nested_dir.join("main.js"))
            .expect("runtime should resolve relative CommonJS modules");
    }

    #[test]
    fn caches_commonjs_modules_by_canonical_path() {
        let project_dir = unique_temp_directory("commonjs-cache");
        write_file(
            &project_dir.join("counter.js"),
            r#"
            globalThis.moduleLoads = (globalThis.moduleLoads ?? 0) + 1;
            module.exports = { count: globalThis.moduleLoads };
            "#,
        );
        write_file(
            &project_dir.join("main.js"),
            r#"
            const first = require("./counter");
            const second = require("./counter.js");

            if (first !== second) {
              throw new Error("cache miss");
            }

            if (first.count !== 1) {
              throw new Error("module executed more than once");
            }
            "#,
        );

        let mut runtime = Runtime::new();
        runtime
            .execute_file(&project_dir.join("main.js"))
            .expect("runtime should cache required modules");
    }

    #[test]
    fn supports_exports_and_module_exports() {
        let project_dir = unique_temp_directory("commonjs-exports");
        write_file(
            &project_dir.join("named.js"),
            "exports.answer = 42; exports.label = 'ok';",
        );
        write_file(&project_dir.join("value.js"), "module.exports = 7;");
        write_file(
            &project_dir.join("main.js"),
            r#"
            const named = require("./named");
            const value = require("./value");

            if (named.answer !== 42 || named.label !== "ok") {
              throw new Error("exports object mismatch");
            }

            if (value !== 7) {
              throw new Error("module.exports mismatch");
            }
            "#,
        );

        let mut runtime = Runtime::new();
        runtime
            .execute_file(&project_dir.join("main.js"))
            .expect("runtime should support CommonJS exports");
    }

    #[test]
    fn executes_zero_delay_timers() {
        let mut runtime = Runtime::new();

        runtime
            .execute(
                r#"
                globalThis.timerEvents = [];
                setTimeout(() => {
                  globalThis.timerEvents.push("timer fired");
                }, 0);
                "#,
            )
            .expect("timer should schedule");

        runtime
            .execute(
                r#"
                if (globalThis.timerEvents.length !== 1 || globalThis.timerEvents[0] !== "timer fired") {
                  throw new Error("timer did not execute");
                }
                "#,
            )
            .expect("timer callback should have run");
    }

    #[test]
    fn clear_timeout_cancels_pending_callbacks() {
        let mut runtime = Runtime::new();

        runtime
            .execute(
                r#"
                globalThis.timerEvents = [];
                const id = setTimeout(() => {
                  globalThis.timerEvents.push("timer fired");
                }, 0);
                clearTimeout(id);
                "#,
            )
            .expect("timer should be cancellable");

        runtime
            .execute(
                r#"
                if (globalThis.timerEvents.length !== 0) {
                  throw new Error("timer should have been cancelled");
                }
                "#,
            )
            .expect("cancelled timer should not fire");
    }

    #[test]
    fn reuses_the_same_runtime_context_across_executions() {
        let mut runtime = Runtime::new();

        runtime
            .execute("globalThis.runtimeReuseCounter = 41;")
            .expect("first execution should set shared state");
        runtime
            .execute(
                r#"
                if (globalThis.runtimeReuseCounter !== 41) {
                  throw new Error("runtime state did not persist");
                }
                globalThis.runtimeReuseCounter += 1;
                "#,
            )
            .expect("second execution should observe shared state");
        runtime
            .execute(
                r#"
                if (globalThis.runtimeReuseCounter !== 42) {
                  throw new Error("runtime state was not updated");
                }
                "#,
            )
            .expect("shared state should be retained");
    }

    #[test]
    fn includes_file_names_in_runtime_errors() {
        let project_dir = unique_temp_directory("error-stack");
        let script_path = project_dir.join("main.js");
        write_file(&script_path, "throw new Error('boom');");

        let mut runtime = Runtime::new();
        let error = runtime
            .execute_file(&script_path)
            .expect_err("script should throw");

        let message = error.to_string();
        assert!(message.contains("Error: boom"));
        assert!(message.contains("main.js"));
    }

    fn unique_temp_directory(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("rust-node-{prefix}-{nanos}"));
        fs::create_dir_all(&path).expect("temporary directory should be created");
        path
    }

    fn write_file(path: &Path, source: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory should be created");
        }
        fs::write(path, source).expect("script should be written");
    }
}

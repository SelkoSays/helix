use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Condvar, Mutex},
};

use once_cell::sync::Lazy;
use steel::{
    rerrs::ErrorKind,
    rvals::IntoSteelVal,
    steel_vm::{builtin::BuiltInModule, engine::Engine, register_fn::RegisterFn},
    SteelErr, SteelVal,
};
use steel_program_linker::{
    CaptureOptions, CompiledObject, CompilerContextId, LoadError, ProgramLoader,
};

use crate::{commands::Context, compositor, ui::PromptEvent};

use super::{steel_init_file, CTX};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationKind {
    Lazy,
    Async,
}

enum ActivationState {
    Unloaded,
    Queued { job_id: u64 },
    Precompiling { job_id: u64 },
    Ready(CompiledObject),
    FallbackPending,
    Activating,
    Loaded,
    Failed(String),
}

struct Plugin {
    modules: Vec<String>,
    initializers: Vec<String>,
    kind: RegistrationKind,
    state: ActivationState,
}

#[derive(Default)]
struct Registry {
    generation: usize,
    plugins: HashMap<String, Plugin>,
    commands: HashMap<String, String>,
    docs: HashMap<String, String>,
    registration_order: Vec<String>,
    initialization_finished: bool,
    next_job_id: u64,
}

struct LoaderState {
    generation: usize,
    loader: Option<ProgramLoader>,
}

static REGISTRY: Lazy<(Mutex<Registry>, Condvar)> =
    Lazy::new(|| (Mutex::new(Registry::default()), Condvar::new()));
static WORKER_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static LOADER: Lazy<Mutex<Option<LoaderState>>> = Lazy::new(|| Mutex::new(None));

const COMPILER_CONTEXT: &str = concat!(
    "helix-term/",
    env!("CARGO_PKG_VERSION"),
    "/steel-lazy-linker-v1"
);

fn compiler_context() -> CompilerContextId {
    CompilerContextId::new(COMPILER_CONTEXT)
}

fn lazy_error(message: impl Into<String>) -> SteelErr {
    SteelErr::new(ErrorKind::Generic, message.into())
}

fn validate_name(kind: &str, name: &str) -> Result<(), SteelErr> {
    if name.is_empty() {
        return Err(lazy_error(format!("lazy plugin {kind} cannot be empty")));
    }
    if name.contains(['\n', '\r', '\0']) {
        return Err(lazy_error(format!(
            "lazy plugin {kind} contains an invalid character: {name:?}"
        )));
    }
    Ok(())
}

fn register_plugin(
    name: String,
    modules: Vec<String>,
    initializers: Vec<String>,
    command_docs: HashMap<String, String>,
    kind: RegistrationKind,
) -> Result<(), SteelErr> {
    validate_name("name", &name)?;
    if modules.is_empty() {
        return Err(lazy_error(format!(
            "lazy plugin {name:?} must register at least one module"
        )));
    }
    if command_docs.is_empty() {
        return Err(lazy_error(format!(
            "lazy plugin {name:?} must register at least one command"
        )));
    }

    for module in &modules {
        validate_name("module", module)?;
    }
    for initializer in &initializers {
        validate_name("initializer", initializer)?;
    }

    let mut commands = command_docs.keys().cloned().collect::<Vec<_>>();
    commands.sort();
    for command in &commands {
        validate_name("command", command)?;
        if command.starts_with(':') {
            return Err(lazy_error(format!(
                "lazy command names must not start with ':': {command:?}"
            )));
        }
        if crate::commands::typed::TYPABLE_COMMAND_MAP.contains_key(command.as_str())
            || super::identifier_available_at_startup(command)
        {
            return Err(lazy_error(format!(
                "lazy command {command:?} collides with a built-in/global command"
            )));
        }
    }

    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if registry.plugins.contains_key(&name) {
        return Err(lazy_error(format!(
            "lazy plugin {name:?} is already registered"
        )));
    }
    if let Some(command) = commands
        .iter()
        .find(|command| registry.commands.contains_key(command.as_str()))
    {
        return Err(lazy_error(format!(
            "lazy command {command:?} is already registered"
        )));
    }

    let queued_job = if kind == RegistrationKind::Async && registry.initialization_finished {
        let job_id = registry.next_job_id;
        registry.next_job_id += 1;
        Some((registry.generation, job_id))
    } else {
        None
    };
    let state = queued_job
        .map(|(_, job_id)| ActivationState::Queued { job_id })
        .unwrap_or(ActivationState::Unloaded);
    for (command, doc) in command_docs {
        registry.commands.insert(command.clone(), name.clone());
        registry.docs.insert(command, doc);
    }
    registry.registration_order.push(name.clone());
    registry.plugins.insert(
        name.clone(),
        Plugin {
            modules,
            initializers,
            kind,
            state,
        },
    );
    condvar.notify_all();
    drop(registry);
    if let Some((generation, job_id)) = queued_job {
        spawn_precompile_queue(generation, vec![(name, job_id)]);
    }
    Ok(())
}

fn register_lazy_plugin(
    name: String,
    modules: Vec<String>,
    initializers: Vec<String>,
    command_docs: HashMap<String, String>,
) -> Result<(), SteelErr> {
    register_plugin(
        name,
        modules,
        initializers,
        command_docs,
        RegistrationKind::Lazy,
    )
}

fn register_async_lazy_plugin(
    name: String,
    modules: Vec<String>,
    initializers: Vec<String>,
    command_docs: HashMap<String, String>,
) -> Result<(), SteelErr> {
    register_plugin(
        name,
        modules,
        initializers,
        command_docs,
        RegistrationKind::Async,
    )
}

fn command_doc(command: String) -> Option<String> {
    documentation(&command)
}

pub(super) fn register_builtin(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/lazy");
    let facade = include_str!("lazy.scm");
    module
        .register_fn("#%register-lazy-plugin!", register_lazy_plugin)
        .register_fn("#%register-async-lazy-plugin!", register_async_lazy_plugin)
        .register_fn("lazy-plugin-command-doc", command_doc);

    if generate_sources {
        super::generate_module("lazy.scm", facade);
        super::configure_lsp_builtins("lazy", &module);
    }
    engine.register_module(module);
    engine.register_steel_module("helix/lazy.scm".to_string(), facade.to_string());
}

pub(super) fn reset(generation: usize) {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    *registry = Registry {
        generation,
        ..Registry::default()
    };
    condvar.notify_all();
    drop(registry);
    *LOADER.lock().unwrap() = None;
}

fn mark_plugin_failed(registry: &mut Registry, plugin_name: &str, message: String) {
    if let Some(plugin) = registry.plugins.get_mut(plugin_name) {
        plugin.state = ActivationState::Failed(message);
    }
}

fn validate_global_collisions(engine: &Engine) {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    let collisions = registry
        .commands
        .iter()
        .filter(|(command, _)| engine.global_exists(command))
        .map(|(command, plugin)| (command.clone(), plugin.clone()))
        .collect::<Vec<_>>();
    for (command, plugin) in collisions {
        mark_plugin_failed(
            &mut registry,
            &plugin,
            format!("lazy command {command:?} collides with an existing global"),
        );
    }
    condvar.notify_all();
}

pub(super) fn finish_initialization(engine: &Engine, generation: usize) {
    validate_global_collisions(engine);

    let loader = match ProgramLoader::from_engine(engine, compiler_context()) {
        Ok(loader) => Some(loader),
        Err(error) => {
            log::warn!("unable to seed lazy-plugin program loader: {error}");
            None
        }
    };
    *LOADER.lock().unwrap() = Some(LoaderState { generation, loader });

    let queued = {
        let (lock, _) = &*REGISTRY;
        let mut registry = lock.lock().unwrap();
        if registry.generation != generation {
            return;
        }
        registry.initialization_finished = true;
        let names = registry.registration_order.clone();
        let mut queued = Vec::new();
        for name in names {
            let should_queue = registry.plugins.get(&name).is_some_and(|plugin| {
                plugin.kind == RegistrationKind::Async
                    && matches!(plugin.state, ActivationState::Unloaded)
            });
            if should_queue {
                let job_id = registry.next_job_id;
                registry.next_job_id += 1;
                registry.plugins.get_mut(&name).unwrap().state = ActivationState::Queued { job_id };
                queued.push((name, job_id));
            }
        }
        queued
    };

    if !queued.is_empty() {
        spawn_precompile_queue(generation, queued);
    }
}

fn spawn_precompile_queue(generation: usize, queued: Vec<(String, u64)>) {
    std::thread::spawn(move || {
        let _worker = WORKER_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (plugin_name, job_id) in queued {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                precompile_one(generation, &plugin_name, job_id)
            }));
            if result.is_err() {
                log::error!("lazy plugin background precompilation panicked for {plugin_name:?}");
                publish_precompile_failure(
                    generation,
                    &plugin_name,
                    job_id,
                    "background precompilation panicked",
                );
            }
        }
    });
}

fn publish_precompile_failure(generation: usize, plugin_name: &str, job_id: u64, message: &str) {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if registry.generation == generation {
        if let Some(plugin) = registry.plugins.get_mut(plugin_name) {
            if matches!(
                plugin.state,
                ActivationState::Queued { job_id: current }
                    | ActivationState::Precompiling { job_id: current }
                    if current == job_id
            ) {
                log::warn!("unable to precompile lazy plugin {plugin_name:?}: {message}");
                plugin.state = ActivationState::FallbackPending;
            }
        }
    }
    condvar.notify_all();
}

fn module_source(modules: &[String]) -> String {
    modules
        .iter()
        .map(|module| format!("(require {module:?})\n"))
        .collect()
}

fn compile_modules(engine: &mut Engine, modules: &[String]) -> Result<CompiledObject, String> {
    let entry_path = steel_init_file();
    let program = engine
        .emit_raw_program(module_source(modules), entry_path.clone())
        .map_err(|error| error.to_string())?;
    CompiledObject::capture(
        engine,
        program,
        CaptureOptions::new(Some(entry_path), compiler_context()),
    )
    .map_err(|error| error.to_string())
}

fn compile_and_run_modules(engine: &mut Engine, modules: &[String]) -> Result<(), SteelErr> {
    engine
        .compile_and_run_raw_program_with_path(module_source(modules), steel_init_file())
        .map(|_| ())
}

fn precompile_one(generation: usize, plugin_name: &str, job_id: u64) {
    let modules = {
        let (lock, condvar) = &*REGISTRY;
        let mut registry = lock.lock().unwrap();
        if registry.generation != generation {
            return;
        }
        let Some(plugin) = registry.plugins.get_mut(plugin_name) else {
            return;
        };
        if !matches!(plugin.state, ActivationState::Queued { job_id: current } if current == job_id)
        {
            return;
        }
        plugin.state = ActivationState::Precompiling { job_id };
        condvar.notify_all();
        plugin.modules.clone()
    };

    // Each artifact must be self-contained. Reusing an engine here would let
    // later artifacts omit modules emitted while compiling earlier plugins.
    let mut engine = super::background_compiler_engine();
    let result = compile_modules(&mut engine, &modules);

    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if registry.generation != generation {
        condvar.notify_all();
        return;
    }
    let Some(plugin) = registry.plugins.get_mut(plugin_name) else {
        condvar.notify_all();
        return;
    };
    if !matches!(plugin.state, ActivationState::Precompiling { job_id: current } if current == job_id)
    {
        condvar.notify_all();
        return;
    }
    plugin.state = match result {
        Ok(object) => ActivationState::Ready(object),
        Err(error) => {
            log::warn!("unable to precompile lazy plugin {plugin_name:?}: {error}");
            ActivationState::FallbackPending
        }
    };
    condvar.notify_all();
}

enum ActivationWork {
    Compile(Vec<String>),
    Precompiled(CompiledObject),
    Call,
}

fn begin_activation(command: &str) -> Result<(String, ActivationWork), SteelErr> {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    let Some(plugin_name) = registry.commands.get(command).cloned() else {
        return Err(lazy_error(format!(
            "lazy command {command:?} disappeared during engine reload"
        )));
    };
    let plugin = registry.plugins.get_mut(&plugin_name).unwrap();
    let work = match &mut plugin.state {
        ActivationState::Unloaded | ActivationState::FallbackPending => {
            let modules = plugin.modules.clone();
            plugin.state = ActivationState::Activating;
            ActivationWork::Compile(modules)
        }
        ActivationState::Ready(_) => {
            let ActivationState::Ready(object) =
                std::mem::replace(&mut plugin.state, ActivationState::Activating)
            else {
                unreachable!()
            };
            ActivationWork::Precompiled(object)
        }
        ActivationState::Loaded => ActivationWork::Call,
        ActivationState::Failed(message) => return Err(lazy_error(message.clone())),
        ActivationState::Activating => {
            return Err(lazy_error(format!(
                "recursive activation of lazy plugin {plugin_name:?}"
            )))
        }
        ActivationState::Queued { .. } | ActivationState::Precompiling { .. } => {
            plugin.state = ActivationState::FallbackPending;
            condvar.notify_all();
            return Err(lazy_error(format!(
                "lazy plugin {plugin_name:?} is still precompiling; invoke {command:?} again to load it normally"
            )));
        }
    };
    Ok((plugin_name, work))
}

enum ActivationCompletion<'a> {
    Loaded,
    FallbackPending,
    Failed(&'a SteelErr),
}

fn finish_activation(plugin_name: &str, completion: ActivationCompletion<'_>) {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if let Some(plugin) = registry.plugins.get_mut(plugin_name) {
        if matches!(plugin.state, ActivationState::Activating) {
            plugin.state = match completion {
                ActivationCompletion::Loaded => ActivationState::Loaded,
                ActivationCompletion::FallbackPending => ActivationState::FallbackPending,
                ActivationCompletion::Failed(error) => ActivationState::Failed(error.to_string()),
            };
        }
    }
    condvar.notify_all();
}

fn callable_value(engine: &Engine, function: &str) -> Result<SteelVal, SteelErr> {
    let mut value = engine.extract_value(function)?;
    for _ in 0..64 {
        match value {
            SteelVal::HeapAllocated(reference) => value = reference.get(),
            _ => return Ok(value),
        }
    }
    Ok(value)
}

fn call_resolved_function_by_name(
    engine: &mut Engine,
    function: &str,
    args: Vec<SteelVal>,
) -> Result<SteelVal, SteelErr> {
    let function = callable_value(engine, function)?;
    engine.call_function_with_args(function, args)
}

enum ArtifactLoadError {
    Safe(String),
    Execution(SteelErr),
}

fn load_precompiled(engine: &mut Engine, object: CompiledObject) -> Result<(), ArtifactLoadError> {
    let generation = REGISTRY.0.lock().unwrap().generation;
    let mut state = LOADER.lock().unwrap();
    if state
        .as_ref()
        .is_none_or(|state| state.generation != generation)
    {
        let loader = ProgramLoader::from_engine(engine, compiler_context())
            .map_err(|error| ArtifactLoadError::Safe(error.to_string()))?;
        *state = Some(LoaderState {
            generation,
            loader: Some(loader),
        });
    }
    let state = state.as_mut().unwrap();
    if state.loader.is_none() {
        state.loader = Some(
            ProgramLoader::from_engine(engine, compiler_context())
                .map_err(|error| ArtifactLoadError::Safe(error.to_string()))?,
        );
    }

    match state.loader.as_mut().unwrap().load_object(engine, object) {
        Ok(_) => Ok(()),
        Err(LoadError::Execution(error)) => {
            log::error!("precompiled lazy-plugin execution failed: {error:?}");
            Err(ArtifactLoadError::Execution(error))
        }
        Err(error) => Err(ArtifactLoadError::Safe(error.to_string())),
    }
}

fn refresh_loader_after_foreground(engine: &Engine) {
    let generation = REGISTRY.0.lock().unwrap().generation;
    let mut state = LOADER.lock().unwrap();
    let result = if let Some(state) = state
        .as_mut()
        .filter(|state| state.generation == generation)
    {
        if let Some(loader) = state.loader.as_mut() {
            loader.refresh_from_engine(engine)
        } else {
            match ProgramLoader::from_engine(engine, compiler_context()) {
                Ok(loader) => {
                    state.loader = Some(loader);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
    } else {
        match ProgramLoader::from_engine(engine, compiler_context()) {
            Ok(loader) => {
                *state = Some(LoaderState {
                    generation,
                    loader: Some(loader),
                });
                Ok(())
            }
            Err(error) => Err(error),
        }
    };
    if let Err(error) = result {
        log::warn!("unable to refresh lazy-plugin program loader: {error}");
        *state = Some(LoaderState {
            generation,
            loader: None,
        });
    }
}

fn activate_and_call(
    engine: &mut Engine,
    command: &str,
    args: Vec<SteelVal>,
) -> Result<SteelVal, SteelErr> {
    let (plugin_name, work) = begin_activation(command)?;
    let activation = !matches!(work, ActivationWork::Call);

    if activation {
        let load_result = match work {
            ActivationWork::Compile(modules) => {
                let result = compile_and_run_modules(engine, &modules);
                if result.is_ok() {
                    refresh_loader_after_foreground(engine);
                }
                result
            }
            ActivationWork::Precompiled(object) => match load_precompiled(engine, object) {
                Ok(()) => Ok(()),
                Err(ArtifactLoadError::Execution(error)) => Err(error),
                Err(ArtifactLoadError::Safe(message)) => {
                    finish_activation(&plugin_name, ActivationCompletion::FallbackPending);
                    return Err(lazy_error(format!(
                        "unable to load precompiled lazy plugin {plugin_name:?}: {message}; invoke {command:?} again to load it normally"
                    )));
                }
            },
            ActivationWork::Call => unreachable!(),
        };

        if let Err(error) = load_result {
            finish_activation(&plugin_name, ActivationCompletion::Failed(&error));
            return Err(error);
        }

        let initialization_result = (|| {
            let initializers = {
                let (lock, _) = &*REGISTRY;
                let registry = lock.lock().unwrap();
                registry
                    .plugins
                    .get(&plugin_name)
                    .map(|plugin| plugin.initializers.clone())
                    .ok_or_else(|| {
                        lazy_error(format!(
                            "lazy plugin {plugin_name:?} disappeared during engine reload"
                        ))
                    })?
            };
            for initializer in initializers {
                call_resolved_function_by_name(engine, &initializer, Vec::new())?;
            }
            Ok(())
        })();

        if let Err(error) = initialization_result {
            finish_activation(&plugin_name, ActivationCompletion::Failed(&error));
            return Err(error);
        }
        finish_activation(&plugin_name, ActivationCompletion::Loaded);
    }

    call_resolved_function_by_name(engine, command, args)
}

fn set_command_result(cx: &mut Context, result: &SteelVal) {
    match result {
        SteelVal::Void => {}
        SteelVal::StringV(value) => cx.editor.set_status(value.as_str().to_owned()),
        value => cx.editor.set_status(value.to_string()),
    }
}

fn invoke(cx: &mut Context, command: &str, args: Vec<SteelVal>) -> Result<SteelVal, SteelErr> {
    super::enter_engine(|engine| {
        super::with_interrupt_handler(|| {
            engine
                .with_mut_reference::<Context, Context>(cx)
                .consume_once(move |engine, arguments| {
                    let context = arguments.into_iter().next().unwrap();
                    engine.update_value(CTX, context);
                    activate_and_call(engine, command, args)
                })
        })
    })
}

pub(super) fn call_function_by_name(cx: &mut Context, name: &str, args: &[Cow<'_, str>]) -> bool {
    if !recognizes(name) {
        return false;
    }
    let args = args
        .iter()
        .map(|arg| arg.clone().into_steelval().unwrap())
        .collect();
    match invoke(cx, name, args) {
        Ok(result) => set_command_result(cx, &result),
        Err(error) => super::enter_engine(|engine| {
            super::present_error_inside_engine_context(cx, engine, error)
        }),
    }
    true
}

pub(super) fn call_typed_command(
    cx: &mut compositor::Context,
    command: &str,
    parts: &[&str],
    event: PromptEvent,
) -> bool {
    if !recognizes(command) {
        return false;
    }
    if event != PromptEvent::Validate {
        return true;
    }

    let args = parts
        .iter()
        .map(|arg| arg.into_steelval().unwrap())
        .collect();
    let mut context = super::with_context_guard(cx);
    match invoke(&mut context, command, args) {
        Ok(result) => set_command_result(&mut context, &result),
        Err(error) => super::enter_engine(|engine| {
            super::present_error_inside_engine_context(&mut context, engine, error)
        }),
    }
    true
}

pub(super) fn recognizes(command: &str) -> bool {
    REGISTRY.0.lock().unwrap().commands.contains_key(command)
}

pub(super) fn documentation(command: &str) -> Option<String> {
    REGISTRY.0.lock().unwrap().docs.get(command).cloned()
}

pub(super) fn available_commands<'a>() -> Vec<Cow<'a, str>> {
    let mut commands = REGISTRY
        .0
        .lock()
        .unwrap()
        .commands
        .keys()
        .cloned()
        .map(Cow::<'a, str>::Owned)
        .collect::<Vec<_>>();
    commands.sort_unstable_by(|left, right| left.as_ref().cmp(right.as_ref()));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn docs(command: &str) -> HashMap<String, String> {
        HashMap::from([(command.to_string(), format!("Documentation for {command}"))])
    }

    fn reset_test_registry() {
        reset(1);
    }

    fn set_ready_plugin(plugin: &str, object: CompiledObject) {
        REGISTRY
            .0
            .lock()
            .unwrap()
            .plugins
            .get_mut(plugin)
            .unwrap()
            .state = ActivationState::Ready(object);
    }

    fn register_shared_load_counter(engine: &mut Engine, counter: Arc<AtomicUsize>) {
        engine.register_fn("lazy-test-shared-loaded!", move || {
            counter.fetch_add(1, Ordering::SeqCst);
            SteelVal::Void
        });
    }

    fn register_prefixed_builtin(engine: &mut Engine) {
        let mut module = BuiltInModule::new("lazy-test/prefixed-builtin");
        module.register_fn("builtin-target", || "builtin-target");
        engine.register_module(module);
    }

    #[test]
    fn completion_and_docs_exist_before_loading() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        register_lazy_plugin(
            "test".into(),
            vec!["test/module.scm".into()],
            vec!["test-init!".into()],
            docs("test-command"),
        )
        .unwrap();
        assert!(recognizes("test-command"));
        assert_eq!(
            documentation("test-command").as_deref(),
            Some("Documentation for test-command")
        );
        assert_eq!(available_commands(), vec![Cow::Borrowed("test-command")]);
    }

    #[test]
    fn builtin_source_setup_can_load_lazy_dependent_modules() {
        let _test = TEST_LOCK.lock().unwrap();
        let mut engine = Engine::new();
        engine.register_value(super::super::CTX, SteelVal::Void);
        engine.register_value(super::super::CONFIG, SteelVal::Void);

        super::super::configure_builtin_sources(&mut engine, false);

        assert!(engine.builtin_modules().get("helix/core/lazy").is_some());
        engine
            .compile_and_run_raw_program(
                "(require \"helix/lazy.scm\")\n(require \"helix/keymaps.scm\")",
            )
            .unwrap();
    }

    #[test]
    fn duplicate_plugin_command_and_builtin_collisions_are_rejected() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        register_lazy_plugin(
            "one".into(),
            vec!["one.scm".into()],
            Vec::new(),
            docs("one-command"),
        )
        .unwrap();
        assert!(register_lazy_plugin(
            "one".into(),
            vec!["two.scm".into()],
            Vec::new(),
            docs("two-command"),
        )
        .is_err());
        assert!(register_lazy_plugin(
            "two".into(),
            vec!["two.scm".into()],
            Vec::new(),
            docs("one-command"),
        )
        .is_err());
        assert!(register_lazy_plugin(
            "builtin".into(),
            vec!["builtin.scm".into()],
            Vec::new(),
            docs("quit"),
        )
        .is_err());
    }

    #[test]
    fn recursive_activation_and_failures_are_retained_until_reset() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        register_lazy_plugin(
            "test".into(),
            vec!["test.scm".into()],
            Vec::new(),
            docs("test-command"),
        )
        .unwrap();
        let (plugin, _) = begin_activation("test-command").unwrap();
        assert!(begin_activation("test-command")
            .err()
            .unwrap()
            .to_string()
            .contains("recursive activation"));
        let failure = lazy_error("remember me");
        finish_activation(&plugin, ActivationCompletion::Failed(&failure));
        assert!(begin_activation("test-command")
            .err()
            .unwrap()
            .to_string()
            .contains("remember me"));
        reset(2);
        assert!(!recognizes("test-command"));
    }

    #[test]
    fn precompilation_does_not_evaluate_top_level_code() {
        let _test = TEST_LOCK.lock().unwrap();
        let mut foreground = Engine::new();
        foreground.register_steel_module(
            "lazy-test/module.scm".into(),
            "(provide lazy-test-value) (define lazy-test-value 42)".into(),
        );
        let mut worker = Engine::new();
        worker.register_steel_module(
            "lazy-test/module.scm".into(),
            "(provide lazy-test-value) (define lazy-test-value 42)".into(),
        );
        compile_modules(&mut worker, &["lazy-test/module.scm".into()]).unwrap();
        assert!(!foreground.global_exists("lazy-test-value"));
        compile_and_run_modules(&mut foreground, &["lazy-test/module.scm".into()]).unwrap();
        assert!(foreground.global_exists("lazy-test-value"));
    }

    #[test]
    fn arguments_initializer_order_and_load_once() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        engine.register_steel_module(
            "lazy-test/order.scm".into(),
            r#"
                (provide lazy-order-command lazy-order-init!)
                (define lazy-order-state "")
                (define (lazy-order-init!)
                  (set! lazy-order-state (string-append lazy-order-state "init:")))
                (define (lazy-order-command arg)
                  (set! lazy-order-state (string-append lazy-order-state arg))
                  lazy-order-state)
            "#
            .into(),
        );
        register_lazy_plugin(
            "order".into(),
            vec!["lazy-test/order.scm".into()],
            vec!["lazy-order-init!".into()],
            docs("lazy-order-command"),
        )
        .unwrap();

        let first = activate_and_call(
            &mut engine,
            "lazy-order-command",
            vec!["one".into_steelval().unwrap()],
        )
        .unwrap();
        let second = activate_and_call(
            &mut engine,
            "lazy-order-command",
            vec!["two".into_steelval().unwrap()],
        )
        .unwrap();
        assert_eq!(first.to_string(), "\"init:one\"");
        assert_eq!(second.to_string(), "\"init:onetwo\"");
    }

    #[test]
    fn global_collisions_and_stale_workers_fail_safely() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        register_lazy_plugin(
            "collision".into(),
            vec!["collision.scm".into()],
            Vec::new(),
            docs("collision-command"),
        )
        .unwrap();
        let mut engine = Engine::new();
        engine.run("(define (collision-command) 10)").unwrap();
        validate_global_collisions(&engine);
        assert!(begin_activation("collision-command")
            .err()
            .unwrap()
            .to_string()
            .contains("existing global"));

        reset(2);
        register_lazy_plugin(
            "stale".into(),
            vec!["stale.scm".into()],
            Vec::new(),
            docs("stale-command"),
        )
        .unwrap();
        precompile_one(1, "stale", 99);
        let registry = REGISTRY.0.lock().unwrap();
        assert!(matches!(
            registry.plugins.get("stale").unwrap().state,
            ActivationState::Unloaded
        ));
    }

    #[test]
    fn sequential_background_precompilation_keeps_foreground_usable() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let directory = tempfile::tempdir().unwrap();
        let dependency_path = directory.path().join("async-dependency.scm");
        let first_path = directory.path().join("async-one.scm");
        let second_path = directory.path().join("async-two.scm");
        std::fs::write(
            &dependency_path,
            "(provide async-dependency-value) (define async-dependency-value 1)",
        )
        .unwrap();
        std::fs::write(
            &first_path,
            format!(
                r#"
                    (require (prefix-in dependency. {:?}))
                    (provide async-one-command)
                    (define (async-one-command) dependency.async-dependency-value)
                "#,
                dependency_path.to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(
            &second_path,
            "(provide async-two-command) (define (async-two-command) 2)",
        )
        .unwrap();

        let mut foreground = Engine::new();
        register_async_lazy_plugin(
            "async-one".into(),
            vec![
                first_path.to_string_lossy().into_owned(),
                dependency_path.to_string_lossy().into_owned(),
            ],
            Vec::new(),
            docs("async-one-command"),
        )
        .unwrap();
        register_async_lazy_plugin(
            "async-two".into(),
            vec![second_path.to_string_lossy().into_owned()],
            Vec::new(),
            docs("async-two-command"),
        )
        .unwrap();

        finish_initialization(&foreground, 1);
        foreground
            .run("(define foreground-still-usable 99)")
            .unwrap();
        assert!(foreground.global_exists("foreground-still-usable"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let (lock, condvar) = &*REGISTRY;
        let mut registry = lock.lock().unwrap();
        while !["async-one", "async-two"].iter().all(|name| {
            matches!(
                registry.plugins.get(*name).unwrap().state,
                ActivationState::Ready(_)
                    | ActivationState::FallbackPending
                    | ActivationState::Failed(_)
            )
        }) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "background precompilation timed out");
            (registry, _) = condvar.wait_timeout(registry, remaining).unwrap();
        }
        assert!(["async-one", "async-two"].iter().all(|name| matches!(
            registry.plugins.get(*name).unwrap().state,
            ActivationState::Ready(_)
        )));
        drop(registry);

        let first = activate_and_call(&mut foreground, "async-one-command", Vec::new()).unwrap();
        let second = activate_and_call(&mut foreground, "async-two-command", Vec::new()).unwrap();
        assert_eq!(first, SteelVal::IntV(1));
        assert_eq!(second, SteelVal::IntV(2));
    }

    #[test]
    fn foreground_activation_failures_are_retained() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        register_lazy_plugin(
            "retained".into(),
            vec!["lazy-test/retained.scm".into()],
            Vec::new(),
            docs("retained-command"),
        )
        .unwrap();
        let error = activate_and_call(&mut engine, "retained-command", Vec::new())
            .unwrap_err()
            .to_string();
        assert!(begin_activation("retained-command")
            .err()
            .unwrap()
            .to_string()
            .contains(&error));
    }

    #[test]
    fn precompile_failure_retries_during_foreground_activation() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        register_async_lazy_plugin(
            "retry".into(),
            vec!["lazy-test/retry.scm".into()],
            Vec::new(),
            HashMap::from([
                ("retry-first".into(), "First retry command".into()),
                ("retry-second".into(), "Second retry command".into()),
            ]),
        )
        .unwrap();
        {
            let mut registry = REGISTRY.0.lock().unwrap();
            registry.plugins.get_mut("retry").unwrap().state =
                ActivationState::Queued { job_id: 7 };
        }

        precompile_one(1, "retry", 7);
        assert!(matches!(
            REGISTRY
                .0
                .lock()
                .unwrap()
                .plugins
                .get("retry")
                .unwrap()
                .state,
            ActivationState::FallbackPending
        ));

        engine.register_steel_module(
            "lazy-test/retry.scm".into(),
            r#"
                (provide retry-first retry-second)
                (define (retry-first) 10)
                (define (retry-second) 20)
            "#
            .into(),
        );
        assert_eq!(
            activate_and_call(&mut engine, "retry-first", Vec::new()).unwrap(),
            SteelVal::IntV(10)
        );
        assert_eq!(
            activate_and_call(&mut engine, "retry-second", Vec::new()).unwrap(),
            SteelVal::IntV(20)
        );
    }

    #[test]
    fn queued_plugin_can_activate_before_precompilation_starts() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        engine.register_steel_module(
            "lazy-test/early.scm".into(),
            r#"
                (provide early-first early-second)
                (define (early-first value) value)
                (define (early-second) 2)
            "#
            .into(),
        );
        register_async_lazy_plugin(
            "early".into(),
            vec!["lazy-test/early.scm".into()],
            Vec::new(),
            HashMap::from([
                ("early-first".into(), "First early command".into()),
                ("early-second".into(), "Second early command".into()),
            ]),
        )
        .unwrap();
        {
            let mut registry = REGISTRY.0.lock().unwrap();
            registry.plugins.get_mut("early").unwrap().state =
                ActivationState::Queued { job_id: 8 };
        }

        assert!(
            activate_and_call(&mut engine, "early-first", vec![SteelVal::IntV(17)])
                .unwrap_err()
                .to_string()
                .contains("invoke \"early-first\" again")
        );
        assert_eq!(
            activate_and_call(&mut engine, "early-first", vec![SteelVal::IntV(17)]).unwrap(),
            SteelVal::IntV(17)
        );
        assert_eq!(
            activate_and_call(&mut engine, "early-second", Vec::new()).unwrap(),
            SteelVal::IntV(2)
        );
        assert!(matches!(
            REGISTRY
                .0
                .lock()
                .unwrap()
                .plugins
                .get("early")
                .unwrap()
                .state,
            ActivationState::Loaded
        ));
    }

    #[test]
    fn ready_plugin_does_not_wait_for_unrelated_precompilation() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut worker = Engine::new();
        worker.register_steel_module(
            "lazy-test/barrier.scm".into(),
            "(provide barrier-command) (define (barrier-command) 1)".into(),
        );
        let object = compile_modules(&mut worker, &["lazy-test/barrier.scm".into()]).unwrap();
        register_async_lazy_plugin(
            "barrier".into(),
            vec!["lazy-test/barrier.scm".into()],
            Vec::new(),
            docs("barrier-command"),
        )
        .unwrap();
        register_async_lazy_plugin(
            "unrelated".into(),
            vec!["lazy-test/unrelated.scm".into()],
            Vec::new(),
            docs("unrelated-command"),
        )
        .unwrap();
        {
            let mut registry = REGISTRY.0.lock().unwrap();
            registry.plugins.get_mut("barrier").unwrap().state = ActivationState::Ready(object);
            registry.plugins.get_mut("unrelated").unwrap().state =
                ActivationState::Precompiling { job_id: 9 };
        }

        assert!(matches!(
            begin_activation("barrier-command").unwrap().1,
            ActivationWork::Precompiled(_)
        ));
        finish_activation("barrier", ActivationCompletion::Loaded);
    }

    #[test]
    fn panicked_worker_only_falls_back_its_own_plugin() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        register_async_lazy_plugin(
            "worker-panic".into(),
            vec!["lazy-test/worker-panic.scm".into()],
            Vec::new(),
            docs("worker-panic-command"),
        )
        .unwrap();
        {
            let mut registry = REGISTRY.0.lock().unwrap();
            registry.plugins.get_mut("worker-panic").unwrap().state =
                ActivationState::Precompiling { job_id: 10 };
        }

        publish_precompile_failure(1, "worker-panic", 10, "worker panicked");

        let registry = REGISTRY.0.lock().unwrap();
        assert!(matches!(
            registry.plugins.get("worker-panic").unwrap().state,
            ActivationState::FallbackPending
        ));
        drop(registry);
        assert!(matches!(
            begin_activation("worker-panic-command").unwrap().1,
            ActivationWork::Compile(_)
        ));
        let failure = lazy_error("expected foreground failure");
        finish_activation("worker-panic", ActivationCompletion::Failed(&failure));
    }

    #[test]
    fn command_and_initializer_aliases_to_prefixed_imports_are_callable() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        engine.register_steel_module(
            "lazy-test/alias-target.scm".into(),
            r#"
                (provide alias-target-command alias-target-init!)
                (define alias-target-state "cold")
                (define (alias-target-init!)
                  (set! alias-target-state "ready"))
                (define (alias-target-command suffix)
                  (string-append alias-target-state suffix))
            "#
            .into(),
        );
        engine.register_steel_module(
            "lazy-test/alias-wrapper.scm".into(),
            r#"
                (require (prefix-in target. "lazy-test/alias-target.scm"))
                (provide alias-command alias-init!)
                (define alias-command target.alias-target-command)
                (define alias-init! target.alias-target-init!)
            "#
            .into(),
        );
        register_lazy_plugin(
            "alias".into(),
            vec![
                "lazy-test/alias-target.scm".into(),
                "lazy-test/alias-wrapper.scm".into(),
            ],
            vec!["alias-init!".into()],
            docs("alias-command"),
        )
        .unwrap();

        let first = activate_and_call(
            &mut engine,
            "alias-command",
            vec!["!".into_steelval().unwrap()],
        )
        .unwrap();
        let second = activate_and_call(
            &mut engine,
            "alias-command",
            vec!["?".into_steelval().unwrap()],
        )
        .unwrap();

        assert_eq!(first.to_string(), "\"ready!\"");
        assert_eq!(second.to_string(), "\"ready?\"");
    }

    #[test]
    fn command_bodies_can_call_prefixed_imports_after_lazy_activation() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        engine.register_steel_module(
            "lazy-test/prefixed-target.scm".into(),
            r#"
                (provide prefixed-target)
                (define (prefixed-target suffix)
                  (string-append "target:" suffix))
            "#
            .into(),
        );
        engine.register_steel_module(
            "lazy-test/prefixed-caller.scm".into(),
            r#"
                (require (prefix-in target. "lazy-test/prefixed-target.scm"))
                (provide prefixed-command prefixed-init!)
                (define prefixed-state "cold")
                (define (prefixed-init!)
                  (set! prefixed-state (target.prefixed-target "init")))
                (define (prefixed-command suffix)
                  (string-append prefixed-state ":" (target.prefixed-target suffix)))
            "#
            .into(),
        );
        register_lazy_plugin(
            "prefixed".into(),
            vec![
                "lazy-test/prefixed-target.scm".into(),
                "lazy-test/prefixed-caller.scm".into(),
            ],
            vec!["prefixed-init!".into()],
            docs("prefixed-command"),
        )
        .unwrap();

        let result = activate_and_call(
            &mut engine,
            "prefixed-command",
            vec!["run".into_steelval().unwrap()],
        )
        .unwrap();

        assert_eq!(result.to_string(), "\"target:init:target:run\"");
    }

    #[test]
    fn ready_artifact_loads_without_foreground_module_sources() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let base = "lazy-test/source-free-base.scm".to_string();
        let command = "lazy-test/source-free-command.scm".to_string();
        let mut worker = Engine::new();
        worker.register_steel_module(
            base.clone(),
            "(provide source-free-base) (define source-free-base 40)".into(),
        );
        worker.register_steel_module(
            command.clone(),
            r#"
                (require "lazy-test/source-free-base.scm")
                (provide source-free-command)
                (define (source-free-command value) (+ source-free-base value))
            "#
            .into(),
        );
        let object = compile_modules(&mut worker, &[base.clone(), command.clone()]).unwrap();
        let mut foreground = Engine::new();
        register_async_lazy_plugin(
            "source-free".into(),
            vec![base, command],
            Vec::new(),
            docs("source-free-command"),
        )
        .unwrap();
        set_ready_plugin("source-free", object);

        assert_eq!(
            activate_and_call(
                &mut foreground,
                "source-free-command",
                vec![SteelVal::IntV(2)]
            )
            .unwrap(),
            SteelVal::IntV(42)
        );
    }

    #[test]
    fn ready_artifact_command_bodies_can_call_prefixed_imports() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let target = "lazy-test/precompiled-prefixed-target.scm".to_string();
        let caller = "lazy-test/precompiled-prefixed-caller.scm".to_string();
        let mut worker = Engine::new();
        worker.register_steel_module(
            target.clone(),
            r#"
                (provide precompiled-prefixed-target)
                (define (precompiled-prefixed-target suffix)
                  (string-append "target:" suffix))
            "#
            .into(),
        );
        worker.register_steel_module(
            caller.clone(),
            r#"
                (require (prefix-in target. "lazy-test/precompiled-prefixed-target.scm"))
                (provide precompiled-prefixed-command)
                (define (precompiled-prefixed-command suffix)
                  (target.precompiled-prefixed-target suffix))
            "#
            .into(),
        );
        let object = compile_modules(&mut worker, &[target.clone(), caller.clone()]).unwrap();
        let mut foreground = Engine::new();
        register_async_lazy_plugin(
            "precompiled-prefixed".into(),
            vec![target, caller],
            Vec::new(),
            docs("precompiled-prefixed-command"),
        )
        .unwrap();
        set_ready_plugin("precompiled-prefixed", object);

        assert_eq!(
            activate_and_call(
                &mut foreground,
                "precompiled-prefixed-command",
                vec!["run".into_steelval().unwrap()],
            )
            .unwrap(),
            SteelVal::StringV("target:run".into())
        );
    }

    #[test]
    fn ready_artifact_keeps_transitive_prefixed_dependencies_source_free() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let target = "lazy-test/transitive-prefixed-target.scm".to_string();
        let caller = "lazy-test/transitive-prefixed-caller.scm".to_string();
        let mut worker = Engine::new();
        worker.register_steel_module(
            target.clone(),
            r#"
                (provide transitive-prefixed-target)
                (define (transitive-prefixed-target suffix)
                  (string-append "target:" suffix))
            "#
            .into(),
        );
        worker.register_steel_module(
            caller.clone(),
            r#"
                (require (prefix-in target. "lazy-test/transitive-prefixed-target.scm"))
                (provide transitive-prefixed-command)
                (define (transitive-prefixed-command suffix)
                  (target.transitive-prefixed-target suffix))
            "#
            .into(),
        );
        let object = compile_modules(&mut worker, std::slice::from_ref(&caller)).unwrap();
        let mut foreground = Engine::new();
        register_async_lazy_plugin(
            "transitive-prefixed".into(),
            vec![caller],
            Vec::new(),
            docs("transitive-prefixed-command"),
        )
        .unwrap();
        set_ready_plugin("transitive-prefixed", object);

        assert_eq!(
            activate_and_call(
                &mut foreground,
                "transitive-prefixed-command",
                vec!["run".into_steelval().unwrap()],
            )
            .unwrap(),
            SteelVal::StringV("target:run".into())
        );
    }

    #[test]
    fn command_bodies_can_call_prefixed_builtins_after_lazy_activation() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        register_prefixed_builtin(&mut engine);
        engine.register_steel_module(
            "lazy-test/prefixed-builtin-caller.scm".into(),
            r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide prefixed-builtin-command)
                (define (prefixed-builtin-command)
                  (builtin.builtin-target))
            "#
            .into(),
        );
        register_lazy_plugin(
            "prefixed-builtin".into(),
            vec!["lazy-test/prefixed-builtin-caller.scm".into()],
            Vec::new(),
            docs("prefixed-builtin-command"),
        )
        .unwrap();

        assert_eq!(
            activate_and_call(&mut engine, "prefixed-builtin-command", Vec::new()).unwrap(),
            SteelVal::StringV("builtin-target".into())
        );
    }

    #[test]
    fn ready_artifact_command_bodies_can_call_prefixed_builtins() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let module = "lazy-test/precompiled-prefixed-builtin-caller.scm".to_string();
        let mut worker = Engine::new();
        register_prefixed_builtin(&mut worker);
        worker.register_steel_module(
            module.clone(),
            r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide precompiled-prefixed-builtin-command)
                (define (precompiled-prefixed-builtin-command)
                  (builtin.builtin-target))
            "#
            .into(),
        );
        let object = compile_modules(&mut worker, std::slice::from_ref(&module)).unwrap();
        let mut foreground = Engine::new();
        register_prefixed_builtin(&mut foreground);
        register_async_lazy_plugin(
            "precompiled-prefixed-builtin".into(),
            vec![module],
            Vec::new(),
            docs("precompiled-prefixed-builtin-command"),
        )
        .unwrap();
        set_ready_plugin("precompiled-prefixed-builtin", object);

        assert_eq!(
            activate_and_call(
                &mut foreground,
                "precompiled-prefixed-builtin-command",
                Vec::new(),
            )
            .unwrap(),
            SteelVal::StringV("builtin-target".into())
        );
    }

    #[test]
    fn command_bodies_can_apply_prefixed_builtin_function_values() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        register_prefixed_builtin(&mut engine);
        engine.register_steel_module(
            "lazy-test/prefixed-builtin-value-caller.scm".into(),
            r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide prefixed-builtin-value-command)
                (define (call-action action)
                  (action))
                (define (prefixed-builtin-value-command)
                  (call-action builtin.builtin-target))
            "#
            .into(),
        );
        register_lazy_plugin(
            "prefixed-builtin-value".into(),
            vec!["lazy-test/prefixed-builtin-value-caller.scm".into()],
            Vec::new(),
            docs("prefixed-builtin-value-command"),
        )
        .unwrap();

        assert_eq!(
            activate_and_call(&mut engine, "prefixed-builtin-value-command", Vec::new()).unwrap(),
            SteelVal::StringV("builtin-target".into())
        );
    }

    #[test]
    fn ready_artifact_command_bodies_can_apply_prefixed_builtin_function_values() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let module = "lazy-test/precompiled-prefixed-builtin-value-caller.scm".to_string();
        let mut worker = Engine::new();
        register_prefixed_builtin(&mut worker);
        worker.register_steel_module(
            module.clone(),
            r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide precompiled-prefixed-builtin-value-command)
                (define (call-action action)
                  (action))
                (define (precompiled-prefixed-builtin-value-command)
                  (call-action builtin.builtin-target))
            "#
            .into(),
        );
        let object = compile_modules(&mut worker, std::slice::from_ref(&module)).unwrap();
        let mut foreground = Engine::new();
        register_prefixed_builtin(&mut foreground);
        register_async_lazy_plugin(
            "precompiled-prefixed-builtin-value".into(),
            vec![module],
            Vec::new(),
            docs("precompiled-prefixed-builtin-value-command"),
        )
        .unwrap();
        set_ready_plugin("precompiled-prefixed-builtin-value", object);

        assert_eq!(
            activate_and_call(
                &mut foreground,
                "precompiled-prefixed-builtin-value-command",
                Vec::new(),
            )
            .unwrap(),
            SteelVal::StringV("builtin-target".into())
        );
    }

    #[test]
    fn precompiled_artifacts_load_a_shared_dependency_once() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let shared = "lazy-test/shared-once.scm".to_string();
        let first = "lazy-test/shared-once-first.scm".to_string();
        let second = "lazy-test/shared-once-second.scm".to_string();
        let shared_source = r#"
            (provide shared-once-value)
            (lazy-test-shared-loaded!)
            (define shared-once-value 7)
        "#;
        let first_source = r#"
            (require "lazy-test/shared-once.scm")
            (provide shared-once-first)
            (define (shared-once-first) shared-once-value)
        "#;
        let second_source = r#"
            (require "lazy-test/shared-once.scm")
            (provide shared-once-second)
            (define (shared-once-second) (+ shared-once-value 1))
        "#;

        let mut first_worker = Engine::new();
        register_shared_load_counter(&mut first_worker, Arc::new(AtomicUsize::new(0)));
        first_worker.register_steel_module(shared.clone(), shared_source.into());
        first_worker.register_steel_module(first.clone(), first_source.into());
        let first_object =
            compile_modules(&mut first_worker, &[shared.clone(), first.clone()]).unwrap();

        let mut second_worker = Engine::new();
        register_shared_load_counter(&mut second_worker, Arc::new(AtomicUsize::new(0)));
        second_worker.register_steel_module(shared.clone(), shared_source.into());
        second_worker.register_steel_module(second.clone(), second_source.into());
        let second_object =
            compile_modules(&mut second_worker, &[shared.clone(), second.clone()]).unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let mut foreground = Engine::new();
        register_shared_load_counter(&mut foreground, count.clone());
        register_async_lazy_plugin(
            "shared-first".into(),
            vec![shared.clone(), first],
            Vec::new(),
            docs("shared-once-first"),
        )
        .unwrap();
        register_async_lazy_plugin(
            "shared-second".into(),
            vec![shared, second],
            Vec::new(),
            docs("shared-once-second"),
        )
        .unwrap();
        set_ready_plugin("shared-first", first_object);
        set_ready_plugin("shared-second", second_object);

        assert_eq!(
            activate_and_call(&mut foreground, "shared-once-first", Vec::new()).unwrap(),
            SteelVal::IntV(7)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            activate_and_call(&mut foreground, "shared-once-second", Vec::new()).unwrap(),
            SteelVal::IntV(8)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn foreground_fallback_refresh_prevents_later_precompiled_shared_reload() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let shared = "lazy-test/refresh-shared.scm".to_string();
        let foreground_module = "lazy-test/refresh-foreground.scm".to_string();
        let precompiled_module = "lazy-test/refresh-precompiled.scm".to_string();
        let shared_source = r#"
            (provide refresh-shared-value)
            (lazy-test-shared-loaded!)
            (define refresh-shared-value 3)
        "#;
        let foreground_source = r#"
            (require "lazy-test/refresh-shared.scm")
            (provide refresh-foreground-command)
            (define (refresh-foreground-command) refresh-shared-value)
        "#;
        let precompiled_source = r#"
            (require "lazy-test/refresh-shared.scm")
            (provide refresh-precompiled-command)
            (define (refresh-precompiled-command) (+ refresh-shared-value 4))
        "#;

        let count = Arc::new(AtomicUsize::new(0));
        let mut foreground = Engine::new();
        register_shared_load_counter(&mut foreground, count.clone());
        foreground.register_steel_module(shared.clone(), shared_source.into());
        foreground.register_steel_module(foreground_module.clone(), foreground_source.into());

        let mut worker = Engine::new();
        register_shared_load_counter(&mut worker, Arc::new(AtomicUsize::new(0)));
        worker.register_steel_module(shared.clone(), shared_source.into());
        worker.register_steel_module(precompiled_module.clone(), precompiled_source.into());
        let object =
            compile_modules(&mut worker, &[shared.clone(), precompiled_module.clone()]).unwrap();

        register_lazy_plugin(
            "refresh-foreground".into(),
            vec![shared.clone(), foreground_module],
            Vec::new(),
            docs("refresh-foreground-command"),
        )
        .unwrap();
        register_async_lazy_plugin(
            "refresh-precompiled".into(),
            vec![shared, precompiled_module],
            Vec::new(),
            docs("refresh-precompiled-command"),
        )
        .unwrap();
        set_ready_plugin("refresh-precompiled", object);

        assert_eq!(
            activate_and_call(&mut foreground, "refresh-foreground-command", Vec::new()).unwrap(),
            SteelVal::IntV(3)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            activate_and_call(&mut foreground, "refresh-precompiled-command", Vec::new()).unwrap(),
            SteelVal::IntV(7)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn precompiled_module_execution_failures_are_retained() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut worker = Engine::new();
        worker.register_steel_module(
            "lazy-test/module-fail.scm".into(),
            r#"
                (provide module-fail-command)
                (error "module execution failed")
                (define (module-fail-command) 1)
            "#
            .into(),
        );
        let object = compile_modules(&mut worker, &["lazy-test/module-fail.scm".into()]).unwrap();
        let mut foreground = Engine::new();
        register_async_lazy_plugin(
            "module-fail".into(),
            vec!["lazy-test/module-fail.scm".into()],
            Vec::new(),
            docs("module-fail-command"),
        )
        .unwrap();
        set_ready_plugin("module-fail", object);

        let error = activate_and_call(&mut foreground, "module-fail-command", Vec::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("module execution failed"));
        assert!(begin_activation("module-fail-command")
            .err()
            .unwrap()
            .to_string()
            .contains("module execution failed"));
    }

    #[test]
    fn initializer_failures_are_retained() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut worker = Engine::new();
        worker.register_steel_module(
            "lazy-test/init-fail.scm".into(),
            r#"
                (provide init-fail-command init-fail-init!)
                (define (init-fail-init!) (error "initializer failed"))
                (define (init-fail-command) 1)
            "#
            .into(),
        );
        let object = compile_modules(&mut worker, &["lazy-test/init-fail.scm".into()]).unwrap();
        let mut foreground = Engine::new();
        register_async_lazy_plugin(
            "init-fail".into(),
            vec!["lazy-test/init-fail.scm".into()],
            vec!["init-fail-init!".into()],
            docs("init-fail-command"),
        )
        .unwrap();
        set_ready_plugin("init-fail", object);

        let error = activate_and_call(&mut foreground, "init-fail-command", Vec::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("initializer failed"));
        assert!(begin_activation("init-fail-command")
            .err()
            .unwrap()
            .to_string()
            .contains("initializer failed"));
    }
}

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

use crate::{commands::Context, compositor, ui::PromptEvent};

use super::{steel_init_file, CTX};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationKind {
    Lazy,
    Async,
}

enum ActivationState {
    Unloaded,
    Queued,
    Precompiling,
    Ready,
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
    initialization_finished: bool,
    worker_active: bool,
}

static REGISTRY: Lazy<(Mutex<Registry>, Condvar)> =
    Lazy::new(|| (Mutex::new(Registry::default()), Condvar::new()));
static WORKER_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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

    let state = if kind == RegistrationKind::Async && registry.initialization_finished {
        ActivationState::Queued
    } else {
        ActivationState::Unloaded
    };
    for (command, doc) in command_docs {
        registry.commands.insert(command.clone(), name.clone());
        registry.docs.insert(command, doc);
    }
    registry.plugins.insert(
        name,
        Plugin {
            modules,
            initializers,
            kind,
            state,
        },
    );
    condvar.notify_all();
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

    let queued = {
        let (lock, _) = &*REGISTRY;
        let mut registry = lock.lock().unwrap();
        if registry.generation != generation {
            return;
        }
        registry.initialization_finished = true;
        let names = registry
            .plugins
            .iter_mut()
            .filter_map(|(name, plugin)| {
                if plugin.kind == RegistrationKind::Async
                    && matches!(plugin.state, ActivationState::Unloaded)
                {
                    plugin.state = ActivationState::Queued;
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        registry.worker_active = !names.is_empty();
        names
    };

    if queued.is_empty() {
        return;
    }

    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _worker = WORKER_LOCK
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            for plugin_name in queued {
                let should_precompile = {
                    let registry = REGISTRY.0.lock().unwrap();
                    registry.generation == generation
                        && registry
                            .plugins
                            .get(&plugin_name)
                            .is_some_and(|plugin| matches!(plugin.state, ActivationState::Queued))
                };
                if !should_precompile {
                    continue;
                }
                let mut worker_engine = super::background_compiler_engine();
                precompile_one(&mut worker_engine, generation, plugin_name);
            }
        }));
        let worker_panicked = result.is_err();
        finish_precompilation(generation, worker_panicked);
        if worker_panicked {
            log::error!("lazy plugin background precompilation worker panicked");
        }
    });
}

fn finish_precompilation(generation: usize, worker_panicked: bool) {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if registry.generation == generation {
        registry.worker_active = false;
        if worker_panicked {
            for plugin in registry.plugins.values_mut() {
                if matches!(
                    plugin.state,
                    ActivationState::Queued | ActivationState::Precompiling
                ) {
                    plugin.state = ActivationState::Unloaded;
                }
            }
        }
    }
    condvar.notify_all();
}

fn module_source(module: &str) -> String {
    format!("(require {module:?})")
}

fn compile_modules(engine: &mut Engine, modules: &[String]) -> Result<(), SteelErr> {
    // Steel raw programs retain engine-local compiler and module metadata. Moving
    // one to the foreground engine can corrupt constant and global indexes, while
    // compiling on a clone mutates the live compiler's module-emission state.
    // Keep this worker strictly compile-only and let activation compile and run on
    // the foreground engine. This still validates and warms module source caches
    // without evaluating top-level forms or initializers in the background.
    for module in modules {
        engine.emit_raw_program(module_source(module), steel_init_file())?;
    }
    Ok(())
}

fn compile_and_run_modules(engine: &mut Engine, modules: &[String]) -> Result<(), SteelErr> {
    for module in modules {
        engine.compile_and_run_raw_program_with_path(module_source(module), steel_init_file())?;
    }
    Ok(())
}

fn precompile_one(engine: &mut Engine, generation: usize, plugin_name: String) {
    let modules = {
        let (lock, condvar) = &*REGISTRY;
        let mut registry = lock.lock().unwrap();
        if registry.generation != generation {
            return;
        }
        let Some(plugin) = registry.plugins.get_mut(&plugin_name) else {
            return;
        };
        if !matches!(plugin.state, ActivationState::Queued) {
            return;
        }
        plugin.state = ActivationState::Precompiling;
        condvar.notify_all();
        plugin.modules.clone()
    };

    let result = compile_modules(engine, &modules);

    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if registry.generation != generation {
        condvar.notify_all();
        return;
    }
    let Some(plugin) = registry.plugins.get_mut(&plugin_name) else {
        condvar.notify_all();
        return;
    };
    if !matches!(plugin.state, ActivationState::Precompiling) {
        condvar.notify_all();
        return;
    }
    plugin.state = match result {
        Ok(()) => ActivationState::Ready,
        Err(error) => {
            // Precompilation is an optimization. Retry on the foreground engine
            // when the command is actually invoked before retaining a failure.
            log::warn!("unable to precompile lazy plugin {plugin_name:?}: {error}");
            ActivationState::Unloaded
        }
    };
    condvar.notify_all();
}

enum ActivationWork {
    Compile(Vec<String>),
    Call,
}

fn begin_activation(command: &str) -> Result<(String, ActivationWork), SteelErr> {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    loop {
        let Some(plugin_name) = registry.commands.get(command).cloned() else {
            return Err(lazy_error(format!(
                "lazy command {command:?} disappeared during engine reload"
            )));
        };
        if registry.worker_active
            && registry.plugins.get(&plugin_name).is_some_and(|plugin| {
                matches!(
                    plugin.state,
                    ActivationState::Unloaded
                        | ActivationState::Queued
                        | ActivationState::Precompiling
                        | ActivationState::Ready
                )
            })
        {
            registry = condvar.wait(registry).unwrap();
            continue;
        }
        let plugin = registry.plugins.get_mut(&plugin_name).unwrap();
        let work = match &mut plugin.state {
            ActivationState::Unloaded | ActivationState::Queued | ActivationState::Ready => {
                let modules = plugin.modules.clone();
                plugin.state = ActivationState::Activating;
                Some(ActivationWork::Compile(modules))
            }
            ActivationState::Loaded => Some(ActivationWork::Call),
            ActivationState::Failed(message) => return Err(lazy_error(message.clone())),
            ActivationState::Activating => {
                return Err(lazy_error(format!(
                    "recursive activation of lazy plugin {plugin_name:?}"
                )))
            }
            ActivationState::Precompiling => None,
        };
        if let Some(work) = work {
            return Ok((plugin_name, work));
        }
        registry = condvar.wait(registry).unwrap();
    }
}

enum ActivationCompletion<'a> {
    Loaded,
    Failed(&'a SteelErr),
}

fn finish_activation(plugin_name: &str, completion: ActivationCompletion<'_>) {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if let Some(plugin) = registry.plugins.get_mut(plugin_name) {
        if matches!(plugin.state, ActivationState::Activating) {
            plugin.state = match completion {
                ActivationCompletion::Loaded => ActivationState::Loaded,
                ActivationCompletion::Failed(error) => ActivationState::Failed(error.to_string()),
            };
        }
    }
    condvar.notify_all();
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
            ActivationWork::Compile(modules) => compile_and_run_modules(engine, &modules),
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
                engine.call_function_by_name_with_args(&initializer, Vec::new())?;
            }
            Ok(())
        })();

        if let Err(error) = initialization_result {
            finish_activation(&plugin_name, ActivationCompletion::Failed(&error));
            return Err(error);
        }
        finish_activation(&plugin_name, ActivationCompletion::Loaded);
    }

    engine.call_function_by_name_with_args(command, args)
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

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn docs(command: &str) -> HashMap<String, String> {
        HashMap::from([(command.to_string(), format!("Documentation for {command}"))])
    }

    fn reset_test_registry() {
        reset(1);
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
        precompile_one(&mut engine, 1, "stale".into());
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
        while registry.worker_active
            || !["async-one", "async-two"].iter().all(|name| {
                matches!(
                    registry.plugins.get(*name).unwrap().state,
                    ActivationState::Ready | ActivationState::Failed(_)
                )
            })
        {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "background precompilation timed out");
            (registry, _) = condvar.wait_timeout(registry, remaining).unwrap();
        }
        assert!(["async-one", "async-two"].iter().all(|name| matches!(
            registry.plugins.get(*name).unwrap().state,
            ActivationState::Ready
        )));
        assert!(!registry.worker_active);
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
            registry.plugins.get_mut("retry").unwrap().state = ActivationState::Queued;
        }

        precompile_one(&mut engine, 1, "retry".into());
        assert!(matches!(
            REGISTRY
                .0
                .lock()
                .unwrap()
                .plugins
                .get("retry")
                .unwrap()
                .state,
            ActivationState::Unloaded
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
            registry.plugins.get_mut("early").unwrap().state = ActivationState::Queued;
        }

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
    fn activation_waits_until_the_background_queue_is_quiescent() {
        let _test = TEST_LOCK.lock().unwrap();
        reset_test_registry();
        let mut engine = Engine::new();
        engine.register_steel_module(
            "lazy-test/barrier.scm".into(),
            "(provide barrier-command) (define (barrier-command) 1)".into(),
        );
        register_async_lazy_plugin(
            "barrier".into(),
            vec!["lazy-test/barrier.scm".into()],
            Vec::new(),
            docs("barrier-command"),
        )
        .unwrap();
        {
            let mut registry = REGISTRY.0.lock().unwrap();
            registry.worker_active = true;
            registry.plugins.get_mut("barrier").unwrap().state = ActivationState::Ready;
        }

        let (sender, receiver) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            sender
                .send(
                    begin_activation("barrier-command")
                        .map(|(_, work)| matches!(work, ActivationWork::Compile(_))),
                )
                .unwrap();
        });
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        finish_precompilation(1, false);
        assert!(receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap());
        waiter.join().unwrap();
        finish_activation("barrier", ActivationCompletion::Loaded);
    }

    #[test]
    fn panicked_worker_releases_waiters_for_foreground_retry() {
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
            registry.worker_active = true;
            registry.plugins.get_mut("worker-panic").unwrap().state = ActivationState::Precompiling;
        }

        finish_precompilation(1, true);

        let registry = REGISTRY.0.lock().unwrap();
        assert!(!registry.worker_active);
        assert!(matches!(
            registry.plugins.get("worker-panic").unwrap().state,
            ActivationState::Unloaded
        ));
        drop(registry);
        assert!(matches!(
            begin_activation("worker-panic-command").unwrap().1,
            ActivationWork::Compile(_)
        ));
        let failure = lazy_error("expected foreground failure");
        finish_activation("worker-panic", ActivationCompletion::Failed(&failure));
    }
}

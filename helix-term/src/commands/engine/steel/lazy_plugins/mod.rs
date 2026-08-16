use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
};

mod activation;
mod discovery;
mod local_dependencies;
mod precompile;
mod state;

use activation::*;
use discovery::*;
use local_dependencies::*;
pub(super) use precompile::finish_initialization;
use precompile::*;
use state::*;

use steel::{
    rerrs::ErrorKind,
    rvals::IntoSteelVal,
    steel_vm::{builtin::BuiltInModule, engine::Engine, register_fn::RegisterFn},
    SteelErr, SteelVal,
};
use steel_program_linker::{CaptureOptions, CompiledObject, CompilerContextId, LoadError};

use crate::{commands::Context, compositor, ui::PromptEvent};

use super::{steel_init_file, CTX};

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
    logical_name: String,
    name: String,
    modules: Vec<String>,
    initializers: Vec<String>,
    command_docs: HashMap<String, String>,
    kind: RegistrationKind,
) -> Result<(), SteelErr> {
    validate_name("logical name", &logical_name)?;
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

    if !registry.logical_plugins.contains_key(&logical_name) {
        registry.logical_plugins.insert(
            logical_name.clone(),
            LogicalPlugin {
                eager_modules: modules.clone(),
                manifests: Vec::new(),
                builtin: false,
                strategy: None,
            },
        );
    }
    let newly_registered = {
        let logical = registry.logical_plugins.get_mut(&logical_name).unwrap();
        if matches!(logical.strategy, Some(LogicalStrategy::Eager)) {
            return Err(lazy_error(format!(
                "logical plugin {logical_name:?} is already registered eagerly"
            )));
        }
        let newly_registered = logical.strategy.is_none();
        logical.strategy = Some(LogicalStrategy::Lazy);
        logical.manifests.push(name.clone());
        newly_registered
    };
    if newly_registered {
        registry
            .logical_registration_order
            .push(logical_name.clone());
    }

    let queued_job = if kind == RegistrationKind::Async && registry.initialization_finished {
        let job_id = registry.allocate_job();
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
        name.clone(),
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
        name.clone(),
        name,
        modules,
        initializers,
        command_docs,
        RegistrationKind::Async,
    )
}

fn discover_lazy_commands(sources: Vec<String>) -> Result<HashMap<String, String>, SteelErr> {
    discover_commands(&sources)
}

fn register_discovered_lazy_plugin(
    name: String,
    modules: Vec<String>,
    initializers: Vec<String>,
    sources: Vec<String>,
) -> Result<(), SteelErr> {
    let command_docs = discover_commands(&sources)?;
    register_plugin(
        name.clone(),
        name,
        modules,
        initializers,
        command_docs,
        RegistrationKind::Lazy,
    )
}

fn register_discovered_logical_lazy_plugin(
    logical_name: String,
    name: String,
    modules: Vec<String>,
    initializers: Vec<String>,
    sources: Vec<String>,
) -> Result<(), SteelErr> {
    let command_docs = discover_commands(&sources)?;
    register_plugin(
        logical_name,
        name,
        modules,
        initializers,
        command_docs,
        RegistrationKind::Lazy,
    )
}

fn register_logical_plugin(name: String, eager_modules: Vec<String>) -> Result<(), SteelErr> {
    validate_name("logical name", &name)?;
    if eager_modules.is_empty() {
        return Err(lazy_error(format!(
            "logical plugin {name:?} must register at least one eager module"
        )));
    }
    for module in &eager_modules {
        validate_name("module", module)?;
    }
    let mut registry = REGISTRY.0.lock().unwrap();
    if let Some(existing) = registry.logical_plugins.get(&name) {
        if existing.builtin && existing.eager_modules == eager_modules {
            return Ok(());
        }
        return Err(lazy_error(format!(
            "logical plugin {name:?} is already cataloged"
        )));
    }
    registry.logical_plugins.insert(
        name,
        LogicalPlugin {
            eager_modules,
            manifests: Vec::new(),
            builtin: true,
            strategy: None,
        },
    );
    Ok(())
}

fn mark_logical_plugin_eager(name: String) -> Result<(), SteelErr> {
    let mut registry = REGISTRY.0.lock().unwrap();
    let newly_registered = {
        let Some(plugin) = registry.logical_plugins.get_mut(&name) else {
            return Err(lazy_error(format!("unknown logical plugin {name:?}")));
        };
        if matches!(plugin.strategy, Some(LogicalStrategy::Lazy)) {
            return Err(lazy_error(format!(
                "logical plugin {name:?} is already registered lazily"
            )));
        }
        let newly_registered = plugin.strategy.is_none();
        plugin.strategy = Some(LogicalStrategy::Eager);
        newly_registered
    };
    if newly_registered {
        registry.logical_registration_order.push(name);
    }
    Ok(())
}

fn logical_plugin_registered(name: String) -> bool {
    REGISTRY
        .0
        .lock()
        .unwrap()
        .logical_plugins
        .get(&name)
        .is_some_and(|plugin| plugin.strategy.is_some())
}

fn logical_plugin_registered_ids() -> Vec<String> {
    let registry = REGISTRY.0.lock().unwrap();
    registry
        .logical_registration_order
        .iter()
        .filter(|name| {
            registry
                .logical_plugins
                .get(*name)
                .is_some_and(|plugin| plugin.strategy.is_some())
        })
        .cloned()
        .collect()
}

fn logical_plugin_strategy(name: String) -> Option<String> {
    REGISTRY
        .0
        .lock()
        .unwrap()
        .logical_plugins
        .get(&name)
        .and_then(|plugin| plugin.strategy)
        .map(|strategy| match strategy {
            LogicalStrategy::Lazy => "lazy".to_owned(),
            LogicalStrategy::Eager => "eager".to_owned(),
        })
}

fn logical_plugin_materialized(name: String) -> bool {
    let registry = REGISTRY.0.lock().unwrap();
    let Some(logical) = registry.logical_plugins.get(&name) else {
        return false;
    };
    match logical.strategy {
        Some(LogicalStrategy::Eager) => true,
        Some(LogicalStrategy::Lazy) => {
            !logical.manifests.is_empty()
                && logical.manifests.iter().all(|manifest| {
                    registry
                        .plugins
                        .get(manifest)
                        .is_some_and(|plugin| matches!(plugin.state, ActivationState::Loaded))
                })
        }
        None => false,
    }
}

fn normalize_dependency_values(dependencies: Vec<SteelVal>) -> Result<Vec<String>, SteelErr> {
    let mut seen = HashSet::new();
    dependencies
        .into_iter()
        .map(|dependency| {
            let SteelVal::SymbolV(id) = dependency else {
                return Err(lazy_error(
                    "local-plugin-dependencies accepts only literal symbol IDs",
                ));
            };
            let id = id.to_string();
            if !seen.insert(id.clone()) {
                return Err(lazy_error(format!(
                    "duplicate local plugin dependency {id:?}"
                )));
            }
            Ok(id)
        })
        .collect()
}

fn verify_local_plugin_dependencies(dependencies: Vec<SteelVal>) -> Result<(), SteelErr> {
    for dependency in normalize_dependency_values(dependencies)? {
        if !logical_plugin_materialized(dependency.clone()) {
            return Err(lazy_error(format!(
                "local plugin dependency {dependency:?} was not materialized before evaluation"
            )));
        }
    }
    Ok(())
}

enum LocalMaterialization {
    AlreadyLoaded,
    Lazy(Vec<String>),
    Eager(Vec<String>),
}

pub(super) fn materialize_local_dependencies(
    engine: &mut Engine,
    source: &str,
    path: &Path,
) -> Result<(), SteelErr> {
    let dependencies = parse_local_dependencies(source, path)?;
    if dependencies.is_empty() {
        return Ok(());
    }

    let existing_commands = {
        let registry = REGISTRY.0.lock().unwrap();
        registry
            .commands
            .keys()
            .filter(|command| engine.global_exists(command))
            .cloned()
            .collect::<HashSet<_>>()
    };
    let plans = {
        let registry = REGISTRY.0.lock().unwrap();
        dependencies
            .iter()
            .map(|dependency| {
                let Some(logical) = registry.logical_plugins.get(dependency) else {
                    return Err(lazy_error(format!(
                        "unknown local plugin dependency {dependency:?}; load plugins/lazy-plugins.scm in the global configuration"
                    )));
                };
                match logical.strategy {
                    Some(LogicalStrategy::Eager) => Ok(LocalMaterialization::AlreadyLoaded),
                    Some(LogicalStrategy::Lazy) if logical.manifests.is_empty() => Err(lazy_error(
                        format!("lazy local plugin dependency {dependency:?} has no manifests"),
                    )),
                    Some(LogicalStrategy::Lazy) => {
                        Ok(LocalMaterialization::Lazy(logical.manifests.clone()))
                    }
                    None if logical.builtin => {
                        Ok(LocalMaterialization::Eager(logical.eager_modules.clone()))
                    }
                    None => Err(lazy_error(format!(
                        "custom local plugin dependency {dependency:?} must be registered globally"
                    ))),
                }
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    for (dependency, plan) in dependencies.iter().zip(plans) {
        match plan {
            LocalMaterialization::AlreadyLoaded => {}
            LocalMaterialization::Lazy(manifests) => {
                for manifest in manifests {
                    activate_plugin(engine, &manifest)?;
                }
            }
            LocalMaterialization::Eager(modules) => {
                compile_and_run_modules(engine, &modules)?;
                let mut registry = REGISTRY.0.lock().unwrap();
                {
                    let logical = registry
                        .logical_plugins
                        .get_mut(dependency)
                        .ok_or_else(|| {
                            lazy_error(format!(
                                "local plugin dependency {dependency:?} disappeared during engine reload"
                            ))
                        })?;
                    logical.strategy = Some(LogicalStrategy::Eager);
                }
                registry.logical_registration_order.push(dependency.clone());
            }
        }
    }

    let mut registry = REGISTRY.0.lock().unwrap();
    let locally_loaded = registry
        .commands
        .keys()
        .filter(|command| {
            !existing_commands.contains(*command) && engine.global_exists(command.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    registry.locally_loaded_commands.extend(locally_loaded);
    Ok(())
}

fn command_doc(command: String) -> Option<String> {
    documentation(&command)
}

pub(super) fn register_builtin(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/lazy");
    let facade = include_str!("../lazy.scm");
    module
        .register_fn("#%register-lazy-plugin!", register_lazy_plugin)
        .register_fn("#%register-async-lazy-plugin!", register_async_lazy_plugin)
        .register_fn("#%discover-lazy-commands", discover_lazy_commands)
        .register_fn(
            "#%register-discovered-lazy-plugin!",
            register_discovered_lazy_plugin,
        )
        .register_fn(
            "#%register-discovered-logical-lazy-plugin!",
            register_discovered_logical_lazy_plugin,
        )
        .register_fn("#%register-logical-plugin!", register_logical_plugin)
        .register_fn("#%mark-logical-plugin-eager!", mark_logical_plugin_eager)
        .register_fn("logical-plugin-registered?", logical_plugin_registered)
        .register_fn(
            "logical-plugin-registered-ids",
            logical_plugin_registered_ids,
        )
        .register_fn("logical-plugin-strategy", logical_plugin_strategy)
        .register_fn("logical-plugin-materialized?", logical_plugin_materialized)
        .register_fn("lazy-plugin-command-doc", command_doc);

    engine.register_fn(
        "local-plugin-dependencies",
        verify_local_plugin_dependencies,
    );

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
    reset_discovery_cache();
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
mod tests;

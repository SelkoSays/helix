use std::{borrow::Cow, collections::HashMap};

mod activation;
mod discovery;
mod precompile;
mod state;

use activation::*;
use discovery::*;
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
        name,
        modules,
        initializers,
        command_docs,
        RegistrationKind::Lazy,
    )
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

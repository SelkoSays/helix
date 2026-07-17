//! Foreground artifact loading, initialization, and command execution.

use super::*;
use steel_program_linker::ProgramLoader;

pub(super) enum ActivationWork {
    Compile(Vec<String>),
    Precompiled(CompiledObject),
    Call,
}

pub(super) fn begin_activation(command: &str) -> Result<(String, ActivationWork), SteelErr> {
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

pub(super) enum ActivationCompletion<'a> {
    Loaded,
    FallbackPending,
    Failed(&'a SteelErr),
}

pub(super) fn finish_activation(plugin_name: &str, completion: ActivationCompletion<'_>) {
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

pub(super) fn callable_value(engine: &Engine, function: &str) -> Result<SteelVal, SteelErr> {
    let mut value = engine.extract_value(function)?;
    for _ in 0..64 {
        match value {
            SteelVal::HeapAllocated(reference) => value = reference.get(),
            _ => return Ok(value),
        }
    }
    Ok(value)
}

pub(super) fn call_resolved_function_by_name(
    engine: &mut Engine,
    function: &str,
    args: Vec<SteelVal>,
) -> Result<SteelVal, SteelErr> {
    let function = callable_value(engine, function)?;
    engine.call_function_with_args(function, args)
}

pub(super) enum ArtifactLoadError {
    Safe(String),
    Execution(SteelErr),
}

fn loader_state_for_generation(
    state: &mut Option<LoaderState>,
    generation: usize,
) -> &mut LoaderState {
    if state
        .as_ref()
        .is_none_or(|state| state.generation != generation)
    {
        *state = Some(LoaderState {
            generation,
            loader: None,
        });
    }
    state.as_mut().unwrap()
}

pub(super) fn load_precompiled(
    engine: &mut Engine,
    object: CompiledObject,
) -> Result<(), ArtifactLoadError> {
    let generation = REGISTRY.0.lock().unwrap().generation;
    let mut state = LOADER.lock().unwrap();
    let state = loader_state_for_generation(&mut state, generation);
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
        Err(error) if error.safe_to_fallback() => Err(ArtifactLoadError::Safe(error.to_string())),
        Err(_) => unreachable!("execution is the only unsafe linker failure"),
    }
}

pub(super) fn refresh_loader_after_foreground(engine: &Engine) {
    let generation = REGISTRY.0.lock().unwrap().generation;
    let mut state = LOADER.lock().unwrap();
    let state = loader_state_for_generation(&mut state, generation);
    let result = if let Some(loader) = state.loader.as_mut() {
        loader.refresh_from_engine(engine)
    } else {
        ProgramLoader::from_engine(engine, compiler_context()).map(|loader| {
            state.loader = Some(loader);
        })
    };
    if let Err(error) = result {
        log::warn!("unable to refresh lazy-plugin program loader: {error}");
        state.loader = None;
    }
}

pub(super) fn activate_and_call(
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

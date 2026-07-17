//! Background compilation and publication of transferable Steel artifacts.

use super::*;
use steel_program_linker::ProgramLoader;

pub(super) fn mark_plugin_failed(registry: &mut Registry, plugin_name: &str, message: String) {
    if let Some(plugin) = registry.plugins.get_mut(plugin_name) {
        plugin.state = ActivationState::Failed(message);
    }
}

pub(super) fn validate_global_collisions(engine: &Engine) {
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

pub(in crate::commands::engine::steel) fn finish_initialization(
    engine: &Engine,
    generation: usize,
) {
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
        registry.queue_initialized_async_plugins()
    };

    if !queued.is_empty() {
        spawn_precompile_queue(generation, queued);
    }
}

pub(super) fn spawn_precompile_queue(generation: usize, queued: Vec<(String, u64)>) {
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

pub(super) fn publish_precompile_failure(
    generation: usize,
    plugin_name: &str,
    job_id: u64,
    message: &str,
) {
    let (lock, condvar) = &*REGISTRY;
    let mut registry = lock.lock().unwrap();
    if registry.generation == generation {
        if let Some(plugin) = registry.plugins.get_mut(plugin_name) {
            if plugin.fallback_if_job_matches(job_id) {
                log::warn!("unable to precompile lazy plugin {plugin_name:?}: {message}");
            }
        }
    }
    condvar.notify_all();
}

pub(super) fn module_source(modules: &[String]) -> String {
    modules
        .iter()
        .map(|module| format!("(require {module:?})\n"))
        .collect()
}

pub(super) fn compile_modules(
    engine: &mut Engine,
    modules: &[String],
) -> Result<CompiledObject, String> {
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

pub(super) fn compile_and_run_modules(
    engine: &mut Engine,
    modules: &[String],
) -> Result<(), SteelErr> {
    engine
        .compile_and_run_raw_program_with_path(module_source(modules), steel_init_file())
        .map(|_| ())
}

pub(super) fn precompile_one(generation: usize, plugin_name: &str, job_id: u64) {
    let modules = {
        let (lock, condvar) = &*REGISTRY;
        let mut registry = lock.lock().unwrap();
        if registry.generation != generation {
            return;
        }
        let Some(plugin) = registry.plugins.get_mut(plugin_name) else {
            return;
        };
        let Some(modules) = plugin.begin_precompile(job_id) else {
            return;
        };
        condvar.notify_all();
        modules
    };

    // Each artifact must be self-contained. Reusing an engine here would let
    // later artifacts omit modules emitted while compiling earlier plugins.
    let mut engine = super::super::background_compiler_engine();
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
    let (object, error) = match result {
        Ok(object) => (Some(object), None),
        Err(error) => (None, Some(error)),
    };
    if !plugin.finish_precompile(job_id, object) {
        condvar.notify_all();
        return;
    }
    if let Some(error) = error {
        log::warn!("unable to precompile lazy plugin {plugin_name:?}: {error}");
    }
    condvar.notify_all();
}

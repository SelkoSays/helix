use super::*;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
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

mod failures;
mod loading;
mod precompile;
mod registration;

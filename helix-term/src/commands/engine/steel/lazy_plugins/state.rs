//! Shared lazy-plugin state.
//!
//! Registry state and loader state use separate locks. Code must read the
//! generation from the registry before taking the loader lock and must never
//! acquire the registry lock while the loader lock is held.

use std::{
    collections::HashMap,
    sync::{Condvar, Mutex},
};

use once_cell::sync::Lazy;
use steel_program_linker::{CompiledObject, ProgramLoader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegistrationKind {
    Lazy,
    Async,
}

/// Lifecycle for one lazy plugin.
///
/// Job IDs distinguish a current background attempt from stale workers. A
/// safe pre-execution failure becomes `FallbackPending`; failures after Steel
/// execution begins are retained in `Failed` and are not retried.
pub(super) enum ActivationState {
    Unloaded,
    Queued { job_id: u64 },
    Precompiling { job_id: u64 },
    Ready(CompiledObject),
    FallbackPending,
    Activating,
    Loaded,
    Failed(String),
}

pub(super) struct Plugin {
    pub(super) modules: Vec<String>,
    pub(super) initializers: Vec<String>,
    pub(super) kind: RegistrationKind,
    pub(super) state: ActivationState,
}

impl Plugin {
    pub(super) fn begin_precompile(&mut self, job_id: u64) -> Option<Vec<String>> {
        if !matches!(self.state, ActivationState::Queued { job_id: current } if current == job_id) {
            return None;
        }
        self.state = ActivationState::Precompiling { job_id };
        Some(self.modules.clone())
    }

    pub(super) fn finish_precompile(
        &mut self,
        job_id: u64,
        object: Option<CompiledObject>,
    ) -> bool {
        if !matches!(self.state, ActivationState::Precompiling { job_id: current } if current == job_id)
        {
            return false;
        }
        self.state = object
            .map(ActivationState::Ready)
            .unwrap_or(ActivationState::FallbackPending);
        true
    }

    pub(super) fn fallback_if_job_matches(&mut self, job_id: u64) -> bool {
        if !matches!(
            self.state,
            ActivationState::Queued { job_id: current }
                | ActivationState::Precompiling { job_id: current }
                if current == job_id
        ) {
            return false;
        }
        self.state = ActivationState::FallbackPending;
        true
    }
}

#[derive(Default)]
pub(super) struct Registry {
    pub(super) generation: usize,
    pub(super) plugins: HashMap<String, Plugin>,
    pub(super) commands: HashMap<String, String>,
    pub(super) docs: HashMap<String, String>,
    pub(super) registration_order: Vec<String>,
    pub(super) initialization_finished: bool,
    pub(super) next_job_id: u64,
}

impl Registry {
    pub(super) fn allocate_job(&mut self) -> u64 {
        let job_id = self.next_job_id;
        self.next_job_id += 1;
        job_id
    }

    pub(super) fn queue_initialized_async_plugins(&mut self) -> Vec<(String, u64)> {
        let names = self.registration_order.clone();
        let mut queued = Vec::new();
        for name in names {
            let should_queue = self.plugins.get(&name).is_some_and(|plugin| {
                plugin.kind == RegistrationKind::Async
                    && matches!(plugin.state, ActivationState::Unloaded)
            });
            if should_queue {
                let job_id = self.allocate_job();
                self.plugins.get_mut(&name).unwrap().state = ActivationState::Queued { job_id };
                queued.push((name, job_id));
            }
        }
        queued
    }
}

pub(super) struct LoaderState {
    pub(super) generation: usize,
    pub(super) loader: Option<ProgramLoader>,
}

pub(super) static REGISTRY: Lazy<(Mutex<Registry>, Condvar)> =
    Lazy::new(|| (Mutex::new(Registry::default()), Condvar::new()));
pub(super) static WORKER_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
pub(super) static LOADER: Lazy<Mutex<Option<LoaderState>>> = Lazy::new(|| Mutex::new(None));

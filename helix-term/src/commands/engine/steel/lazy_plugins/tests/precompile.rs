use super::*;

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
        registry.plugins.get_mut("retry").unwrap().state = ActivationState::Queued { job_id: 7 };
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
        registry.plugins.get_mut("early").unwrap().state = ActivationState::Queued { job_id: 8 };
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

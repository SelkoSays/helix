use super::*;

#[test]
fn local_dependency_materializes_lazy_plugin_and_initializer_once() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/local-lazy.scm".into(),
        r#"
            (provide local-lazy-command local-lazy-init!)
            (define local-lazy-state "cold")
            (define (local-lazy-init!) (set! local-lazy-state "ready"))
            (define (local-lazy-command) local-lazy-state)
        "#
        .into(),
    );
    register_logical_plugin("local-lazy".into(), vec!["lazy-test/local-lazy.scm".into()]).unwrap();
    register_plugin(
        "local-lazy".into(),
        "local-lazy/core".into(),
        vec!["lazy-test/local-lazy.scm".into()],
        vec!["local-lazy-init!".into()],
        docs("local-lazy-command"),
        RegistrationKind::Lazy,
    )
    .unwrap();

    materialize_local_dependencies(
        &mut engine,
        "(local-plugin-dependencies '(local-lazy))",
        std::path::Path::new("local.scm"),
    )
    .unwrap();

    assert!(logical_plugin_materialized("local-lazy".into()));
    assert_eq!(
        activate_and_call(&mut engine, "local-lazy-command", Vec::new()).unwrap(),
        SteelVal::StringV("ready".into())
    );
}

#[test]
fn local_dependency_can_introduce_omitted_builtin() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/local-eager.scm".into(),
        "(provide local-eager-value) (define local-eager-value 42)".into(),
    );
    register_logical_plugin(
        "local-eager".into(),
        vec!["lazy-test/local-eager.scm".into()],
    )
    .unwrap();

    materialize_local_dependencies(
        &mut engine,
        "(local-plugin-dependencies '(local-eager))",
        std::path::Path::new("local.scm"),
    )
    .unwrap();

    assert!(engine.global_exists("local-eager-value"));
    assert_eq!(
        logical_plugin_strategy("local-eager".into()).as_deref(),
        Some("eager")
    );
    assert!(logical_plugin_materialized("local-eager".into()));
}

#[test]
fn local_dependency_can_materialize_registered_custom_plugin() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/local-custom.scm".into(),
        "(provide local-custom-command) (define (local-custom-command) 9)".into(),
    );
    register_lazy_plugin(
        "local-custom".into(),
        vec!["lazy-test/local-custom.scm".into()],
        Vec::new(),
        docs("local-custom-command"),
    )
    .unwrap();

    materialize_local_dependencies(
        &mut engine,
        "(local-plugin-dependencies '(local-custom))",
        std::path::Path::new("local.scm"),
    )
    .unwrap();

    assert!(logical_plugin_materialized("local-custom".into()));
    assert_eq!(
        activate_and_call(&mut engine, "local-custom-command", Vec::new()).unwrap(),
        SteelVal::IntV(9)
    );
}

#[test]
fn local_dependency_preflights_all_ids_before_loading() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/local-known.scm".into(),
        "(provide local-known-value) (define local-known-value 1)".into(),
    );
    register_logical_plugin("known".into(), vec!["lazy-test/local-known.scm".into()]).unwrap();

    assert!(materialize_local_dependencies(
        &mut engine,
        "(local-plugin-dependencies '(known unknown))",
        std::path::Path::new("local.scm"),
    )
    .is_err());
    assert!(!engine.global_exists("local-known-value"));
}

#[test]
fn trusted_local_transitive_import_is_not_a_startup_collision() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/local-transitive.scm".into(),
        "(provide local-transitive-command) (define (local-transitive-command) 7)".into(),
    );
    engine.register_steel_module(
        "lazy-test/local-root.scm".into(),
        "(require \"lazy-test/local-transitive.scm\") (provide local-root-value) (define local-root-value 1)"
            .into(),
    );
    register_logical_plugin(
        "transitive".into(),
        vec!["lazy-test/local-transitive.scm".into()],
    )
    .unwrap();
    register_plugin(
        "transitive".into(),
        "transitive/core".into(),
        vec!["lazy-test/local-transitive.scm".into()],
        Vec::new(),
        docs("local-transitive-command"),
        RegistrationKind::Lazy,
    )
    .unwrap();
    register_logical_plugin("root".into(), vec!["lazy-test/local-root.scm".into()]).unwrap();

    materialize_local_dependencies(
        &mut engine,
        "(local-plugin-dependencies '(root))",
        std::path::Path::new("local.scm"),
    )
    .unwrap();
    validate_global_collisions(&engine);

    assert_eq!(
        activate_and_call(&mut engine, "local-transitive-command", Vec::new()).unwrap(),
        SteelVal::IntV(7)
    );
}

#[test]
fn local_dependency_supersedes_queued_precompile() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/local-queued.scm".into(),
        "(provide local-queued-command) (define (local-queued-command) 11)".into(),
    );
    register_async_lazy_plugin(
        "local-queued".into(),
        vec!["lazy-test/local-queued.scm".into()],
        Vec::new(),
        docs("local-queued-command"),
    )
    .unwrap();
    REGISTRY
        .0
        .lock()
        .unwrap()
        .plugins
        .get_mut("local-queued")
        .unwrap()
        .state = ActivationState::Queued { job_id: 7 };

    materialize_local_dependencies(
        &mut engine,
        "(local-plugin-dependencies '(local-queued))",
        std::path::Path::new("local.scm"),
    )
    .unwrap();

    assert!(matches!(
        REGISTRY
            .0
            .lock()
            .unwrap()
            .plugins
            .get("local-queued")
            .unwrap()
            .state,
        ActivationState::Loaded
    ));
}

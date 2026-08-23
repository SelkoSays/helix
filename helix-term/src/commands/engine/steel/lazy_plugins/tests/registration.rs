use super::*;

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
fn preindexed_logical_manifest_retains_ownership_and_docs() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    register_logical_plugin("catalog".into(), vec!["catalog.scm".into()]).unwrap();
    register_logical_lazy_plugin(
        "catalog".into(),
        "catalog/first".into(),
        vec!["first.scm".into()],
        Vec::new(),
        docs("catalog-first"),
    )
    .unwrap();
    register_logical_lazy_plugin(
        "catalog".into(),
        "catalog/second".into(),
        vec!["second.scm".into()],
        Vec::new(),
        docs("catalog-second"),
    )
    .unwrap();

    assert_eq!(
        documentation("catalog-first").as_deref(),
        Some("Documentation for catalog-first")
    );
    assert_eq!(
        logical_plugin_strategy("catalog".into()).as_deref(),
        Some("lazy")
    );
    let registry = REGISTRY.0.lock().unwrap();
    assert_eq!(
        registry.logical_plugins["catalog"].manifests,
        vec!["catalog/first", "catalog/second"]
    );
}

#[test]
fn preindexed_logical_manifest_uses_existing_validation() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    register_logical_plugin("catalog".into(), vec!["catalog.scm".into()]).unwrap();

    assert!(register_logical_lazy_plugin(
        "catalog".into(),
        "catalog/empty".into(),
        vec!["empty.scm".into()],
        Vec::new(),
        HashMap::new(),
    )
    .is_err());
    assert!(register_logical_lazy_plugin(
        "catalog".into(),
        "catalog/builtin".into(),
        vec!["builtin.scm".into()],
        Vec::new(),
        docs("quit"),
    )
    .is_err());
}

#[test]
fn builtin_source_setup_can_load_lazy_dependent_modules() {
    let _test = TEST_LOCK.lock().unwrap();
    let mut engine = Engine::new();
    engine.register_value(super::super::super::CTX, SteelVal::Void);
    engine.register_value(super::super::super::CONFIG, SteelVal::Void);

    super::super::super::configure_builtin_sources(&mut engine, false);

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

use super::*;

#[test]
fn precompiled_module_execution_failures_are_retained() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut worker = Engine::new();
    worker.register_steel_module(
        "lazy-test/module-fail.scm".into(),
        r#"
                (provide module-fail-command)
                (error "module execution failed")
                (define (module-fail-command) 1)
            "#
        .into(),
    );
    let object = compile_modules(&mut worker, &["lazy-test/module-fail.scm".into()]).unwrap();
    let mut foreground = Engine::new();
    register_async_lazy_plugin(
        "module-fail".into(),
        vec!["lazy-test/module-fail.scm".into()],
        Vec::new(),
        docs("module-fail-command"),
    )
    .unwrap();
    set_ready_plugin("module-fail", object);

    let error = activate_and_call(&mut foreground, "module-fail-command", Vec::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("module execution failed"));
    assert!(begin_activation("module-fail-command")
        .err()
        .unwrap()
        .to_string()
        .contains("module execution failed"));
}

#[test]
fn initializer_failures_are_retained() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut worker = Engine::new();
    worker.register_steel_module(
        "lazy-test/init-fail.scm".into(),
        r#"
                (provide init-fail-command init-fail-init!)
                (define (init-fail-init!) (error "initializer failed"))
                (define (init-fail-command) 1)
            "#
        .into(),
    );
    let object = compile_modules(&mut worker, &["lazy-test/init-fail.scm".into()]).unwrap();
    let mut foreground = Engine::new();
    register_async_lazy_plugin(
        "init-fail".into(),
        vec!["lazy-test/init-fail.scm".into()],
        vec!["init-fail-init!".into()],
        docs("init-fail-command"),
    )
    .unwrap();
    set_ready_plugin("init-fail", object);

    let error = activate_and_call(&mut foreground, "init-fail-command", Vec::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("initializer failed"));
    assert!(begin_activation("init-fail-command")
        .err()
        .unwrap()
        .to_string()
        .contains("initializer failed"));
}

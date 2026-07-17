use super::*;

#[test]
fn command_and_initializer_aliases_to_prefixed_imports_are_callable() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/alias-target.scm".into(),
        r#"
                (provide alias-target-command alias-target-init!)
                (define alias-target-state "cold")
                (define (alias-target-init!)
                  (set! alias-target-state "ready"))
                (define (alias-target-command suffix)
                  (string-append alias-target-state suffix))
            "#
        .into(),
    );
    engine.register_steel_module(
        "lazy-test/alias-wrapper.scm".into(),
        r#"
                (require (prefix-in target. "lazy-test/alias-target.scm"))
                (provide alias-command alias-init!)
                (define alias-command target.alias-target-command)
                (define alias-init! target.alias-target-init!)
            "#
        .into(),
    );
    register_lazy_plugin(
        "alias".into(),
        vec![
            "lazy-test/alias-target.scm".into(),
            "lazy-test/alias-wrapper.scm".into(),
        ],
        vec!["alias-init!".into()],
        docs("alias-command"),
    )
    .unwrap();

    let first = activate_and_call(
        &mut engine,
        "alias-command",
        vec!["!".into_steelval().unwrap()],
    )
    .unwrap();
    let second = activate_and_call(
        &mut engine,
        "alias-command",
        vec!["?".into_steelval().unwrap()],
    )
    .unwrap();

    assert_eq!(first.to_string(), "\"ready!\"");
    assert_eq!(second.to_string(), "\"ready?\"");
}

#[test]
fn command_bodies_can_call_prefixed_imports_after_lazy_activation() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    engine.register_steel_module(
        "lazy-test/prefixed-target.scm".into(),
        r#"
                (provide prefixed-target)
                (define (prefixed-target suffix)
                  (string-append "target:" suffix))
            "#
        .into(),
    );
    engine.register_steel_module(
        "lazy-test/prefixed-caller.scm".into(),
        r#"
                (require (prefix-in target. "lazy-test/prefixed-target.scm"))
                (provide prefixed-command prefixed-init!)
                (define prefixed-state "cold")
                (define (prefixed-init!)
                  (set! prefixed-state (target.prefixed-target "init")))
                (define (prefixed-command suffix)
                  (string-append prefixed-state ":" (target.prefixed-target suffix)))
            "#
        .into(),
    );
    register_lazy_plugin(
        "prefixed".into(),
        vec![
            "lazy-test/prefixed-target.scm".into(),
            "lazy-test/prefixed-caller.scm".into(),
        ],
        vec!["prefixed-init!".into()],
        docs("prefixed-command"),
    )
    .unwrap();

    let result = activate_and_call(
        &mut engine,
        "prefixed-command",
        vec!["run".into_steelval().unwrap()],
    )
    .unwrap();

    assert_eq!(result.to_string(), "\"target:init:target:run\"");
}

#[test]
fn ready_artifact_loads_without_foreground_module_sources() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let base = "lazy-test/source-free-base.scm".to_string();
    let command = "lazy-test/source-free-command.scm".to_string();
    let mut worker = Engine::new();
    worker.register_steel_module(
        base.clone(),
        "(provide source-free-base) (define source-free-base 40)".into(),
    );
    worker.register_steel_module(
        command.clone(),
        r#"
                (require "lazy-test/source-free-base.scm")
                (provide source-free-command)
                (define (source-free-command value) (+ source-free-base value))
            "#
        .into(),
    );
    let object = compile_modules(&mut worker, &[base.clone(), command.clone()]).unwrap();
    let mut foreground = Engine::new();
    register_async_lazy_plugin(
        "source-free".into(),
        vec![base, command],
        Vec::new(),
        docs("source-free-command"),
    )
    .unwrap();
    set_ready_plugin("source-free", object);

    assert_eq!(
        activate_and_call(
            &mut foreground,
            "source-free-command",
            vec![SteelVal::IntV(2)]
        )
        .unwrap(),
        SteelVal::IntV(42)
    );
}

#[test]
fn ready_artifact_command_bodies_can_call_prefixed_imports() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let target = "lazy-test/precompiled-prefixed-target.scm".to_string();
    let caller = "lazy-test/precompiled-prefixed-caller.scm".to_string();
    let mut worker = Engine::new();
    worker.register_steel_module(
        target.clone(),
        r#"
                (provide precompiled-prefixed-target)
                (define (precompiled-prefixed-target suffix)
                  (string-append "target:" suffix))
            "#
        .into(),
    );
    worker.register_steel_module(
        caller.clone(),
        r#"
                (require (prefix-in target. "lazy-test/precompiled-prefixed-target.scm"))
                (provide precompiled-prefixed-command)
                (define (precompiled-prefixed-command suffix)
                  (target.precompiled-prefixed-target suffix))
            "#
        .into(),
    );
    let object = compile_modules(&mut worker, &[target.clone(), caller.clone()]).unwrap();
    let mut foreground = Engine::new();
    register_async_lazy_plugin(
        "precompiled-prefixed".into(),
        vec![target, caller],
        Vec::new(),
        docs("precompiled-prefixed-command"),
    )
    .unwrap();
    set_ready_plugin("precompiled-prefixed", object);

    assert_eq!(
        activate_and_call(
            &mut foreground,
            "precompiled-prefixed-command",
            vec!["run".into_steelval().unwrap()],
        )
        .unwrap(),
        SteelVal::StringV("target:run".into())
    );
}

#[test]
fn ready_artifact_keeps_transitive_prefixed_dependencies_source_free() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let target = "lazy-test/transitive-prefixed-target.scm".to_string();
    let caller = "lazy-test/transitive-prefixed-caller.scm".to_string();
    let mut worker = Engine::new();
    worker.register_steel_module(
        target.clone(),
        r#"
                (provide transitive-prefixed-target)
                (define (transitive-prefixed-target suffix)
                  (string-append "target:" suffix))
            "#
        .into(),
    );
    worker.register_steel_module(
        caller.clone(),
        r#"
                (require (prefix-in target. "lazy-test/transitive-prefixed-target.scm"))
                (provide transitive-prefixed-command)
                (define (transitive-prefixed-command suffix)
                  (target.transitive-prefixed-target suffix))
            "#
        .into(),
    );
    let object = compile_modules(&mut worker, std::slice::from_ref(&caller)).unwrap();
    let mut foreground = Engine::new();
    register_async_lazy_plugin(
        "transitive-prefixed".into(),
        vec![caller],
        Vec::new(),
        docs("transitive-prefixed-command"),
    )
    .unwrap();
    set_ready_plugin("transitive-prefixed", object);

    assert_eq!(
        activate_and_call(
            &mut foreground,
            "transitive-prefixed-command",
            vec!["run".into_steelval().unwrap()],
        )
        .unwrap(),
        SteelVal::StringV("target:run".into())
    );
}

#[test]
fn command_bodies_can_call_prefixed_builtins_after_lazy_activation() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    register_prefixed_builtin(&mut engine);
    engine.register_steel_module(
        "lazy-test/prefixed-builtin-caller.scm".into(),
        r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide prefixed-builtin-command)
                (define (prefixed-builtin-command)
                  (builtin.builtin-target))
            "#
        .into(),
    );
    register_lazy_plugin(
        "prefixed-builtin".into(),
        vec!["lazy-test/prefixed-builtin-caller.scm".into()],
        Vec::new(),
        docs("prefixed-builtin-command"),
    )
    .unwrap();

    assert_eq!(
        activate_and_call(&mut engine, "prefixed-builtin-command", Vec::new()).unwrap(),
        SteelVal::StringV("builtin-target".into())
    );
}

#[test]
fn ready_artifact_command_bodies_can_call_prefixed_builtins() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let module = "lazy-test/precompiled-prefixed-builtin-caller.scm".to_string();
    let mut worker = Engine::new();
    register_prefixed_builtin(&mut worker);
    worker.register_steel_module(
        module.clone(),
        r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide precompiled-prefixed-builtin-command)
                (define (precompiled-prefixed-builtin-command)
                  (builtin.builtin-target))
            "#
        .into(),
    );
    let object = compile_modules(&mut worker, std::slice::from_ref(&module)).unwrap();
    let mut foreground = Engine::new();
    register_prefixed_builtin(&mut foreground);
    register_async_lazy_plugin(
        "precompiled-prefixed-builtin".into(),
        vec![module],
        Vec::new(),
        docs("precompiled-prefixed-builtin-command"),
    )
    .unwrap();
    set_ready_plugin("precompiled-prefixed-builtin", object);

    assert_eq!(
        activate_and_call(
            &mut foreground,
            "precompiled-prefixed-builtin-command",
            Vec::new(),
        )
        .unwrap(),
        SteelVal::StringV("builtin-target".into())
    );
}

#[test]
fn command_bodies_can_apply_prefixed_builtin_function_values() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let mut engine = Engine::new();
    register_prefixed_builtin(&mut engine);
    engine.register_steel_module(
        "lazy-test/prefixed-builtin-value-caller.scm".into(),
        r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide prefixed-builtin-value-command)
                (define (call-action action)
                  (action))
                (define (prefixed-builtin-value-command)
                  (call-action builtin.builtin-target))
            "#
        .into(),
    );
    register_lazy_plugin(
        "prefixed-builtin-value".into(),
        vec!["lazy-test/prefixed-builtin-value-caller.scm".into()],
        Vec::new(),
        docs("prefixed-builtin-value-command"),
    )
    .unwrap();

    assert_eq!(
        activate_and_call(&mut engine, "prefixed-builtin-value-command", Vec::new()).unwrap(),
        SteelVal::StringV("builtin-target".into())
    );
}

#[test]
fn ready_artifact_command_bodies_can_apply_prefixed_builtin_function_values() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let module = "lazy-test/precompiled-prefixed-builtin-value-caller.scm".to_string();
    let mut worker = Engine::new();
    register_prefixed_builtin(&mut worker);
    worker.register_steel_module(
        module.clone(),
        r#"
                (require-builtin lazy-test/prefixed-builtin as builtin.)
                (provide precompiled-prefixed-builtin-value-command)
                (define (call-action action)
                  (action))
                (define (precompiled-prefixed-builtin-value-command)
                  (call-action builtin.builtin-target))
            "#
        .into(),
    );
    let object = compile_modules(&mut worker, std::slice::from_ref(&module)).unwrap();
    let mut foreground = Engine::new();
    register_prefixed_builtin(&mut foreground);
    register_async_lazy_plugin(
        "precompiled-prefixed-builtin-value".into(),
        vec![module],
        Vec::new(),
        docs("precompiled-prefixed-builtin-value-command"),
    )
    .unwrap();
    set_ready_plugin("precompiled-prefixed-builtin-value", object);

    assert_eq!(
        activate_and_call(
            &mut foreground,
            "precompiled-prefixed-builtin-value-command",
            Vec::new(),
        )
        .unwrap(),
        SteelVal::StringV("builtin-target".into())
    );
}

#[test]
fn precompiled_artifacts_load_a_shared_dependency_once() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let shared = "lazy-test/shared-once.scm".to_string();
    let first = "lazy-test/shared-once-first.scm".to_string();
    let second = "lazy-test/shared-once-second.scm".to_string();
    let shared_source = r#"
            (provide shared-once-value)
            (lazy-test-shared-loaded!)
            (define shared-once-value 7)
        "#;
    let first_source = r#"
            (require "lazy-test/shared-once.scm")
            (provide shared-once-first)
            (define (shared-once-first) shared-once-value)
        "#;
    let second_source = r#"
            (require "lazy-test/shared-once.scm")
            (provide shared-once-second)
            (define (shared-once-second) (+ shared-once-value 1))
        "#;

    let mut first_worker = Engine::new();
    register_shared_load_counter(&mut first_worker, Arc::new(AtomicUsize::new(0)));
    first_worker.register_steel_module(shared.clone(), shared_source.into());
    first_worker.register_steel_module(first.clone(), first_source.into());
    let first_object =
        compile_modules(&mut first_worker, &[shared.clone(), first.clone()]).unwrap();

    let mut second_worker = Engine::new();
    register_shared_load_counter(&mut second_worker, Arc::new(AtomicUsize::new(0)));
    second_worker.register_steel_module(shared.clone(), shared_source.into());
    second_worker.register_steel_module(second.clone(), second_source.into());
    let second_object =
        compile_modules(&mut second_worker, &[shared.clone(), second.clone()]).unwrap();

    let count = Arc::new(AtomicUsize::new(0));
    let mut foreground = Engine::new();
    register_shared_load_counter(&mut foreground, count.clone());
    register_async_lazy_plugin(
        "shared-first".into(),
        vec![shared.clone(), first],
        Vec::new(),
        docs("shared-once-first"),
    )
    .unwrap();
    register_async_lazy_plugin(
        "shared-second".into(),
        vec![shared, second],
        Vec::new(),
        docs("shared-once-second"),
    )
    .unwrap();
    set_ready_plugin("shared-first", first_object);
    set_ready_plugin("shared-second", second_object);

    assert_eq!(
        activate_and_call(&mut foreground, "shared-once-first", Vec::new()).unwrap(),
        SteelVal::IntV(7)
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        activate_and_call(&mut foreground, "shared-once-second", Vec::new()).unwrap(),
        SteelVal::IntV(8)
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn foreground_fallback_refresh_prevents_later_precompiled_shared_reload() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let shared = "lazy-test/refresh-shared.scm".to_string();
    let foreground_module = "lazy-test/refresh-foreground.scm".to_string();
    let precompiled_module = "lazy-test/refresh-precompiled.scm".to_string();
    let shared_source = r#"
            (provide refresh-shared-value)
            (lazy-test-shared-loaded!)
            (define refresh-shared-value 3)
        "#;
    let foreground_source = r#"
            (require "lazy-test/refresh-shared.scm")
            (provide refresh-foreground-command)
            (define (refresh-foreground-command) refresh-shared-value)
        "#;
    let precompiled_source = r#"
            (require "lazy-test/refresh-shared.scm")
            (provide refresh-precompiled-command)
            (define (refresh-precompiled-command) (+ refresh-shared-value 4))
        "#;

    let count = Arc::new(AtomicUsize::new(0));
    let mut foreground = Engine::new();
    register_shared_load_counter(&mut foreground, count.clone());
    foreground.register_steel_module(shared.clone(), shared_source.into());
    foreground.register_steel_module(foreground_module.clone(), foreground_source.into());

    let mut worker = Engine::new();
    register_shared_load_counter(&mut worker, Arc::new(AtomicUsize::new(0)));
    worker.register_steel_module(shared.clone(), shared_source.into());
    worker.register_steel_module(precompiled_module.clone(), precompiled_source.into());
    let object =
        compile_modules(&mut worker, &[shared.clone(), precompiled_module.clone()]).unwrap();

    register_lazy_plugin(
        "refresh-foreground".into(),
        vec![shared.clone(), foreground_module],
        Vec::new(),
        docs("refresh-foreground-command"),
    )
    .unwrap();
    register_async_lazy_plugin(
        "refresh-precompiled".into(),
        vec![shared, precompiled_module],
        Vec::new(),
        docs("refresh-precompiled-command"),
    )
    .unwrap();
    set_ready_plugin("refresh-precompiled", object);

    assert_eq!(
        activate_and_call(&mut foreground, "refresh-foreground-command", Vec::new()).unwrap(),
        SteelVal::IntV(3)
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        activate_and_call(&mut foreground, "refresh-precompiled-command", Vec::new()).unwrap(),
        SteelVal::IntV(7)
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

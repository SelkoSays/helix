use super::*;

use std::{fs, path::Path};

fn write_source(directory: &Path, name: &str, source: &str) -> String {
    let path = directory.join(name);
    fs::write(&path, source).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn discovers_provided_documented_commands_without_evaluating_source() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();
    let source = write_source(
        directory.path(),
        "valid.scm",
        r#"
;;@lazy-command
;;@doc
;; Run the discovered command.
;;
;; Longer reference material is not used in command completion.
(define (discovered-command value) value)

(error "discovery must not evaluate source")
(provide discovered-command)
"#,
    );

    let commands = discover_commands(&[source]).unwrap();
    assert_eq!(
        commands.get("discovered-command").map(String::as_str),
        Some("Run the discovered command.")
    );
}

#[test]
fn discovered_registration_populates_completion_and_docs() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();
    let source = write_source(
        directory.path(),
        "registered.scm",
        r#"
;;@lazy-command
;;@doc
;; Registered from source.
(define registered-command (lambda () "ok"))
(provide registered-command)
"#,
    );

    register_discovered_lazy_plugin(
        "discovered".into(),
        vec![source.clone()],
        Vec::new(),
        vec![source],
    )
    .unwrap();

    assert!(recognizes("registered-command"));
    assert_eq!(
        documentation("registered-command").as_deref(),
        Some("Registered from source.")
    );
}

#[test]
fn rejects_orphan_markers_and_unprovided_commands() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();
    let orphan = write_source(
        directory.path(),
        "orphan.scm",
        ";;@lazy-command\n(define orphan 1)\n(provide orphan)\n",
    );
    assert!(discover_commands(&[orphan])
        .unwrap_err()
        .to_string()
        .contains("orphan"));

    let missing_provide = write_source(
        directory.path(),
        "missing-provide.scm",
        ";;@lazy-command\n;;@doc\n;; Hidden.\n(define (hidden-command) #true)\n",
    );
    assert!(discover_commands(&[missing_provide])
        .unwrap_err()
        .to_string()
        .contains("not provided"));
}

#[test]
fn rejects_commands_discovered_from_multiple_sources() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();
    let one = write_source(
        directory.path(),
        "one.scm",
        ";;@lazy-command\n;;@doc\n;; One.\n(define (duplicate-command) 1)\n(provide duplicate-command)\n",
    );
    let two = write_source(
        directory.path(),
        "two.scm",
        ";;@lazy-command\n;;@doc\n;; Two.\n(define (duplicate-command) 2)\n(provide duplicate-command)\n",
    );

    assert!(discover_commands(&[one, two])
        .unwrap_err()
        .to_string()
        .contains("more than once"));
}

#[test]
fn discovers_shorthand_and_value_definitions_with_first_paragraph_docs() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();
    let source = write_source(
        directory.path(),
        "forms.scm",
        r#"
;;@lazy-command
;;@doc
;; Shorthand summary line one.
;; Summary line two.
;;
;; Ignored detail.
(define (shorthand-command) #true)

;;@lazy-command
;;@doc
;; Value-style summary.
(define value-command (lambda () #true))

(provide shorthand-command value-command)
"#,
    );

    let commands = discover_commands(&[source]).unwrap();
    assert_eq!(commands.len(), 2);
    assert_eq!(
        commands.get("shorthand-command").map(String::as_str),
        Some("Shorthand summary line one. Summary line two.")
    );
    assert_eq!(
        commands.get("value-command").map(String::as_str),
        Some("Value-style summary.")
    );
}

#[test]
fn reports_source_lines_for_orphans_and_parse_failures() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();
    let orphan = write_source(
        directory.path(),
        "line.scm",
        "\n\n;;@lazy-command\n(define command #true)\n(provide command)\n",
    );
    assert!(discover_commands(&[orphan])
        .unwrap_err()
        .to_string()
        .contains("line.scm:3"));

    let malformed = write_source(
        directory.path(),
        "malformed.scm",
        ";;@lazy-command\n;;@doc\n;; Broken.\n(define (broken-command)\n",
    );
    let error = discover_commands(&[malformed]).unwrap_err().to_string();
    assert!(error.contains("malformed.scm:"), "{error}");
}

#[test]
fn validates_paths_encoding_and_request_limits() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();

    let text = write_source(directory.path(), "not-scheme.txt", "text");
    assert!(discover_commands(&[text])
        .unwrap_err()
        .to_string()
        .contains(".scm"));

    let invalid_utf8 = directory.path().join("invalid.scm");
    fs::write(&invalid_utf8, [0xff, 0xfe]).unwrap();
    assert!(
        discover_commands(&[invalid_utf8.to_string_lossy().into_owned()])
            .unwrap_err()
            .to_string()
            .contains("failed to read source")
    );

    let oversized = directory.path().join("oversized.scm");
    fs::write(&oversized, vec![b' '; MAX_SOURCE_BYTES as usize + 1]).unwrap();
    assert!(
        discover_commands(&[oversized.to_string_lossy().into_owned()])
            .unwrap_err()
            .to_string()
            .contains("exceeds")
    );

    let too_many = vec!["missing.scm".to_string(); MAX_SOURCE_COUNT + 1];
    assert!(discover_commands(&too_many)
        .unwrap_err()
        .to_string()
        .contains("at most"));
}

#[test]
fn cache_is_generation_scoped() {
    let _test = TEST_LOCK.lock().unwrap();
    reset_test_registry();
    let directory = tempfile::tempdir().unwrap();
    let source = write_source(
        directory.path(),
        "cached.scm",
        ";;@lazy-command\n;;@doc\n;; Before reset.\n(define (cached-command) #true)\n(provide cached-command)\n",
    );
    assert_eq!(
        discover_commands(std::slice::from_ref(&source))
            .unwrap()
            .get("cached-command")
            .map(String::as_str),
        Some("Before reset.")
    );

    fs::write(
        &source,
        ";;@lazy-command\n;;@doc\n;; After reset.\n(define (cached-command) #true)\n(provide cached-command)\n",
    )
    .unwrap();
    assert_eq!(
        discover_commands(std::slice::from_ref(&source))
            .unwrap()
            .get("cached-command")
            .map(String::as_str),
        Some("Before reset.")
    );

    reset_test_registry();
    assert_eq!(
        discover_commands(&[source])
            .unwrap()
            .get("cached-command")
            .map(String::as_str),
        Some("After reset.")
    );
}

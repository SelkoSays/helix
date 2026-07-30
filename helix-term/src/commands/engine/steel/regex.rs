//! A regular-expression engine for Steel plugins.
//!
//! Steel has no regex support, so plugins that need one today either shell out
//! or approximate it with string operations. This exposes the engine Helix
//! already depends on.
//!
//! Two rules shape the surface. Compilation returns nothing rather than raising
//! on an invalid pattern, because patterns arrive from live user typing and a
//! half-typed pattern is a normal state, not an error. And every offset that
//! crosses the boundary is a **character** index, because Steel strings are
//! indexed by character while the engine matches over bytes.

use helix_core::regex::Regex;
use steel::rvals::{AsRefSteelVal, Custom, IntoSteelVal};
use steel::steel_vm::builtin::BuiltInModule;
use steel::SteelVal;

use crate::commands::engine::steel::RegisterFn;

#[derive(Clone)]
pub(super) struct SteelRegex {
    pattern: String,
    regex: Regex,
    /// The same pattern anchored to both ends. A whole-string test cannot be
    /// derived from an unanchored search: `a|ab` finds `a` inside `ab`, which
    /// says nothing about whether the whole string matches.
    anchored: Regex,
}

impl Custom for SteelRegex {}

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn("regex?", is_regex)
        .register_fn("regex-compile", compile)
        .register_fn("regex-pattern", pattern)
        .register_fn("regex-full-match?", full_match)
        .register_fn("regex-find", find)
        .register_fn("regex-find-all", find_all)
        .register_fn("regex-replace-all", replace_all);
}

fn is_regex(value: SteelVal) -> bool {
    SteelRegex::as_ref(&value).is_ok()
}

fn compile(pattern: String) -> Option<SteelRegex> {
    let regex = Regex::new(&pattern).ok()?;
    let anchored = Regex::new(&format!("^(?:{pattern})$")).ok()?;
    Some(SteelRegex {
        pattern,
        regex,
        anchored,
    })
}

/// The pattern text, so a picker can render its active filter without keeping a
/// parallel copy of the string it compiled.
fn pattern(regex: &SteelRegex) -> String {
    regex.pattern.clone()
}

fn full_match(regex: &SteelRegex, text: String) -> bool {
    regex.anchored.is_match(&text)
}

fn find(regex: &SteelRegex, text: String) -> Option<SteelVal> {
    let found = regex.regex.find(&text)?;
    let mut converter = ByteToChar::new(&text);
    let start = converter.convert(found.start());
    let end = converter.convert(found.end());
    Some(range(start, end))
}

fn find_all(regex: &SteelRegex, text: String) -> SteelVal {
    let mut converter = ByteToChar::new(&text);
    SteelVal::ListV(
        regex
            .regex
            .find_iter(&text)
            .map(|found| {
                let start = converter.convert(found.start());
                let end = converter.convert(found.end());
                range(start, end)
            })
            .collect(),
    )
}

/// Replace every match, expanding `$1` and `${name}` capture references in the
/// replacement. This is what makes a regex replacement worth having over a
/// literal one.
fn replace_all(regex: &SteelRegex, text: String, replacement: String) -> String {
    regex
        .regex
        .replace_all(&text, replacement.as_str())
        .into_owned()
}

fn range(start: usize, end: usize) -> SteelVal {
    SteelVal::ListV(vec![start.into_steelval().unwrap(), end.into_steelval().unwrap()].into())
}

/// Converts byte offsets to character indices in one forward pass.
///
/// Matches arrive in increasing order, so counting characters from the previous
/// offset keeps the whole conversion linear in the text rather than quadratic
/// in the number of matches.
struct ByteToChar<'a> {
    text: &'a str,
    byte: usize,
    character: usize,
}

impl<'a> ByteToChar<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            byte: 0,
            character: 0,
        }
    }

    fn convert(&mut self, offset: usize) -> usize {
        if offset < self.byte {
            // Defensive: an out-of-order offset restarts the walk rather than
            // silently returning a stale index.
            self.byte = 0;
            self.character = 0;
        }
        self.character += self.text[self.byte..offset].chars().count();
        self.byte = offset;
        self.character
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(value: SteelVal) -> Vec<(usize, usize)> {
        let SteelVal::ListV(items) = value else {
            panic!("not a list")
        };
        items
            .iter()
            .map(|item| {
                let SteelVal::ListV(fields) = item else {
                    panic!("range is not a list")
                };
                (
                    integer(fields.get(0).unwrap()),
                    integer(fields.get(1).unwrap()),
                )
            })
            .collect()
    }

    fn integer(value: &SteelVal) -> usize {
        match value {
            SteelVal::IntV(value) => *value as usize,
            other => panic!("not an integer: {other:?}"),
        }
    }

    fn one(value: Option<SteelVal>) -> (usize, usize) {
        let SteelVal::ListV(fields) = value.expect("expected a match") else {
            panic!("range is not a list")
        };
        (
            integer(fields.get(0).unwrap()),
            integer(fields.get(1).unwrap()),
        )
    }

    #[test]
    fn an_invalid_pattern_compiles_to_nothing() {
        assert!(compile("(".to_string()).is_none());
        assert!(compile("[a-".to_string()).is_none());
        assert!(compile("a{2,1}".to_string()).is_none());
        assert!(compile("[0-9]+".to_string()).is_some());
    }

    #[test]
    fn the_pattern_text_survives_compilation() {
        let regex = compile("[a-z]+".to_string()).unwrap();
        assert_eq!(pattern(&regex), "[a-z]+");
    }

    #[test]
    fn full_match_is_not_a_substring_search() {
        // Deriving a whole-string test from an unanchored search would be
        // wrong: leftmost matching picks `a` out of `ab`, which says nothing
        // about whether the whole string matches.
        let regex = compile("a|ab".to_string()).unwrap();
        assert_eq!(one(find(&regex, "ab".to_string())), (0, 1));
        assert!(full_match(&regex, "ab".to_string()));
        assert!(full_match(&regex, "a".to_string()));
        assert!(!full_match(&regex, "abc".to_string()));

        let digits = compile("[0-9]".to_string()).unwrap();
        assert!(digits.regex.is_match("a1b"));
        assert!(!full_match(&digits, "a1b".to_string()));
        assert!(full_match(&digits, "1".to_string()));
    }

    #[test]
    fn offsets_are_character_indices() {
        let regex = compile("b".to_string()).unwrap();
        // "界" is three bytes and one character.
        assert_eq!(one(find(&regex, "界ab".to_string())), (2, 3));
        assert_eq!(
            ranges(find_all(&regex, "界b界b".to_string())),
            vec![(1, 2), (3, 4)]
        );
    }

    #[test]
    fn find_all_returns_non_overlapping_matches_in_order() {
        let regex = compile("aa".to_string()).unwrap();
        assert_eq!(
            ranges(find_all(&regex, "aaaa".to_string())),
            vec![(0, 2), (2, 4)]
        );
    }

    #[test]
    fn empty_matches_advance_rather_than_looping() {
        let regex = compile("x*".to_string()).unwrap();
        let found = ranges(find_all(&regex, "ab".to_string()));
        assert_eq!(found, vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn a_pattern_that_never_matches_finds_nothing() {
        let regex = compile("zzz".to_string()).unwrap();
        assert!(find(&regex, "abc".to_string()).is_none());
        assert!(ranges(find_all(&regex, "abc".to_string())).is_empty());
    }

    #[test]
    fn replacements_expand_captures() {
        let regex = compile("(\\w+)@(\\w+)".to_string()).unwrap();
        assert_eq!(
            replace_all(&regex, "a@b and c@d".to_string(), "$2:$1".to_string()),
            "b:a and d:c"
        );

        let named = compile("(?<word>[a-z]+)".to_string()).unwrap();
        assert_eq!(
            replace_all(&named, "hi there".to_string(), "<${word}>".to_string()),
            "<hi> <there>"
        );
    }

    #[test]
    fn a_literal_replacement_is_inserted_verbatim() {
        let regex = compile("cat".to_string()).unwrap();
        assert_eq!(
            replace_all(&regex, "cat cat".to_string(), "dog".to_string()),
            "dog dog"
        );
    }

    #[test]
    fn byte_to_char_walks_forward_across_calls() {
        let text = "界a界b";
        let mut converter = ByteToChar::new(text);
        assert_eq!(converter.convert(3), 1);
        assert_eq!(converter.convert(4), 2);
        assert_eq!(converter.convert(7), 3);
        // An out-of-order offset restarts rather than drifting.
        assert_eq!(converter.convert(3), 1);
    }
}

//! Shallow source discovery for native lazy commands.
//!
//! Discovery deliberately stops at parsing. It never expands macros, compiles
//! modules, evaluates forms, or follows `require` expressions.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use once_cell::sync::Lazy;
use steel::{
    parser::{ast::ExprKind, parser::Parser, tokens::TokenType},
    SteelErr,
};

use super::{lazy_error, steel_init_file};

pub(super) const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
pub(super) const MAX_SOURCE_COUNT: usize = 256;
const COMMAND_MARKER: &str = ";;@lazy-command";

#[derive(Clone)]
struct SourceCommands {
    commands: HashMap<String, String>,
}

static SOURCE_CACHE: Lazy<Mutex<HashMap<PathBuf, SourceCommands>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub(super) fn reset_discovery_cache() {
    SOURCE_CACHE.lock().unwrap().clear();
}

fn source_error(path: &Path, line: Option<usize>, message: impl AsRef<str>) -> SteelErr {
    let location = line
        .map(|line| format!("{}:{line}", path.display()))
        .unwrap_or_else(|| path.display().to_string());
    lazy_error(format!(
        "lazy command discovery failed at {location}: {}",
        message.as_ref()
    ))
}

fn resolve_source(source: &str) -> Result<PathBuf, SteelErr> {
    if source.is_empty() || source.contains(['\n', '\r', '\0']) {
        return Err(lazy_error(format!(
            "lazy discovery source contains an invalid path: {source:?}"
        )));
    }

    let source_path = Path::new(source);
    if source_path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("scm")
    {
        return Err(lazy_error(format!(
            "lazy discovery source must be a .scm file: {source:?}"
        )));
    }

    let mut candidates = Vec::new();
    if source_path.is_absolute() {
        candidates.push(source_path.to_path_buf());
    } else {
        if let Some(config_dir) = steel_init_file().parent() {
            candidates.push(config_dir.join(source_path));
        }
        for runtime_dir in helix_loader::runtime_dirs() {
            candidates.push(runtime_dir.join(source_path));
        }
    }

    let resolved = candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| lazy_error(format!("lazy discovery source was not found: {source:?}")))?;
    fs::canonicalize(&resolved).map_err(|error| {
        lazy_error(format!(
            "failed to canonicalize lazy discovery source {:?}: {error}",
            resolved.display().to_string()
        ))
    })
}

fn line_number(source: &str, offset: usize) -> usize {
    source
        .as_bytes()
        .iter()
        .take(offset.min(source.len()))
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn identifier(expr: &ExprKind) -> Option<String> {
    let ExprKind::Atom(atom) = expr else {
        return None;
    };
    let TokenType::Identifier(identifier) = &atom.syn.ty else {
        return None;
    };
    Some(identifier.resolve().to_owned())
}

fn string_literal(expr: &ExprKind) -> Option<String> {
    let ExprKind::Atom(atom) = expr else {
        return None;
    };
    let TokenType::StringLiteral(value) = &atom.syn.ty else {
        return None;
    };
    Some(value.resolve().to_owned())
}

fn first_paragraph(documentation: &str) -> String {
    documentation
        .lines()
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn marker_before_definition(lines: &[&str], definition_line: usize) -> Option<usize> {
    let mut index = definition_line.saturating_sub(1);
    while index > 0 {
        index -= 1;
        let raw = lines[index];
        let trimmed = raw.trim();
        if trimmed == COMMAND_MARKER && raw.starts_with(COMMAND_MARKER) {
            return Some(index + 1);
        }
        if trimmed.is_empty() || raw.starts_with(";;") {
            continue;
        }
        return None;
    }
    None
}

fn scan_source(path: &Path) -> Result<SourceCommands, SteelErr> {
    if let Some(cached) = SOURCE_CACHE.lock().unwrap().get(path).cloned() {
        return Ok(cached);
    }

    let source = fs::read_to_string(path)
        .map_err(|error| source_error(path, None, format!("failed to read source: {error}")))?;
    let lines = source.lines().collect::<Vec<_>>();
    let marker_lines = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            (line.trim() == COMMAND_MARKER && line.starts_with(COMMAND_MARKER)).then_some(index + 1)
        })
        .collect::<HashSet<_>>();

    let mut consumed_markers = HashSet::new();
    let mut provides = HashSet::new();
    let mut documented = Vec::new();

    for parsed in Parser::doc_comment_parser(&source, None) {
        let expression = parsed.map_err(|error| {
            source_error(
                path,
                Some(line_number(&source, error.span().start() as usize)),
                error.to_string(),
            )
        })?;

        let ExprKind::List(list) = &expression else {
            continue;
        };
        let mut arguments = list.args.iter();
        let Some(head) = arguments.next().and_then(identifier) else {
            continue;
        };

        if head == "provide" {
            provides.extend(arguments.filter_map(identifier));
            continue;
        }
        if head != "@doc" {
            continue;
        }

        let Some(documentation) = arguments.next().and_then(string_literal) else {
            continue;
        };
        let Some(ExprKind::Define(definition)) = arguments.next() else {
            continue;
        };
        let Some(name) = identifier(&definition.name) else {
            continue;
        };
        let definition_line = line_number(&source, definition.location.span.start() as usize);
        let Some(marker_line) = marker_before_definition(&lines, definition_line) else {
            continue;
        };
        consumed_markers.insert(marker_line);

        let summary = first_paragraph(&documentation);
        if summary.is_empty() {
            return Err(source_error(
                path,
                Some(definition_line),
                format!("lazy command {name:?} has empty documentation"),
            ));
        }
        documented.push((name, summary, definition_line));
    }

    if let Some(line) = marker_lines.difference(&consumed_markers).min() {
        return Err(source_error(
            path,
            Some(*line),
            "orphan ;;@lazy-command marker",
        ));
    }

    let mut commands = HashMap::new();
    for (name, documentation, line) in documented {
        if !provides.contains(&name) {
            return Err(source_error(
                path,
                Some(line),
                format!("lazy command {name:?} is not provided by this source"),
            ));
        }
        if commands.insert(name.clone(), documentation).is_some() {
            return Err(source_error(
                path,
                Some(line),
                format!("lazy command {name:?} is declared more than once"),
            ));
        }
    }

    let result = SourceCommands { commands };
    SOURCE_CACHE
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), result.clone());
    Ok(result)
}

pub(super) fn discover_commands(sources: &[String]) -> Result<HashMap<String, String>, SteelErr> {
    if sources.is_empty() {
        return Err(lazy_error(
            "lazy command discovery requires at least one source",
        ));
    }
    if sources.len() > MAX_SOURCE_COUNT {
        return Err(lazy_error(format!(
            "lazy command discovery accepts at most {MAX_SOURCE_COUNT} sources"
        )));
    }

    let mut resolved = Vec::with_capacity(sources.len());
    let mut total_bytes = 0_u64;
    for source in sources {
        let path = resolve_source(source)?;
        let bytes = fs::metadata(&path)
            .map_err(|error| source_error(&path, None, format!("failed to stat source: {error}")))?
            .len();
        if bytes > MAX_SOURCE_BYTES {
            return Err(source_error(
                &path,
                None,
                format!("source exceeds the {MAX_SOURCE_BYTES}-byte limit"),
            ));
        }
        total_bytes = total_bytes
            .checked_add(bytes)
            .ok_or_else(|| lazy_error("lazy command discovery aggregate source size overflowed"))?;
        if total_bytes > MAX_REQUEST_BYTES {
            return Err(lazy_error(format!(
                "lazy command discovery sources exceed the {MAX_REQUEST_BYTES}-byte aggregate limit"
            )));
        }
        resolved.push(path);
    }
    resolved.sort();
    resolved.dedup();

    let mut commands = HashMap::new();
    for path in resolved {
        for (command, documentation) in scan_source(&path)?.commands {
            if let Some(existing) = commands.insert(command.clone(), documentation.clone()) {
                return Err(lazy_error(format!(
                    "lazy command {command:?} was discovered more than once with documentation {existing:?} and {documentation:?}"
                )));
            }
        }
    }
    Ok(commands)
}

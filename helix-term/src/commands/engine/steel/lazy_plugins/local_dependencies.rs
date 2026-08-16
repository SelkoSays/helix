//! Trusted workspace-local plugin dependency declarations.

use std::{collections::HashSet, path::Path};

use steel::parser::{ast::ExprKind, parser::Parser, tokens::TokenType};

use super::*;

const DECLARATION: &str = "local-plugin-dependencies";

fn identifier(expr: &ExprKind) -> Option<String> {
    let ExprKind::Atom(atom) = expr else {
        return None;
    };
    let TokenType::Identifier(identifier) = &atom.syn.ty else {
        return None;
    };
    Some(identifier.resolve().to_owned())
}

pub(super) fn parse_local_dependencies(source: &str, path: &Path) -> Result<Vec<String>, SteelErr> {
    let expressions = Parser::parse(source).map_err(|error| {
        lazy_error(format!(
            "unable to parse workspace Steel config {}: {error}",
            path.display()
        ))
    })?;
    let mut declaration = None;

    for expression in expressions {
        let ExprKind::List(form) = expression else {
            continue;
        };
        if form.args.first().and_then(identifier).as_deref() != Some(DECLARATION) {
            continue;
        }
        if declaration.is_some() {
            return Err(lazy_error(format!(
                "workspace Steel config {} contains more than one {DECLARATION} declaration",
                path.display()
            )));
        }
        let [_, ExprKind::Quote(quoted)] = form.args.as_slice() else {
            return Err(lazy_error(format!(
                "{DECLARATION} in {} must contain one literal quoted list of plugin IDs",
                path.display()
            )));
        };
        let ExprKind::List(ids) = &quoted.expr else {
            return Err(lazy_error(format!(
                "{DECLARATION} in {} must contain one literal quoted list of plugin IDs",
                path.display()
            )));
        };
        if ids.improper {
            return Err(lazy_error(format!(
                "{DECLARATION} in {} must use a proper list",
                path.display()
            )));
        }
        let mut seen = HashSet::new();
        let mut dependencies = Vec::with_capacity(ids.args.len());
        for id in &ids.args {
            let Some(id) = identifier(id) else {
                return Err(lazy_error(format!(
                    "{DECLARATION} in {} accepts only literal symbol IDs",
                    path.display()
                )));
            };
            if !seen.insert(id.clone()) {
                return Err(lazy_error(format!(
                    "duplicate local plugin dependency {id:?} in {}",
                    path.display()
                )));
            }
            dependencies.push(id);
        }
        declaration = Some(dependencies);
    }

    Ok(declaration.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_literal_top_level_declaration() {
        let path = Path::new("/workspace/.helix/local.scm");
        assert_eq!(
            parse_local_dependencies(
                "(local-plugin-dependencies '(tasks jobs))\n(define local-value 1)",
                path,
            )
            .unwrap(),
            ["tasks", "jobs"]
        );
        assert!(
            parse_local_dependencies("(local-plugin-dependencies (list 'tasks))", path).is_err()
        );
        assert!(
            parse_local_dependencies("(local-plugin-dependencies '(tasks tasks))", path).is_err()
        );
        assert!(parse_local_dependencies(
            "(local-plugin-dependencies '(tasks))\n(local-plugin-dependencies '(jobs))",
            path,
        )
        .is_err());
    }

    #[test]
    fn ignores_nested_calls() {
        assert!(parse_local_dependencies(
            "(define (configure) (local-plugin-dependencies '(tasks)))",
            Path::new("local.scm"),
        )
        .unwrap()
        .is_empty());
    }
}

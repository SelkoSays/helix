use helix_core::{Tendril, Transaction};
use steel::{
    steel_vm::{builtin::BuiltInModule, register_fn::RegisterFn},
    SteelVal,
};

use super::{Context, CTX};

pub(super) fn register(module: &mut BuiltInModule) {
    module.register_fn_with_ctx(CTX, "apply-custom-text-edits!", apply_custom_text_edits);
    module.register_fn_with_ctx(
        CTX,
        "apply-custom-text-edits-to-path!",
        apply_custom_text_edits_to_path,
    );
}

/// Apply edits to the document for `path`, which need not be focused or even
/// open, so one command can edit many files while each keeps its own undo
/// history. The document is left modified rather than written, so the whole
/// change stays reversible until the user saves.
fn apply_custom_text_edits_to_path(
    cx: &mut Context,
    path: String,
    edits: SteelVal,
) -> anyhow::Result<bool> {
    let path = helix_stdx::path::canonicalize(std::path::PathBuf::from(path));
    let doc_id = match cx.editor.document_by_path(&path).map(|doc| doc.id()) {
        Some(doc_id) => doc_id,
        // Load without displaying: a project-wide edit must not disturb the
        // window layout or move the user's focus.
        None => cx.editor.open(&path, helix_view::editor::Action::Load)?,
    };

    let view_id = cx.editor.tree.focus;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return Ok(false);
    };

    let edits = parse_edits(doc.text().len_chars(), edits)?;
    if edits.is_empty() {
        return Ok(true);
    }

    // A document that was never displayed has no selection for this view yet.
    doc.ensure_view_init(view_id);
    let transaction = Transaction::change(doc.text(), edits.into_iter());
    Ok(doc.apply(&transaction, view_id))
}

fn apply_custom_text_edits(cx: &mut Context, edits: SteelVal) -> anyhow::Result<bool> {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return Ok(false);
    };

    let edits = parse_edits(doc.text().len_chars(), edits)?;
    if edits.is_empty() {
        return Ok(true);
    }

    let transaction = Transaction::change(doc.text(), edits.into_iter());
    Ok(doc.apply(&transaction, view_id))
}

fn parse_edits(
    char_len: usize,
    value: SteelVal,
) -> anyhow::Result<Vec<(usize, usize, Option<Tendril>)>> {
    let SteelVal::ListV(rows) = value else {
        anyhow::bail!("custom text edits must be a list");
    };

    let mut edits = Vec::with_capacity(rows.len());
    let mut previous_end = 0;
    for row in rows.iter() {
        let SteelVal::ListV(values) = row else {
            anyhow::bail!("each custom text edit must be a list");
        };
        let values: Vec<_> = values.iter().collect();
        let [start, end, replacement] = values.as_slice() else {
            anyhow::bail!("each custom text edit must contain start, end, and replacement");
        };
        let start = integer(start).ok_or_else(|| {
            anyhow::anyhow!("custom text edit start must be a non-negative integer")
        })?;
        let end = integer(end).ok_or_else(|| {
            anyhow::anyhow!("custom text edit end must be a non-negative integer")
        })?;
        let replacement = string(replacement)
            .ok_or_else(|| anyhow::anyhow!("custom text edit replacement must be a string"))?;

        if start > end || end > char_len {
            anyhow::bail!(
                "custom text edit range {start}..{end} is outside document length {char_len}"
            );
        }
        if !edits.is_empty() && start < previous_end {
            anyhow::bail!("custom text edits must be sorted and non-overlapping");
        }
        previous_end = end;
        edits.push((
            start,
            end,
            (!replacement.is_empty()).then(|| replacement.into()),
        ));
    }

    Ok(edits)
}

fn integer(value: &SteelVal) -> Option<usize> {
    match value {
        SteelVal::IntV(value) if *value >= 0 => Some(*value as usize),
        _ => None,
    }
}

fn string(value: &SteelVal) -> Option<String> {
    match value {
        SteelVal::StringV(value) => Some(value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(values: Vec<SteelVal>) -> SteelVal {
        SteelVal::ListV(values.into())
    }

    fn string(value: &str) -> SteelVal {
        SteelVal::StringV(value.into())
    }

    #[test]
    fn parses_valid_unicode_character_edits() {
        let edits = parse_edits(
            8,
            list(vec![
                list(vec![SteelVal::IntV(1), SteelVal::IntV(1), string("λ")]),
                list(vec![SteelVal::IntV(4), SteelVal::IntV(7), string("")]),
            ]),
        )
        .unwrap();

        assert_eq!(edits.len(), 2);
        assert_eq!((edits[0].0, edits[0].1), (1, 1));
        assert_eq!(edits[0].2.as_ref().unwrap().chars().count(), 1);
        assert!(edits[1].2.is_none());
    }

    #[test]
    fn rejects_invalid_or_overlapping_edits() {
        let overlapping = list(vec![
            list(vec![SteelVal::IntV(1), SteelVal::IntV(4), string("a")]),
            list(vec![SteelVal::IntV(3), SteelVal::IntV(5), string("b")]),
        ]);
        assert!(parse_edits(8, overlapping).is_err());

        let out_of_bounds = list(vec![list(vec![
            SteelVal::IntV(2),
            SteelVal::IntV(9),
            string("a"),
        ])]);
        assert!(parse_edits(8, out_of_bounds).is_err());
    }
}

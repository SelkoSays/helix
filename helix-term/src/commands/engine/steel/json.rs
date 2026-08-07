use serde_json::Value;
use steel::{rvals::SteelVal, steel_vm::builtin::BuiltInModule};

use super::RegisterFn;

const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_ITEMS: usize = 100_000;

fn ensure_size(input: &str, max_bytes: usize) -> anyhow::Result<()> {
    if input.len() > max_bytes {
        anyhow::bail!(
            "JSON input is {} bytes, exceeding the {} byte limit",
            input.len(),
            max_bytes
        );
    }
    Ok(())
}

fn into_steel(value: Value) -> anyhow::Result<SteelVal> {
    SteelVal::try_from(value).map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn json_parse_bounded(input: String, max_bytes: usize) -> anyhow::Result<SteelVal> {
    ensure_size(&input, max_bytes)?;
    into_steel(serde_json::from_str(&input)?)
}

fn json_parse(input: String) -> anyhow::Result<SteelVal> {
    json_parse_bounded(input, DEFAULT_MAX_BYTES)
}

fn json_parse_lines_bounded(
    input: String,
    max_bytes: usize,
    max_items: usize,
) -> anyhow::Result<SteelVal> {
    ensure_size(&input, max_bytes)?;

    let mut values = Vec::new();
    for (line_index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if values.len() >= max_items {
            anyhow::bail!("JSON-lines input exceeds the {max_items} item limit");
        }
        let value = serde_json::from_str(line)
            .map_err(|error| anyhow::anyhow!("invalid JSON on line {}: {error}", line_index + 1))?;
        values.push(value);
    }

    into_steel(Value::Array(values))
}

fn json_parse_lines(input: String) -> anyhow::Result<SteelVal> {
    json_parse_lines_bounded(input, DEFAULT_MAX_BYTES, DEFAULT_MAX_ITEMS)
}

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn("json-parse", json_parse)
        .register_fn("json-parse-bounded", json_parse_bounded)
        .register_fn("json-parse-lines", json_parse_lines)
        .register_fn("json-parse-lines-bounded", json_parse_lines_bounded);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_objects_and_unicode() {
        let value = json_parse_bounded(r#"{"name":"界","ok":true,"none":null}"#.into(), 128);
        assert!(value.is_ok());
    }

    #[test]
    fn enforces_byte_limit() {
        let error = json_parse_bounded(r#"{"long":true}"#.into(), 4).unwrap_err();
        assert!(error.to_string().contains("byte limit"));
    }

    #[test]
    fn reports_json_lines_line_number() {
        let error = json_parse_lines_bounded("{\"ok\":1}\nnope\n".into(), 128, 10).unwrap_err();
        assert!(error.to_string().contains("line 2"));
    }

    #[test]
    fn ignores_blank_json_lines_and_enforces_item_limit() {
        assert!(json_parse_lines_bounded("1\n\n2\n".into(), 128, 2).is_ok());
        let error = json_parse_lines_bounded("1\n2\n".into(), 128, 1).unwrap_err();
        assert!(error.to_string().contains("item limit"));
    }
}

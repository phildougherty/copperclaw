//! Render command-response payloads for the terminal.
//!
//! The host returns arbitrary JSON; this module turns a list-of-objects
//! shape (`[ { "id": ..., "name": ... }, … ]`) into a simple padded text
//! table. Anything more complex (or anything the caller wants raw) goes
//! through [`render_json_pretty`], which is what `--json` triggers.

use crate::style::Palette;
use serde_json::Value;
use std::fmt::Write as _;

/// Render `value` to a human-friendly string with no styling. Equivalent
/// to [`render_with`] under [`Palette::plain`].
pub fn render(value: &Value) -> String {
    render_with(value, Palette::plain())
}

/// Render `value` to a human-friendly string.
///
/// Rules:
/// 1. If `value` is an array of objects (homogeneous or not), render a
///    text table whose columns are the union of object keys in iteration
///    order.
/// 2. If `value` is a single object, render a two-column key/value table.
/// 3. Otherwise, fall back to pretty JSON.
///
/// Table headers are emphasised via `palette` (bold when color is on;
/// verbatim otherwise). JSON fallback is never styled.
pub fn render_with(value: &Value, palette: Palette) -> String {
    match value {
        Value::Array(items) if items.iter().all(Value::is_object) => render_table(items, palette),
        Value::Object(map) => render_kv(map, palette),
        _ => render_json_pretty(value),
    }
}

/// Pretty-print JSON unconditionally.
pub fn render_json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn render_table(items: &[Value], palette: Palette) -> String {
    if items.is_empty() {
        return "(empty)".to_string();
    }
    // Collect column order: first-seen across all rows.
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        if let Value::Object(map) = item {
            for k in map.keys() {
                if !columns.iter().any(|c| c == k) {
                    columns.push(k.clone());
                }
            }
        }
    }
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(items.len());
    for item in items {
        let mut row = Vec::with_capacity(columns.len());
        if let Value::Object(map) = item {
            for col in &columns {
                row.push(map.get(col).map(stringify).unwrap_or_default());
            }
        }
        rows.push(row);
    }
    let headers: Vec<String> = columns.iter().map(|c| c.to_uppercase()).collect();
    format_table(&headers, &rows, palette)
}

fn render_kv(map: &serde_json::Map<String, Value>, palette: Palette) -> String {
    if map.is_empty() {
        return "(empty)".to_string();
    }
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(map.len());
    for (k, v) in map {
        rows.push(vec![k.clone(), stringify(v)]);
    }
    format_table(&["KEY".to_string(), "VALUE".to_string()], &rows, palette)
}

fn stringify(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => v.to_string(),
    }
}

fn format_table(headers: &[String], rows: &[Vec<String>], palette: Palette) -> String {
    let cols = headers.len();
    if cols == 0 {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(String::len).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() && cell.chars().count() > widths[i] {
                widths[i] = cell.chars().count();
            }
        }
    }
    let mut out = String::new();
    // Style the header line as a whole *after* padding so ANSI escape
    // bytes never participate in the column-width math.
    let mut header_line = String::new();
    write_row(&mut header_line, headers, &widths);
    let trimmed = header_line.trim_end_matches('\n');
    out.push_str(&palette.header(trimmed));
    out.push('\n');
    writeln_separator(&mut out, &widths);
    for row in rows {
        write_row(&mut out, row, &widths);
    }
    out
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let pad = widths.get(i).copied().unwrap_or(0);
        let actual = cell.chars().count();
        let _ = write!(out, "{cell}");
        if i + 1 != cells.len() && actual < pad {
            for _ in 0..pad - actual {
                out.push(' ');
            }
        }
    }
    out.push('\n');
}

fn writeln_separator(out: &mut String, widths: &[usize]) {
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        for _ in 0..*w {
            out.push('-');
        }
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_list_renders_marker() {
        assert_eq!(render(&json!([])), "(empty)");
    }

    #[test]
    fn single_row_table() {
        let v = json!([{"id":"ag_1","name":"Greeter"}]);
        let out = render(&v);
        assert!(out.contains("ID"));
        assert!(out.contains("NAME"));
        assert!(out.contains("ag_1"));
        assert!(out.contains("Greeter"));
        // Separator under the headers.
        assert!(out.contains("---"));
    }

    #[test]
    fn multi_row_table_aligns_columns() {
        let v = json!([
            {"id":"ag_1","name":"a"},
            {"id":"ag_22","name":"longer"},
        ]);
        let out = render(&v);
        let lines: Vec<&str> = out.lines().collect();
        // Header, separator, two data rows.
        assert_eq!(lines.len(), 4);
        // The data rows should be the same length once trailing whitespace is allowed.
        assert!(lines[2].starts_with("ag_1"));
        assert!(lines[3].starts_with("ag_22"));
    }

    #[test]
    fn heterogeneous_row_objects_union_columns() {
        let v = json!([
            {"id":"x","name":"A"},
            {"id":"y","status":"running"},
        ]);
        let out = render(&v);
        assert!(out.contains("ID"));
        assert!(out.contains("NAME"));
        assert!(out.contains("STATUS"));
    }

    #[test]
    fn single_object_renders_kv() {
        let v = json!({"id":"ag_1","name":"Greeter","provider":null});
        let out = render(&v);
        assert!(out.contains("KEY"));
        assert!(out.contains("VALUE"));
        assert!(out.contains("ag_1"));
    }

    #[test]
    fn empty_object_renders_marker() {
        assert_eq!(render(&json!({})), "(empty)");
    }

    #[test]
    fn scalar_falls_back_to_pretty_json() {
        let out = render(&json!(42));
        assert!(out.contains("42"));
        let out = render(&json!("hello"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn array_of_scalars_falls_back_to_json() {
        let out = render(&json!(["a", "b"]));
        // Not an array of objects → pretty JSON.
        assert!(out.contains('['));
    }

    #[test]
    fn nested_values_serialize_via_stringify() {
        let v = json!([{"id":"x","tags":["a","b"],"meta":{"k":1}}]);
        let out = render(&v);
        assert!(out.contains("[\"a\",\"b\"]"));
        assert!(out.contains("{\"k\":1}"));
    }

    #[test]
    fn null_cell_renders_empty() {
        let v = json!([{"id":"x","provider":null}]);
        let out = render(&v);
        let last = out.lines().last().unwrap();
        // The provider column should be missing/empty.
        assert!(last.starts_with('x'));
    }

    #[test]
    fn bool_and_number_cells_stringify() {
        let v = json!([{"on":true,"count":3}]);
        let out = render(&v);
        assert!(out.contains("true"));
        assert!(out.contains('3'));
    }

    #[test]
    fn json_passthrough_pretty_prints() {
        let pretty = render_json_pretty(&json!({"a":1}));
        assert!(pretty.contains('\n'));
        assert!(pretty.contains("\"a\""));
    }

    #[test]
    fn render_json_pretty_handles_non_object() {
        let out = render_json_pretty(&json!([1, 2, 3]));
        assert!(out.contains('1'));
    }

    #[test]
    fn format_table_with_no_columns_is_empty() {
        assert_eq!(format_table(&[], &[], Palette::plain()), "");
    }

    #[test]
    fn plain_palette_output_has_no_ansi() {
        let v = json!([{"id":"ag_1","name":"Greeter"}]);
        let out = render_with(&v, Palette::plain());
        assert!(!out.contains('\u{1b}'));
        assert_eq!(out, render(&v));
    }

    #[test]
    fn colored_palette_bolds_headers_only() {
        let v = json!([{"id":"ag_1","name":"Greeter"}]);
        let out = render_with(&v, Palette::new(true));
        let lines: Vec<&str> = out.lines().collect();
        // Header line carries ANSI, data rows do not.
        assert!(lines[0].contains('\u{1b}'));
        assert!(!lines[2].contains('\u{1b}'));
        // Stripping the escapes yields the plain rendering.
        let stripped = out.replace("\u{1b}[1m", "").replace("\u{1b}[0m", "");
        assert_eq!(stripped, render(&v));
    }

    #[test]
    fn colored_kv_headers_do_not_break_alignment() {
        let v = json!({"id":"ag_1","name":"Greeter"});
        let plain = render_with(&v, Palette::plain());
        let colored = render_with(&v, Palette::new(true));
        let stripped = colored.replace("\u{1b}[1m", "").replace("\u{1b}[0m", "");
        assert_eq!(stripped, plain);
    }

    #[test]
    fn json_fallback_is_never_styled() {
        let out = render_with(&json!([1, 2, 3]), Palette::new(true));
        assert!(!out.contains('\u{1b}'));
    }
}

//! Structured Markdown document preview for Steel plugins.
//!
//! Parsing, layout, source mapping and concrete syntax styles stay native.
//! Steel owns the scratch-buffer lifecycle and interaction policy.

use std::{collections::HashMap, ops::Range, path::Path, str::FromStr, sync::Arc};

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::{
    annotations::custom_text::{CustomHighlight, CustomHighlightStyle, CustomTextAnnotations},
    editor::Action,
    graphics::Style,
};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use steel::{
    rvals::{AsRefSteelVal, Custom, IntoSteelVal},
    steel_vm::{builtin::BuiltInModule, register_fn::RegisterFn},
    SteelVal,
};

use super::{syntax_highlight, Context, CTX};

const MIN_WIDTH: usize = 20;
const MAX_WIDTH: usize = 500;
const DECORATION_NAMESPACE: &str = "markdown-preview";

#[derive(Clone, Debug)]
enum RenderStyle {
    Scope(String),
    Concrete(Style),
}

#[derive(Clone, Debug)]
struct StyledRange {
    range: Range<usize>,
    style: RenderStyle,
}

#[derive(Clone, Debug)]
struct SourceMap {
    output: Range<usize>,
    source: Range<usize>,
    node: String,
}

#[derive(Clone, Debug)]
struct Heading {
    level: usize,
    title: String,
    anchor: String,
    output: usize,
    source: Range<usize>,
}

#[derive(Clone, Debug)]
struct Link {
    label: String,
    destination: String,
    resolved: bool,
    output: Range<usize>,
    source: Range<usize>,
}

#[derive(Clone, Debug)]
struct CodeBlock {
    language: String,
    code: String,
    output: Range<usize>,
    source: Range<usize>,
}

#[derive(Clone, Debug)]
struct Media {
    alt: String,
    destination: String,
    resolved: bool,
    remote: bool,
    output: Range<usize>,
    source: Range<usize>,
}

#[derive(Clone, Debug)]
struct MarkdownRender {
    text: String,
    source_text: String,
    width: usize,
    styles: Vec<StyledRange>,
    mappings: Vec<SourceMap>,
    anchors: HashMap<String, usize>,
    headings: Vec<Heading>,
    links: Vec<Link>,
    code_blocks: Vec<CodeBlock>,
    media: Vec<Media>,
}

#[derive(Clone)]
struct SteelMarkdownRender(Arc<MarkdownRender>);

impl Custom for SteelMarkdownRender {}

#[derive(Default)]
struct HeadingState {
    level: usize,
    title: String,
    output: usize,
    source: Range<usize>,
}

struct LinkState {
    destination: String,
    resolved: bool,
    output: usize,
    source: Range<usize>,
    label: String,
}

struct ImageState {
    destination: String,
    resolved: bool,
    remote: bool,
    source: Range<usize>,
    alt: String,
}

struct NativeCodeState {
    language: String,
    code: String,
    source: Range<usize>,
}

#[derive(Default)]
struct TableCell {
    text: String,
}

struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<TableCell>>,
    row: Vec<TableCell>,
    cell: Option<TableCell>,
    header_rows: usize,
    in_head: bool,
    source: Range<usize>,
}

struct Builder {
    text: String,
    width: usize,
    column: usize,
    styles: Vec<StyledRange>,
    mappings: Vec<SourceMap>,
    anchors: HashMap<String, usize>,
    headings: Vec<Heading>,
    links: Vec<Link>,
    code_blocks: Vec<CodeBlock>,
    media: Vec<Media>,
    node_index: usize,
    heading_ids: HashMap<String, usize>,
}

impl Builder {
    fn new(width: usize) -> Self {
        Self {
            text: String::new(),
            width: width.clamp(MIN_WIDTH, MAX_WIDTH),
            column: 0,
            styles: Vec::new(),
            mappings: Vec::new(),
            anchors: HashMap::new(),
            headings: Vec::new(),
            links: Vec::new(),
            code_blocks: Vec::new(),
            media: Vec::new(),
            node_index: 0,
            heading_ids: HashMap::new(),
        }
    }

    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    fn at_line_start(&self) -> bool {
        self.column == 0
    }

    fn newline(&mut self) {
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.column = 0;
    }

    fn blank_line(&mut self) {
        self.newline();
        if !self.text.ends_with("\n\n") {
            self.text.push('\n');
        }
        self.column = 0;
    }

    fn emit_raw(
        &mut self,
        value: &str,
        scopes: &[&str],
        source: Range<usize>,
        node: &str,
    ) -> Range<usize> {
        let value = sanitize_controls(value);
        let start = self.char_len();
        self.text.push_str(&value);
        if let Some(last) = value.rsplit('\n').next() {
            self.column = if value.contains('\n') {
                UnicodeWidthStr::width(last)
            } else {
                self.column + UnicodeWidthStr::width(last)
            };
        }
        let end = self.char_len();
        if start < end {
            for scope in scopes {
                self.styles.push(StyledRange {
                    range: start..end,
                    style: RenderStyle::Scope((*scope).to_string()),
                });
            }
            self.mappings.push(SourceMap {
                output: start..end,
                source,
                node: node.to_string(),
            });
        }
        start..end
    }

    fn emit_wrapped(
        &mut self,
        value: &str,
        scopes: &[&str],
        source: Range<usize>,
        node: &str,
        prefix: &str,
    ) -> Range<usize> {
        let start = self.char_len();
        let mut first = true;
        for word in sanitize_controls(value).split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            let separator = usize::from(!first && !self.at_line_start());
            if self.column + separator + word_width > self.width && !self.at_line_start() {
                self.newline();
                if !prefix.is_empty() {
                    self.emit_raw(prefix, scopes, source.clone(), node);
                }
            } else if separator == 1 {
                self.emit_raw(" ", scopes, source.clone(), node);
            }
            self.emit_raw(word, scopes, source.clone(), node);
            first = false;
        }
        start..self.char_len()
    }

    fn node(&mut self, kind: &str) -> String {
        let node = format!("{kind}:{}", self.node_index);
        self.node_index += 1;
        node
    }

    fn unique_anchor(&mut self, title: &str) -> String {
        let base = slug(title);
        let count = self.heading_ids.entry(base.clone()).or_default();
        let result = if *count == 0 {
            base
        } else {
            format!("{base}-{count}")
        };
        *count += 1;
        result
    }

    fn finish(mut self, source_text: String) -> MarkdownRender {
        while self.text.ends_with("\n\n") {
            self.text.pop();
        }
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.styles
            .retain(|span| span.range.end <= self.text.chars().count());
        self.mappings
            .sort_by_key(|mapping| (mapping.output.start, mapping.output.end));
        MarkdownRender {
            text: self.text,
            source_text,
            width: self.width,
            styles: self.styles,
            mappings: self.mappings,
            anchors: self.anchors,
            headings: self.headings,
            links: self.links,
            code_blocks: self.code_blocks,
            media: self.media,
        }
    }
}

fn render(
    cx: &mut Context,
    source: String,
    source_path: Option<String>,
    width: usize,
) -> SteelMarkdownRender {
    let mut options = Options::ENABLE_GFM;
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_MATH);

    let mut builder = Builder::new(width);
    let mut tags: Vec<TagEnd> = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut heading: Option<HeadingState> = None;
    let mut link: Option<LinkState> = None;
    let mut image: Option<ImageState> = None;
    let mut code: Option<NativeCodeState> = None;
    let mut table: Option<TableState> = None;
    let mut quote_depth = 0usize;
    let mut footnote_definitions = Vec::new();
    let mut footnote_references: HashMap<String, usize> = HashMap::new();

    for (event, byte_range) in Parser::new_ext(&source, options).into_offset_iter() {
        let source_range = byte_range_to_chars(&source, byte_range);

        if let Some(table) = table.as_mut() {
            if !matches!(event, Event::End(TagEnd::Table)) {
                capture_table_event(table, &event, source_range.clone());
                continue;
            }
        }
        if let Some(code) = code.as_mut() {
            if !matches!(event, Event::End(TagEnd::CodeBlock)) {
                if let Event::Text(text) | Event::Code(text) = &event {
                    code.code.push_str(text);
                    code.source.end = source_range.end;
                }
                continue;
            }
        }
        if let Some(image) = image.as_mut() {
            if !matches!(event, Event::End(TagEnd::Image)) {
                if let Event::Text(text) | Event::Code(text) = &event {
                    image.alt.push_str(text);
                }
                continue;
            }
        }

        match event {
            Event::Start(tag) => {
                let end = tag.to_end();
                match tag {
                    Tag::Paragraph => {
                        if quote_depth > 0 && builder.at_line_start() {
                            let prefix = "│ ".repeat(quote_depth);
                            let node = builder.node("quote");
                            builder.emit_raw(
                                &prefix,
                                &["markup.quote"],
                                source_range.clone(),
                                &node,
                            );
                        }
                    }
                    Tag::Heading { level, .. } => {
                        builder.blank_line();
                        heading = Some(HeadingState {
                            level: heading_level(level),
                            title: String::new(),
                            output: builder.char_len(),
                            source: source_range.clone(),
                        });
                    }
                    Tag::BlockQuote(kind) => {
                        builder.blank_line();
                        quote_depth += 1;
                        if let Some(kind) = kind {
                            let (label, scope) = callout(kind);
                            let node = builder.node("callout");
                            builder.emit_raw("┌─ ", &[scope], source_range.clone(), &node);
                            builder.emit_raw(
                                label,
                                &[scope, "markup.bold"],
                                source_range.clone(),
                                &node,
                            );
                            builder.newline();
                        }
                    }
                    Tag::CodeBlock(kind) => {
                        builder.blank_line();
                        let language = match kind {
                            CodeBlockKind::Fenced(value) => value.trim().to_string(),
                            CodeBlockKind::Indented => String::new(),
                        };
                        code = Some(NativeCodeState {
                            language,
                            code: String::new(),
                            source: source_range.clone(),
                        });
                    }
                    Tag::List(start) => lists.push(start),
                    Tag::Item => {
                        if !builder.at_line_start() {
                            builder.newline();
                        }
                        let depth = lists.len().saturating_sub(1);
                        let indent = "  ".repeat(depth);
                        let (bullet, scope) = match lists.last_mut() {
                            Some(Some(number)) => {
                                let bullet = format!("{}. ", *number);
                                *number += 1;
                                (bullet, "markup.list.numbered")
                            }
                            _ => ("• ".to_string(), "markup.list.unnumbered"),
                        };
                        let node = builder.node("item");
                        builder.emit_raw(
                            &(indent + bullet.as_str()),
                            &[scope],
                            source_range.clone(),
                            &node,
                        );
                    }
                    Tag::FootnoteDefinition(label) => {
                        builder.blank_line();
                        let node = builder.node("footnote");
                        builder
                            .anchors
                            .insert(format!("fn-{label}"), builder.char_len());
                        footnote_definitions.push(label.to_string());
                        builder.emit_raw(
                            &format!("[^{label}] "),
                            &["markup.link.label"],
                            source_range.clone(),
                            &node,
                        );
                    }
                    Tag::Table(alignments) => {
                        table = Some(TableState {
                            alignments,
                            rows: Vec::new(),
                            row: Vec::new(),
                            cell: None,
                            header_rows: 0,
                            in_head: false,
                            source: source_range.clone(),
                        });
                    }
                    Tag::Link { dest_url, .. } => {
                        let (destination, resolved) =
                            resolve_destination(&dest_url, source_path.as_deref());
                        link = Some(LinkState {
                            destination,
                            resolved,
                            output: builder.char_len(),
                            source: source_range.clone(),
                            label: String::new(),
                        });
                    }
                    Tag::Image { dest_url, .. } => {
                        let raw = dest_url.to_string();
                        let (destination, resolved) =
                            resolve_destination(&raw, source_path.as_deref());
                        image = Some(ImageState {
                            destination,
                            resolved,
                            remote: raw.starts_with("http://") || raw.starts_with("https://"),
                            source: source_range.clone(),
                            alt: String::new(),
                        });
                    }
                    _ => {}
                }
                tags.push(end);
            }
            Event::End(end) => {
                match end {
                    TagEnd::Paragraph => builder.blank_line(),
                    TagEnd::Heading(_) => {
                        if let Some(mut state) = heading.take() {
                            state.source.end = source_range.end;
                            let anchor = builder.unique_anchor(&state.title);
                            builder.headings.push(Heading {
                                level: state.level,
                                title: state.title,
                                anchor,
                                output: state.output,
                                source: state.source,
                            });
                        }
                        builder.blank_line();
                    }
                    TagEnd::BlockQuote(_) => {
                        quote_depth = quote_depth.saturating_sub(1);
                        builder.blank_line();
                    }
                    TagEnd::CodeBlock => {
                        if let Some(mut state) = code.take() {
                            state.source.end = source_range.end;
                            render_code(cx, &mut builder, state);
                        }
                        builder.blank_line();
                    }
                    TagEnd::List(_) => {
                        lists.pop();
                        if lists.is_empty() {
                            builder.blank_line();
                        }
                    }
                    TagEnd::Item => builder.newline(),
                    TagEnd::FootnoteDefinition => {
                        let label = footnote_definitions.pop();
                        let start = builder.char_len();
                        let node = builder.node("footnote-backlink");
                        let output = builder.emit_raw(
                            " ↩",
                            &["markup.link.url"],
                            source_range.clone(),
                            &node,
                        );
                        if let Some(label) = label {
                            builder.links.push(Link {
                                label: "back to reference".to_string(),
                                destination: format!("#fnref-{label}"),
                                resolved: true,
                                output: start..output.end,
                                source: source_range.clone(),
                            });
                        }
                        builder.blank_line();
                    }
                    TagEnd::Table => {
                        if let Some(mut state) = table.take() {
                            state.source.end = source_range.end;
                            render_table(&mut builder, state);
                        }
                        builder.blank_line();
                    }
                    TagEnd::Link => {
                        if let Some(mut state) = link.take() {
                            state.source.end = source_range.end;
                            let end = builder.char_len();
                            builder.links.push(Link {
                                label: state.label,
                                destination: state.destination,
                                resolved: state.resolved,
                                output: state.output..end,
                                source: state.source,
                            });
                        }
                    }
                    TagEnd::Image => {
                        if let Some(mut state) = image.take() {
                            state.source.end = source_range.end;
                            let node = builder.node("image");
                            let start = builder.char_len();
                            let alt = if state.alt.trim().is_empty() {
                                "image"
                            } else {
                                state.alt.trim()
                            };
                            builder.emit_raw(
                                "🖼 ",
                                &["markup.link.url"],
                                state.source.clone(),
                                &node,
                            );
                            builder.emit_wrapped(
                                alt,
                                &["markup.link.text"],
                                state.source.clone(),
                                &node,
                                "",
                            );
                            builder.emit_raw(
                                " — ",
                                &["ui.text.inactive"],
                                state.source.clone(),
                                &node,
                            );
                            builder.emit_wrapped(
                                &state.destination,
                                &["markup.link.url"],
                                state.source.clone(),
                                &node,
                                "  ",
                            );
                            builder.emit_raw(
                                " [external]",
                                &["ui.text.inactive"],
                                state.source.clone(),
                                &node,
                            );
                            let output = start..builder.char_len();
                            builder.media.push(Media {
                                alt: alt.to_string(),
                                destination: state.destination.clone(),
                                resolved: state.resolved,
                                remote: state.remote,
                                output: output.clone(),
                                source: state.source.clone(),
                            });
                            builder.links.push(Link {
                                label: alt.to_string(),
                                destination: state.destination,
                                resolved: state.resolved,
                                output,
                                source: state.source,
                            });
                        }
                    }
                    _ => {}
                }
                if let Some(position) = tags.iter().rposition(|tag| *tag == end) {
                    tags.remove(position);
                }
            }
            Event::Text(text) => {
                if let Some(state) = heading.as_mut() {
                    state.title.push_str(&text);
                }
                if let Some(state) = link.as_mut() {
                    state.label.push_str(&text);
                }
                let scopes = active_scopes(&tags, heading.as_ref().map(|state| state.level));
                let refs = scopes.iter().map(String::as_str).collect::<Vec<_>>();
                let node = builder.node("text");
                builder.emit_wrapped(&text, &refs, source_range, &node, &"│ ".repeat(quote_depth));
            }
            Event::Code(text) => {
                if let Some(state) = heading.as_mut() {
                    state.title.push_str(&text);
                }
                if let Some(state) = link.as_mut() {
                    state.label.push_str(&text);
                }
                let node = builder.node("inline-code");
                builder.emit_raw(&text, &["markup.raw.inline"], source_range, &node);
            }
            Event::InlineMath(math) => {
                let rendered = terminal_math(&math);
                let node = builder.node("inline-math");
                builder.emit_raw(&rendered, &["markup.raw.inline"], source_range, &node);
            }
            Event::DisplayMath(math) => {
                builder.blank_line();
                let rendered = terminal_math(&math);
                let padding = builder
                    .width
                    .saturating_sub(UnicodeWidthStr::width(rendered.as_str()))
                    / 2;
                let node = builder.node("display-math");
                builder.emit_raw(
                    &" ".repeat(padding),
                    &["markup.raw.block"],
                    source_range.clone(),
                    &node,
                );
                builder.emit_raw(&rendered, &["markup.raw.block"], source_range, &node);
                builder.blank_line();
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let node = builder.node("html");
                builder.emit_wrapped(
                    &escape_html(&html),
                    &["ui.text.inactive"],
                    source_range,
                    &node,
                    "",
                );
            }
            Event::FootnoteReference(label) => {
                let node = builder.node("footnote-reference");
                let count = footnote_references.entry(label.to_string()).or_default();
                let anchor = if *count == 0 {
                    format!("fnref-{label}")
                } else {
                    format!("fnref-{label}-{}", *count + 1)
                };
                *count += 1;
                builder.anchors.insert(anchor, builder.char_len());
                let output = builder.emit_raw(
                    &format!("[^{label}]"),
                    &["markup.link.label"],
                    source_range.clone(),
                    &node,
                );
                builder.links.push(Link {
                    label: label.to_string(),
                    destination: format!("#fn-{label}"),
                    resolved: true,
                    output,
                    source: source_range,
                });
            }
            Event::SoftBreak => {
                let node = builder.node("soft-break");
                builder.emit_raw(" ", &[], source_range, &node);
            }
            Event::HardBreak => builder.newline(),
            Event::Rule => {
                builder.blank_line();
                let node = builder.node("rule");
                builder.emit_raw(
                    &"─".repeat(builder.width.min(72)),
                    &["punctuation.special"],
                    source_range,
                    &node,
                );
                builder.blank_line();
            }
            Event::TaskListMarker(checked) => {
                let node = builder.node("task");
                builder.emit_raw(
                    if checked { "[x] " } else { "[ ] " },
                    &[if checked {
                        "markup.list.checked"
                    } else {
                        "markup.list.unchecked"
                    }],
                    source_range,
                    &node,
                );
            }
        }
    }

    SteelMarkdownRender(Arc::new(builder.finish(source)))
}

fn capture_table_event(table: &mut TableState, event: &Event<'_>, source: Range<usize>) {
    table.source.end = source.end;
    match event {
        Event::Start(Tag::TableHead) => table.in_head = true,
        Event::End(TagEnd::TableHead) => {
            table.in_head = false;
            table.header_rows = table.rows.len();
        }
        Event::Start(Tag::TableRow) => table.row = Vec::new(),
        Event::End(TagEnd::TableRow) => table.rows.push(std::mem::take(&mut table.row)),
        Event::Start(Tag::TableCell) => table.cell = Some(TableCell::default()),
        Event::End(TagEnd::TableCell) => {
            if let Some(cell) = table.cell.take() {
                table.row.push(cell);
            }
        }
        Event::Text(text) | Event::Code(text) | Event::InlineMath(text) => {
            if let Some(cell) = table.cell.as_mut() {
                if !cell.text.is_empty() {
                    cell.text.push(' ');
                }
                cell.text.push_str(text);
            }
        }
        Event::SoftBreak | Event::HardBreak => {
            if let Some(cell) = table.cell.as_mut() {
                cell.text.push(' ');
            }
        }
        Event::TaskListMarker(checked) => {
            if let Some(cell) = table.cell.as_mut() {
                cell.text.push_str(if *checked { "[x] " } else { "[ ] " });
            }
        }
        _ => {}
    }
}

fn render_table(builder: &mut Builder, table: TableState) {
    if table.rows.is_empty() {
        return;
    }
    builder.blank_line();
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return;
    }
    let mut widths = vec![8usize; columns];
    for row in &table.rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(UnicodeWidthStr::width(cell.text.as_str()).min(48));
        }
    }
    while widths.iter().sum::<usize>() + columns * 3 + 1 > builder.width {
        let Some((column, width)) = widths.iter().enumerate().max_by_key(|(_, width)| *width)
        else {
            break;
        };
        if *width <= 8 {
            break;
        }
        widths[column] -= 1;
    }
    let node = builder.node("table");
    table_border(builder, '┌', '┬', '┐', &widths, table.source.clone(), &node);
    for (row_index, row) in table.rows.iter().enumerate() {
        let wrapped = (0..columns)
            .map(|column| {
                wrap_cell(
                    row.get(column).map(|cell| cell.text.as_str()).unwrap_or(""),
                    widths[column],
                )
            })
            .collect::<Vec<_>>();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line in 0..height {
            builder.emit_raw("│", &["ui.text.inactive"], table.source.clone(), &node);
            for column in 0..columns {
                let value = wrapped[column].get(line).map(String::as_str).unwrap_or("");
                let aligned = align_cell(
                    value,
                    widths[column],
                    table
                        .alignments
                        .get(column)
                        .copied()
                        .unwrap_or(Alignment::None),
                );
                let scope = if row_index < table.header_rows.max(1) {
                    "markup.heading"
                } else {
                    "ui.text"
                };
                builder.emit_raw(" ", &[scope], table.source.clone(), &node);
                builder.emit_raw(&aligned, &[scope], table.source.clone(), &node);
                builder.emit_raw(" │", &["ui.text.inactive"], table.source.clone(), &node);
            }
            builder.newline();
        }
        if row_index + 1 < table.rows.len() {
            table_border(builder, '├', '┼', '┤', &widths, table.source.clone(), &node);
        }
    }
    table_border(builder, '└', '┴', '┘', &widths, table.source, &node);
}

fn table_border(
    builder: &mut Builder,
    left: char,
    middle: char,
    right: char,
    widths: &[usize],
    source: Range<usize>,
    node: &str,
) {
    let mut border = String::new();
    border.push(left);
    for (index, width) in widths.iter().enumerate() {
        border.push_str(&"─".repeat(*width + 2));
        border.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    builder.emit_raw(&border, &["ui.text.inactive"], source, node);
    builder.newline();
}

fn wrap_cell(value: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in sanitize_controls(value).split_whitespace() {
        let current = lines.last_mut().unwrap();
        let extra = usize::from(!current.is_empty());
        if UnicodeWidthStr::width(current.as_str()) + extra + UnicodeWidthStr::width(word) <= width
        {
            if extra == 1 {
                current.push(' ');
            }
            current.push_str(word);
        } else if UnicodeWidthStr::width(word) <= width {
            lines.push(word.to_string());
        } else {
            let mut part = String::new();
            for ch in word.chars() {
                if UnicodeWidthStr::width(part.as_str())
                    + UnicodeWidthStr::width(ch.to_string().as_str())
                    > width
                {
                    lines.push(std::mem::take(&mut part));
                }
                part.push(ch);
            }
            if !part.is_empty() {
                lines.push(part);
            }
        }
    }
    if lines.len() > 1 && lines[0].is_empty() {
        lines.remove(0);
    }
    lines
}

fn align_cell(value: &str, width: usize, alignment: Alignment) -> String {
    let used = UnicodeWidthStr::width(value);
    let free = width.saturating_sub(used);
    let (left, right) = match alignment {
        Alignment::Right => (free, 0),
        Alignment::Center => (free / 2, free - free / 2),
        Alignment::None | Alignment::Left => (0, free),
    };
    format!("{}{}{}", " ".repeat(left), value, " ".repeat(right))
}

fn render_code(cx: &mut Context, builder: &mut Builder, state: NativeCodeState) {
    let node = builder.node("code");
    if !state.language.is_empty() {
        builder.emit_raw(
            &format!(" {} ", state.language),
            &["ui.text.inactive", "markup.bold"],
            state.source.clone(),
            &node,
        );
        builder.newline();
    }
    let output = builder.emit_raw(
        &state.code,
        &["markup.raw.block"],
        state.source.clone(),
        &node,
    );
    if !state.language.is_empty() {
        for span in syntax_highlight::spans(cx, &state.code, &state.language) {
            let SteelVal::ListV(fields) = span else {
                continue;
            };
            let (Some(start), Some(end), Some(style)) = (
                steel_integer(fields.get(0)),
                steel_integer(fields.get(1)),
                fields
                    .get(2)
                    .and_then(|value| Style::as_ref(value).ok())
                    .map(|style| *style),
            ) else {
                continue;
            };
            if start < end && output.start + end <= output.end {
                builder.styles.push(StyledRange {
                    range: output.start + start..output.start + end,
                    style: RenderStyle::Concrete(style),
                });
            }
        }
    }
    builder.code_blocks.push(CodeBlock {
        language: state.language,
        code: state.code,
        output,
        source: state.source,
    });
}

fn active_scopes(tags: &[TagEnd], heading: Option<usize>) -> Vec<String> {
    let mut scopes = vec!["ui.text".to_string()];
    if let Some(level) = heading {
        scopes.push(format!("markup.heading.{level}"));
    }
    for tag in tags {
        match tag {
            TagEnd::Emphasis => scopes.push("markup.italic".to_string()),
            TagEnd::Strong => scopes.push("markup.bold".to_string()),
            TagEnd::Strikethrough => scopes.push("markup.strikethrough".to_string()),
            TagEnd::Link => scopes.push("markup.link.text".to_string()),
            _ => {}
        }
    }
    scopes
}

fn callout(kind: BlockQuoteKind) -> (&'static str, &'static str) {
    match kind {
        BlockQuoteKind::Note => ("NOTE", "diagnostic.info"),
        BlockQuoteKind::Tip => ("TIP", "diff.plus"),
        BlockQuoteKind::Important => ("IMPORTANT", "diagnostic.info"),
        BlockQuoteKind::Warning => ("WARNING", "diagnostic.warning"),
        BlockQuoteKind::Caution => ("CAUTION", "diagnostic.error"),
    }
}

fn heading_level(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn byte_range_to_chars(source: &str, range: Range<usize>) -> Range<usize> {
    source[..range.start.min(source.len())].chars().count()
        ..source[..range.end.min(source.len())].chars().count()
}

fn sanitize_controls(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\n' | '\t' => ch,
            '\u{1b}' => '␛',
            ch if ch.is_control() => '�',
            ch => ch,
        })
        .collect()
}

fn escape_html(value: &str) -> String {
    sanitize_controls(value)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    for ch in value.to_lowercase().chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            result.push(ch);
        } else if ch.is_whitespace() {
            result.push('-');
        }
    }
    if result.is_empty() {
        "section".to_string()
    } else {
        result
    }
}

fn terminal_math(value: &str) -> String {
    let replacements = [
        ("\\alpha", "α"),
        ("\\beta", "β"),
        ("\\gamma", "γ"),
        ("\\delta", "δ"),
        ("\\epsilon", "ε"),
        ("\\theta", "θ"),
        ("\\lambda", "λ"),
        ("\\mu", "μ"),
        ("\\pi", "π"),
        ("\\sigma", "σ"),
        ("\\phi", "φ"),
        ("\\omega", "ω"),
        ("\\Gamma", "Γ"),
        ("\\Delta", "Δ"),
        ("\\Theta", "Θ"),
        ("\\Lambda", "Λ"),
        ("\\Pi", "Π"),
        ("\\Sigma", "Σ"),
        ("\\Phi", "Φ"),
        ("\\Omega", "Ω"),
        ("\\times", "×"),
        ("\\cdot", "·"),
        ("\\pm", "±"),
        ("\\leq", "≤"),
        ("\\geq", "≥"),
        ("\\neq", "≠"),
        ("\\approx", "≈"),
        ("\\infty", "∞"),
        ("\\sum", "∑"),
        ("\\prod", "∏"),
        ("\\int", "∫"),
        ("\\sqrt", "√"),
        ("\\to", "→"),
        ("\\rightarrow", "→"),
        ("\\leftarrow", "←"),
        ("\\Rightarrow", "⇒"),
        ("\\Leftarrow", "⇐"),
    ];
    let mut result = sanitize_controls(value);
    for (from, to) in replacements {
        result = result.replace(from, to);
    }
    simple_scripts(&result)
}

fn simple_scripts(value: &str) -> String {
    let sub = "₀₁₂₃₄₅₆₇₈₉";
    let sup = "⁰¹²³⁴⁵⁶⁷⁸⁹";
    let mut result = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if (ch == '_' || ch == '^') && chars.peek().is_some_and(|next| next.is_ascii_digit()) {
            let digit = chars.next().unwrap().to_digit(10).unwrap() as usize;
            result.push(if ch == '_' {
                sub.chars().nth(digit).unwrap()
            } else {
                sup.chars().nth(digit).unwrap()
            });
        } else {
            result.push(ch);
        }
    }
    result
}

fn resolve_destination(destination: &str, source_path: Option<&str>) -> (String, bool) {
    if destination.starts_with('#')
        || has_scheme(destination)
        || Path::new(destination).is_absolute()
    {
        return (destination.to_string(), true);
    }
    let Some(source_path) = source_path else {
        return (destination.to_string(), false);
    };
    let (path, fragment) = destination.split_once('#').unwrap_or((destination, ""));
    let parent = Path::new(source_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let mut resolved = parent.join(path).to_string_lossy().to_string();
    if !fragment.is_empty() {
        resolved.push('#');
        resolved.push_str(fragment);
    }
    (resolved, true)
}

fn has_scheme(value: &str) -> bool {
    value.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
    })
}

fn steel_integer(value: Option<&SteelVal>) -> Option<usize> {
    match value? {
        SteelVal::IntV(value) if *value >= 0 => Some(*value as usize),
        _ => None,
    }
}

fn markdown_render_text(render: &SteelMarkdownRender) -> String {
    render.0.text.clone()
}

fn markdown_render_width(render: &SteelMarkdownRender) -> usize {
    render.0.width
}

fn markdown_render_source_unchanged(render: &SteelMarkdownRender, source: String) -> bool {
    render.0.source_text == source
}

fn markdown_render_styles(render: &SteelMarkdownRender) -> SteelVal {
    list(render.0.styles.iter().map(|span| {
        let style = match &span.style {
            RenderStyle::Scope(scope) => scope.clone().into_steelval().unwrap(),
            RenderStyle::Concrete(style) => style.into_steelval().unwrap(),
        };
        list_value(vec![
            integer(span.range.start),
            integer(span.range.end),
            style,
        ])
    }))
}

fn markdown_render_headings(render: &SteelMarkdownRender) -> SteelVal {
    list(render.0.headings.iter().map(|heading| {
        list_value(vec![
            integer(heading.level),
            heading.title.clone().into_steelval().unwrap(),
            heading.anchor.clone().into_steelval().unwrap(),
            integer(heading.output),
            integer(heading.source.start),
            integer(heading.source.end),
        ])
    }))
}

fn markdown_render_links(render: &SteelMarkdownRender) -> SteelVal {
    list(render.0.links.iter().map(|link| {
        list_value(vec![
            link.label.clone().into_steelval().unwrap(),
            link.destination.clone().into_steelval().unwrap(),
            link.resolved.into_steelval().unwrap(),
            integer(link.output.start),
            integer(link.output.end),
            integer(link.source.start),
            integer(link.source.end),
        ])
    }))
}

fn markdown_render_code_blocks(render: &SteelMarkdownRender) -> SteelVal {
    list(render.0.code_blocks.iter().map(|block| {
        list_value(vec![
            block.language.clone().into_steelval().unwrap(),
            block.code.clone().into_steelval().unwrap(),
            integer(block.output.start),
            integer(block.output.end),
            integer(block.source.start),
            integer(block.source.end),
        ])
    }))
}

fn markdown_render_media(render: &SteelMarkdownRender) -> SteelVal {
    list(render.0.media.iter().map(|media| {
        list_value(vec![
            media.alt.clone().into_steelval().unwrap(),
            media.destination.clone().into_steelval().unwrap(),
            media.resolved.into_steelval().unwrap(),
            media.remote.into_steelval().unwrap(),
            integer(media.output.start),
            integer(media.output.end),
            integer(media.source.start),
            integer(media.source.end),
        ])
    }))
}

fn markdown_render_mappings(render: &SteelMarkdownRender) -> SteelVal {
    list(render.0.mappings.iter().map(|mapping| {
        list_value(vec![
            integer(mapping.output.start),
            integer(mapping.output.end),
            integer(mapping.source.start),
            integer(mapping.source.end),
            mapping.node.clone().into_steelval().unwrap(),
        ])
    }))
}

fn markdown_render_anchor_output(render: &SteelMarkdownRender, anchor: String) -> Option<usize> {
    render.0.anchors.get(&anchor).copied().or_else(|| {
        render
            .0
            .headings
            .iter()
            .find(|heading| heading.anchor == anchor)
            .map(|heading| heading.output)
    })
}

fn source_for_output(render: &SteelMarkdownRender, output: usize) -> Option<usize> {
    nearest_mapping(&render.0.mappings, output, true)
}

fn output_for_source(render: &SteelMarkdownRender, source: usize) -> Option<usize> {
    nearest_mapping(&render.0.mappings, source, false)
}

fn nearest_mapping(
    mappings: &[SourceMap],
    position: usize,
    output_to_source: bool,
) -> Option<usize> {
    let mapping = mappings.iter().min_by_key(|mapping| {
        let range = if output_to_source {
            &mapping.output
        } else {
            &mapping.source
        };
        if range.contains(&position) {
            0
        } else {
            range
                .start
                .abs_diff(position)
                .min(range.end.abs_diff(position))
        }
    })?;
    let (from, to) = if output_to_source {
        (&mapping.output, &mapping.source)
    } else {
        (&mapping.source, &mapping.output)
    };
    let offset = position
        .saturating_sub(from.start)
        .min(from.end.saturating_sub(from.start));
    Some(to.start + offset.min(to.end.saturating_sub(to.start)))
}

fn apply_focused(cx: &mut Context, render: SteelMarkdownRender) -> bool {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return false;
    };
    if doc.text().to_string() != render.0.text {
        return false;
    }
    let highlights = render
        .0
        .styles
        .iter()
        .map(|span| CustomHighlight {
            range: span.range.clone(),
            style: match &span.style {
                RenderStyle::Scope(scope) => CustomHighlightStyle::Scope(scope.clone()),
                RenderStyle::Concrete(style) => CustomHighlightStyle::Concrete(*style),
            },
        })
        .collect();
    doc.set_custom_text_annotations(
        view_id,
        DECORATION_NAMESPACE.to_string(),
        CustomTextAnnotations {
            highlights,
            ..Default::default()
        },
    );
    // Generated scratch content has no on-disk save target. Treat installing a
    // validated render as its clean baseline so ordinary close/quit commands
    // never prompt to save the preview.
    doc.reset_modified();
    true
}

fn clear_focused(cx: &mut Context) -> bool {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return false;
    };
    doc.clear_custom_text_annotations(view_id, DECORATION_NAMESPACE);
    true
}

fn open_target(cx: &mut Context, target: String) -> anyhow::Result<bool> {
    if target.starts_with('#') {
        return Ok(false);
    }
    let scheme = target
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    let url = match scheme.as_deref() {
        Some("http" | "https" | "mailto" | "file") => helix_stdx::Url::from_str(&target)?,
        Some(_) => anyhow::bail!("unsupported link scheme; target was not opened"),
        None => {
            let path = target
                .split_once('#')
                .map(|(path, _)| path)
                .unwrap_or(&target);
            helix_stdx::Url::from_file_path(path)
                .map_err(|_| anyhow::anyhow!("local preview target must be an absolute path"))?
        }
    };
    crate::commands::open_url(cx, url, Action::Load);
    Ok(true)
}

fn integer(value: usize) -> SteelVal {
    (value as isize).into_steelval().unwrap()
}

fn list_value(values: Vec<SteelVal>) -> SteelVal {
    SteelVal::ListV(values.into())
}

fn list(values: impl IntoIterator<Item = SteelVal>) -> SteelVal {
    list_value(values.into_iter().collect())
}

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn_with_ctx(CTX, "markdown-render", render)
        .register_fn("markdown-render-text", markdown_render_text)
        .register_fn("markdown-render-width", markdown_render_width)
        .register_fn(
            "markdown-render-source-unchanged?",
            markdown_render_source_unchanged,
        )
        .register_fn("markdown-render-styles", markdown_render_styles)
        .register_fn("markdown-render-headings", markdown_render_headings)
        .register_fn("markdown-render-links", markdown_render_links)
        .register_fn("markdown-render-code-blocks", markdown_render_code_blocks)
        .register_fn("markdown-render-media", markdown_render_media)
        .register_fn("markdown-render-mappings", markdown_render_mappings)
        .register_fn(
            "markdown-render-anchor-output",
            markdown_render_anchor_output,
        )
        .register_fn("markdown-render-source-for-output", source_for_output)
        .register_fn("markdown-render-output-for-source", output_for_source)
        .register_fn_with_ctx(CTX, "markdown-render-apply-focused!", apply_focused)
        .register_fn_with_ctx(CTX, "markdown-render-clear-focused!", clear_focused)
        .register_fn_with_ctx(CTX, "markdown-preview-open-target!", open_target);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_slugs_are_deduplicated() {
        let mut builder = Builder::new(80);
        assert_eq!(builder.unique_anchor("Hello, World!"), "hello-world");
        assert_eq!(builder.unique_anchor("Hello, World!"), "hello-world-1");
    }

    #[test]
    fn terminal_math_keeps_unknown_tex_visible() {
        assert_eq!(
            terminal_math("\\alpha + x_2 + \\unknown"),
            "α + x₂ + \\unknown"
        );
    }

    #[test]
    fn controls_are_never_emitted_verbatim() {
        assert_eq!(sanitize_controls("ok\u{1b}[31m\u{7}"), "ok␛[31m�");
    }

    #[test]
    fn relative_targets_need_a_source_path() {
        assert_eq!(
            resolve_destination("img/a.png", None),
            ("img/a.png".into(), false)
        );
        assert_eq!(
            resolve_destination("img/a.png", Some("/tmp/docs/readme.md")),
            ("/tmp/docs/img/a.png".into(), true)
        );
    }

    #[test]
    fn table_cells_wrap_at_terminal_width() {
        assert_eq!(wrap_cell("one two three", 7), vec!["one two", "three"]);
    }

    #[test]
    fn explicit_anchors_precede_heading_fallbacks() {
        let mut builder = Builder::new(80);
        builder.anchors.insert("fn-one".into(), 12);
        let render = SteelMarkdownRender(Arc::new(builder.finish(String::new())));
        assert_eq!(
            markdown_render_anchor_output(&render, "fn-one".into()),
            Some(12)
        );
        assert_eq!(
            markdown_render_anchor_output(&render, "missing".into()),
            None
        );
    }
}

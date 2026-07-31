//! Structured Markdown document preview for Steel plugins.
//!
//! Parsing, layout, source mapping and concrete syntax styles stay native.
//! Steel owns the scratch-buffer lifecycle and interaction policy.

use std::{
    collections::HashMap,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::{
    annotations::custom_text::{CustomHighlight, CustomHighlightStyle, CustomTextAnnotations},
    editor::Action,
    graphics::{Color, Style},
};
use image::{
    codecs::png::PngEncoder, imageops, ExtendedColorType, ImageEncoder, ImageReader, Limits, Rgba,
    RgbaImage,
};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use steel::{
    rvals::{AsRefSteelVal, Custom, IntoSteelVal},
    steel_vm::{builtin::BuiltInModule, register_fn::RegisterFn},
    SteelErr, SteelVal,
};

use super::{syntax_highlight, Context, CTX};

const MIN_WIDTH: usize = 20;
const MAX_WIDTH: usize = 500;
const DECORATION_NAMESPACE: &str = "markdown-preview";
const MAX_MEDIA_BYTES: u64 = 12 * 1024 * 1024;
const MAX_MEDIA_DIMENSION: u32 = 8192;
const MAX_MEDIA_PIXELS: u64 = 24_000_000;
const MAX_MEDIA_CELLS: usize = 200_000;
const MAX_MEDIA_HEIGHT_CELLS: usize = 60;
const MAX_REMOTE_MEDIA: usize = 8;
const MAX_TEX_BYTES: usize = 16 * 1024;
const MAX_TEX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MATH_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_KITTY_COLUMNS: u32 = 64;
const MAX_KITTY_ROWS: u32 = 64;
const KITTY_PLACEHOLDER: char = '\u{10eeee}';
const KITTY_DIACRITICS: [char; 64] = [
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
];

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
struct Formula {
    tex: String,
    display: bool,
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
    formulas: Vec<Formula>,
    kitty_ids: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaMode {
    External,
    Unicode,
    Kitty,
}

impl MediaMode {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "external" => Ok(Self::External),
            "unicode" => Ok(Self::Unicode),
            "kitty" => Ok(Self::Kitty),
            _ => anyhow::bail!("local media mode must be external, unicode, or kitty"),
        }
    }
}

#[derive(Clone, Debug)]
struct MediaEdit {
    range: Range<usize>,
    text: String,
    styles: Vec<StyledRange>,
    kitty_id: Option<u32>,
}

#[derive(Default)]
struct MathCache {
    entries: HashMap<String, Arc<Vec<u8>>>,
    bytes: usize,
}

static MATH_CACHE: OnceLock<Mutex<MathCache>> = OnceLock::new();
static NEXT_KITTY_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Clone)]
struct SteelMarkdownRender(Arc<MarkdownRender>);

impl Custom for SteelMarkdownRender {}

struct MarkdownCallbackValue(SteelMarkdownRender);

impl TryInto<SteelVal> for MarkdownCallbackValue {
    type Error = SteelErr;

    fn try_into(self) -> Result<SteelVal, Self::Error> {
        self.0.into_steelval()
    }
}

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
    formulas: Vec<Formula>,
    kitty_ids: Vec<u32>,
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
            formulas: Vec::new(),
            kitty_ids: Vec::new(),
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
            formulas: self.formulas,
            kitty_ids: self.kitty_ids,
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
                                if state.remote {
                                    " [remote blocked]"
                                } else if state.resolved {
                                    " [external; inspecting]"
                                } else {
                                    " [unresolved]"
                                },
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
                let output = builder.emit_raw(
                    &rendered,
                    &["markup.raw.inline"],
                    source_range.clone(),
                    &node,
                );
                builder.formulas.push(Formula {
                    tex: math.to_string(),
                    display: false,
                    output,
                    source: source_range,
                });
            }
            Event::DisplayMath(math) => {
                builder.blank_line();
                let rendered = terminal_math(&math);
                let start = builder.char_len();
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
                builder.emit_raw(
                    &rendered,
                    &["markup.raw.block"],
                    source_range.clone(),
                    &node,
                );
                builder.formulas.push(Formula {
                    tex: math.to_string(),
                    display: true,
                    output: start..builder.char_len(),
                    source: source_range.clone(),
                });
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

fn render_local_media(
    render: SteelMarkdownRender,
    mode: MediaMode,
    background: [u8; 3],
    true_color: bool,
    allow_remote: bool,
    raster_math: bool,
) -> SteelMarkdownRender {
    let mut edits = Vec::new();
    let kitty_available = tui::backend::kitty_graphics_available();
    let media_mode = if mode == MediaMode::Kitty && !kitty_available {
        MediaMode::External
    } else {
        mode
    };
    let math_mode = if mode == MediaMode::Kitty && !kitty_available {
        MediaMode::Unicode
    } else {
        mode
    };
    if raster_math {
        for formula in &render.0.formulas {
            if let Ok(edit) =
                raster_math_edit(formula, render.0.width, math_mode, background, true_color)
            {
                edits.push(edit);
            }
        }
    }
    let mut remote_count = 0;
    for media in &render.0.media {
        if !media.resolved {
            continue;
        }
        let result = if media.remote {
            if !allow_remote {
                continue;
            }
            if remote_count >= MAX_REMOTE_MEDIA {
                Ok(media_status_edit(media, "remote item limit exceeded", None))
            } else {
                remote_count += 1;
                fetch_remote_media_edit(media, render.0.width, media_mode, background, true_color)
            }
        } else if !has_scheme(&media.destination) {
            decode_media_edit(media, render.0.width, media_mode, background, true_color)
        } else {
            continue;
        };
        edits.push(result.unwrap_or_else(|error| {
            media_status_edit(media, &format!("media failed: {error}"), None)
        }));
    }
    SteelMarkdownRender(Arc::new(apply_media_edits(render.0.as_ref(), edits)))
}

fn raster_math_edit(
    formula: &Formula,
    render_width: usize,
    mode: MediaMode,
    background: [u8; 3],
    true_color: bool,
) -> anyhow::Result<MediaEdit> {
    anyhow::ensure!(
        formula.tex.len() <= MAX_TEX_BYTES,
        "formula exceeds TeX byte limit"
    );
    let key = format!("{}:{}", formula.display, formula.tex);
    let png = cached_math_png(&key).unwrap_or_else(|| Arc::new(Vec::new()));
    let png = if png.is_empty() {
        let generated = Arc::new(generate_math_png(formula)?);
        cache_math_png(key, generated.clone());
        generated
    } else {
        png
    };

    let mut reader = ImageReader::new(std::io::Cursor::new(png.as_ref())).with_guessed_format()?;
    reader.limits(media_limits());
    let image = reader.decode()?.to_rgba8();
    let max_height = if formula.display { 40 } else { 2 };
    let image = imageops::thumbnail(
        &image,
        render_width.clamp(MIN_WIDTH, MAX_WIDTH) as u32,
        max_height,
    );
    let cells = image.width() as usize * image.height().div_ceil(2) as usize;
    anyhow::ensure!(cells <= MAX_MEDIA_CELLS, "raster math exceeds cell limit");

    if mode == MediaMode::Kitty {
        return kitty_image_edit(
            formula.output.clone(),
            &image,
            String::new(),
            formula.display,
        );
    }

    let mut text = String::new();
    let mut styles = Vec::with_capacity(cells);
    let padding = if formula.display {
        render_width.saturating_sub(image.width() as usize) / 2
    } else {
        0
    };
    for y in (0..image.height()).step_by(2) {
        text.push_str(&" ".repeat(padding));
        for x in 0..image.width() {
            let top = composite(image.get_pixel(x, y), background);
            let bottom = if y + 1 < image.height() {
                composite(image.get_pixel(x, y + 1), background)
            } else {
                background
            };
            let start = text.chars().count();
            text.push('▀');
            styles.push(StyledRange {
                range: start..start + 1,
                style: RenderStyle::Concrete(
                    Style::default()
                        .fg(terminal_color(top, true_color))
                        .bg(terminal_color(bottom, true_color)),
                ),
            });
        }
        if formula.display {
            text.push('\n');
        }
    }
    Ok(MediaEdit {
        range: formula.output.clone(),
        text,
        styles,
        kitty_id: None,
    })
}

fn cached_math_png(key: &str) -> Option<Arc<Vec<u8>>> {
    MATH_CACHE
        .get_or_init(|| Mutex::new(MathCache::default()))
        .lock()
        .ok()?
        .entries
        .get(key)
        .cloned()
}

fn cache_math_png(key: String, png: Arc<Vec<u8>>) {
    let Ok(mut cache) = MATH_CACHE
        .get_or_init(|| Mutex::new(MathCache::default()))
        .lock()
    else {
        return;
    };
    if png.len() > MAX_MATH_CACHE_BYTES {
        return;
    }
    if cache.bytes + png.len() > MAX_MATH_CACHE_BYTES {
        cache.entries.clear();
        cache.bytes = 0;
    }
    cache.bytes += png.len();
    cache.entries.insert(key, png);
}

fn generate_math_png(formula: &Formula) -> anyhow::Result<Vec<u8>> {
    let directory = tempfile::Builder::new()
        .prefix("helix-markdown-math-")
        .tempdir()?;
    let expression = if formula.display {
        format!("\\[{}\\]", formula.tex)
    } else {
        format!("${}$", formula.tex)
    };
    let document = format!(
        "\\documentclass{{article}}\n\\usepackage[active,tightpage]{{preview}}\n\\usepackage{{amsmath,amssymb}}\n\\pagestyle{{empty}}\n\\begin{{document}}\n\\begin{{preview}}\n{expression}\n\\end{{preview}}\n\\end{{document}}\n"
    );
    anyhow::ensure!(
        document.len() <= MAX_TEX_BYTES + 1024,
        "TeX document exceeds limit"
    );
    let tex = directory.path().join("formula.tex");
    fs::write(&tex, document)?;

    let latex = bounded_process(
        "latex",
        directory.path(),
        [
            "-no-shell-escape",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "formula.tex",
        ],
    )
    .status()?;
    anyhow::ensure!(latex.success(), "latex failed or timed out");
    anyhow::ensure!(
        directory_size(directory.path())? <= MAX_TEX_ARTIFACT_BYTES,
        "TeX artifacts exceed output limit"
    );

    let dvipng = bounded_process(
        "dvipng",
        directory.path(),
        [
            "-T",
            "tight",
            "-D",
            "130",
            "-bg",
            "Transparent",
            "-o",
            "formula.png",
            "formula.dvi",
        ],
    )
    .status()?;
    anyhow::ensure!(dvipng.success(), "dvipng failed or timed out");
    let png = fs::read(directory.path().join("formula.png"))?;
    anyhow::ensure!(
        png.len() as u64 <= MAX_MEDIA_BYTES,
        "rasterized formula exceeds byte limit"
    );
    Ok(png)
}

fn bounded_process<const N: usize>(program: &str, cwd: &Path, args: [&str; N]) -> Command {
    let mut command = Command::new("timeout");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd)
        .arg("--signal=KILL")
        .arg("5")
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn directory_size(path: &Path) -> anyhow::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let metadata = entry?.metadata()?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn decode_media_edit(
    media: &Media,
    render_width: usize,
    mode: MediaMode,
    background: [u8; 3],
    true_color: bool,
) -> anyhow::Result<MediaEdit> {
    let path = PathBuf::from(
        media
            .destination
            .split_once('#')
            .map(|(path, _)| path)
            .unwrap_or(&media.destination),
    );
    decode_media_edit_from_path(media, &path, render_width, mode, background, true_color)
}

fn decode_media_edit_from_path(
    media: &Media,
    path: &Path,
    render_width: usize,
    mode: MediaMode,
    background: [u8; 3],
    true_color: bool,
) -> anyhow::Result<MediaEdit> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    {
        return Ok(media_status_edit(media, "SVG; external", None));
    }
    let size = fs::metadata(&path)?.len();
    anyhow::ensure!(size <= MAX_MEDIA_BYTES, "compressed file exceeds 12 MiB");

    let mut dimensions_reader = ImageReader::open(&path)?.with_guessed_format()?;
    dimensions_reader.limits(media_limits());
    let (width, height) = dimensions_reader.into_dimensions()?;
    anyhow::ensure!(
        u64::from(width) * u64::from(height) <= MAX_MEDIA_PIXELS,
        "decoded image exceeds pixel limit"
    );
    if mode == MediaMode::External {
        return Ok(media_status_edit(media, "external", Some((width, height))));
    }

    let mut reader = ImageReader::open(&path)?.with_guessed_format()?;
    reader.limits(media_limits());
    let image = reader.decode()?.to_rgba8();
    let max_width = if mode == MediaMode::Kitty {
        (render_width.clamp(MIN_WIDTH, MAX_WIDTH) as u32).min(MAX_KITTY_COLUMNS)
    } else {
        render_width.clamp(MIN_WIDTH, MAX_WIDTH) as u32
    };
    let max_pixel_height = if mode == MediaMode::Kitty {
        MAX_KITTY_ROWS * 2
    } else {
        (MAX_MEDIA_HEIGHT_CELLS * 2) as u32
    };
    let image = imageops::thumbnail(&image, max_width, max_pixel_height);
    let cells = image.width() as usize * image.height().div_ceil(2) as usize;
    anyhow::ensure!(
        cells <= MAX_MEDIA_CELLS,
        "rendered image exceeds cell limit"
    );

    if mode == MediaMode::Kitty {
        let header = format!(
            "🖼 {} — {} [{}×{}; kitty]\n",
            media.alt, media.destination, width, height
        );
        return kitty_image_edit(media.output.clone(), &image, header, true).or_else(|_| {
            Ok(media_status_edit(
                media,
                "Kitty upload unavailable; external",
                Some((width, height)),
            ))
        });
    }

    let mut text = format!(
        "🖼 {} — {} [{}×{}; unicode]\n",
        media.alt, media.destination, width, height
    );
    let mut styles = Vec::with_capacity(cells + 1);
    styles.push(StyledRange {
        range: 0..text.chars().count(),
        style: RenderStyle::Scope("ui.text.inactive".to_string()),
    });
    for y in (0..image.height()).step_by(2) {
        for x in 0..image.width() {
            let top = composite(image.get_pixel(x, y), background);
            let bottom = if y + 1 < image.height() {
                composite(image.get_pixel(x, y + 1), background)
            } else {
                background
            };
            let start = text.chars().count();
            text.push('▀');
            styles.push(StyledRange {
                range: start..start + 1,
                style: RenderStyle::Concrete(
                    Style::default()
                        .fg(terminal_color(top, true_color))
                        .bg(terminal_color(bottom, true_color)),
                ),
            });
        }
        text.push('\n');
    }
    Ok(MediaEdit {
        range: media.output.clone(),
        text,
        styles,
        kitty_id: None,
    })
}

fn kitty_image_edit(
    range: Range<usize>,
    image: &RgbaImage,
    header: String,
    newline_after_last: bool,
) -> anyhow::Result<MediaEdit> {
    let columns = image.width().min(MAX_KITTY_COLUMNS);
    let rows = image.height().div_ceil(2).min(MAX_KITTY_ROWS);
    anyhow::ensure!(columns > 0 && rows > 0, "Kitty image has no cells");

    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    )?;
    anyhow::ensure!(
        png.len() as u64 <= MAX_MEDIA_BYTES,
        "Kitty PNG exceeds byte limit"
    );

    let id = next_kitty_id();
    anyhow::ensure!(
        tui::backend::queue_kitty_upload(id, png, columns as u16, rows as u16),
        "Kitty graphics queue unavailable"
    );

    let mut text = header;
    let mut styles = Vec::with_capacity((columns * rows) as usize + 1);
    if !text.is_empty() {
        styles.push(StyledRange {
            range: 0..text.chars().count(),
            style: RenderStyle::Scope("ui.text.inactive".to_string()),
        });
    }
    let color = Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8);
    for row in 0..rows as usize {
        for column in 0..columns as usize {
            let start = text.chars().count();
            text.push(KITTY_PLACEHOLDER);
            text.push(KITTY_DIACRITICS[row]);
            text.push(KITTY_DIACRITICS[column]);
            styles.push(StyledRange {
                range: start..start + 3,
                style: RenderStyle::Concrete(Style::default().fg(color)),
            });
        }
        if row + 1 < rows as usize || newline_after_last {
            text.push('\n');
        }
    }
    Ok(MediaEdit {
        range,
        text,
        styles,
        kitty_id: Some(id),
    })
}

fn next_kitty_id() -> u32 {
    loop {
        let id = NEXT_KITTY_ID.fetch_add(1, Ordering::Relaxed) & 0x00ff_ffff;
        if id != 0 {
            return id;
        }
    }
}

fn fetch_remote_media_edit(
    media: &Media,
    render_width: usize,
    mode: MediaMode,
    background: [u8; 3],
    true_color: bool,
) -> anyhow::Result<MediaEdit> {
    anyhow::ensure!(
        media.destination.starts_with("http://") || media.destination.starts_with("https://"),
        "remote media scheme is not permitted"
    );
    let temporary = tempfile::Builder::new()
        .prefix("helix-markdown-preview-")
        .tempfile()?;
    let output = bounded_curl_command(&media.destination, temporary.path()).output()?;
    anyhow::ensure!(
        output.status.success(),
        "curl exited with {}: {}",
        output.status,
        sanitize_controls(String::from_utf8_lossy(&output.stderr).trim())
    );
    anyhow::ensure!(
        fs::metadata(temporary.path())?.len() <= MAX_MEDIA_BYTES,
        "download exceeds byte limit"
    );
    decode_media_edit_from_path(
        media,
        temporary.path(),
        render_width,
        mode,
        background,
        true_color,
    )
}

fn bounded_curl_command(url: &str, output: &Path) -> Command {
    let mut command = Command::new("curl");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .arg("--disable")
        .arg("--fail")
        .arg("--silent")
        .arg("--show-error")
        .arg("--location")
        .arg("--max-redirs")
        .arg("3")
        .arg("--connect-timeout")
        .arg("3")
        .arg("--max-time")
        .arg("8")
        .arg("--proto")
        .arg("=http,https")
        .arg("--proto-redir")
        .arg("=http,https")
        .arg("--max-filesize")
        .arg(MAX_MEDIA_BYTES.to_string())
        .arg("--user-agent")
        .arg("helix-markdown-preview/1")
        .arg("--output")
        .arg(output)
        .arg(url);
    command
}

fn media_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_MEDIA_DIMENSION);
    limits.max_image_height = Some(MAX_MEDIA_DIMENSION);
    limits.max_alloc = Some(MAX_MEDIA_PIXELS.saturating_mul(4));
    limits
}

fn media_status_edit(media: &Media, status: &str, dimensions: Option<(u32, u32)>) -> MediaEdit {
    let dimensions = dimensions
        .map(|(width, height)| format!("{width}×{height}; "))
        .unwrap_or_default();
    let text = format!(
        "🖼 {} — {} [{}{}]",
        media.alt, media.destination, dimensions, status
    );
    let length = text.chars().count();
    MediaEdit {
        range: media.output.clone(),
        text,
        styles: vec![StyledRange {
            range: 0..length,
            style: RenderStyle::Scope("ui.text.inactive".to_string()),
        }],
        kitty_id: None,
    }
}

fn composite(pixel: &Rgba<u8>, background: [u8; 3]) -> [u8; 3] {
    let alpha = u16::from(pixel[3]);
    let inverse = 255 - alpha;
    [
        ((u16::from(pixel[0]) * alpha + u16::from(background[0]) * inverse) / 255) as u8,
        ((u16::from(pixel[1]) * alpha + u16::from(background[1]) * inverse) / 255) as u8,
        ((u16::from(pixel[2]) * alpha + u16::from(background[2]) * inverse) / 255) as u8,
    ]
}

fn terminal_color(rgb: [u8; 3], true_color: bool) -> Color {
    if true_color {
        Color::Rgb(rgb[0], rgb[1], rgb[2])
    } else {
        let red = ((u16::from(rgb[0]) * 5 + 127) / 255) as u8;
        let green = ((u16::from(rgb[1]) * 5 + 127) / 255) as u8;
        let blue = ((u16::from(rgb[2]) * 5 + 127) / 255) as u8;
        Color::Indexed(16 + 36 * red + 6 * green + blue)
    }
}

fn theme_background(color: Option<Color>) -> [u8; 3] {
    match color {
        Some(Color::Rgb(red, green, blue)) => [red, green, blue],
        Some(Color::Black | Color::Reset) | None => [0, 0, 0],
        Some(Color::White | Color::LightGray) => [220, 220, 220],
        Some(Color::Gray) => [128, 128, 128],
        Some(Color::Red | Color::LightRed) => [205, 49, 49],
        Some(Color::Green | Color::LightGreen) => [13, 188, 121],
        Some(Color::Yellow | Color::LightYellow) => [229, 229, 16],
        Some(Color::Blue | Color::LightBlue) => [36, 114, 200],
        Some(Color::Magenta | Color::LightMagenta) => [188, 63, 188],
        Some(Color::Cyan | Color::LightCyan) => [17, 168, 205],
        Some(Color::Indexed(index)) => ansi256_rgb(index),
    }
}

fn ansi256_rgb(index: u8) -> [u8; 3] {
    if index < 16 {
        return match index {
            0 => [0, 0, 0],
            1 => [128, 0, 0],
            2 => [0, 128, 0],
            3 => [128, 128, 0],
            4 => [0, 0, 128],
            5 => [128, 0, 128],
            6 => [0, 128, 128],
            7 => [192, 192, 192],
            8 => [128, 128, 128],
            9 => [255, 0, 0],
            10 => [0, 255, 0],
            11 => [255, 255, 0],
            12 => [0, 0, 255],
            13 => [255, 0, 255],
            14 => [0, 255, 255],
            _ => [255, 255, 255],
        };
    }
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return [level, level, level];
    }
    let index = index - 16;
    let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
    [level(index / 36), level((index % 36) / 6), level(index % 6)]
}

fn apply_media_edits(render: &MarkdownRender, mut edits: Vec<MediaEdit>) -> MarkdownRender {
    edits.sort_by_key(|edit| edit.range.start);
    edits.retain(|edit| {
        edit.range.start <= edit.range.end && edit.range.end <= render.text.chars().count()
    });
    let mut text = String::new();
    let mut cursor = 0;
    let mut inserted_styles = Vec::new();
    let mut kitty_ids = Vec::new();
    for edit in &edits {
        if edit.range.start < cursor {
            continue;
        }
        text.push_str(char_slice(&render.text, cursor..edit.range.start));
        let offset = text.chars().count();
        text.push_str(&edit.text);
        inserted_styles.extend(edit.styles.iter().cloned().map(|mut style| {
            style.range = offset + style.range.start..offset + style.range.end;
            style
        }));
        if let Some(id) = edit.kitty_id {
            kitty_ids.push(id);
        }
        cursor = edit.range.end;
    }
    text.push_str(char_slice(
        &render.text,
        cursor..render.text.chars().count(),
    ));

    let map_range = |range: &Range<usize>| {
        map_position(range.start, &edits, false)..map_position(range.end, &edits, true)
    };
    let mut styles = render
        .styles
        .iter()
        .filter(|style| {
            !edits
                .iter()
                .any(|edit| intersects(&style.range, &edit.range))
        })
        .cloned()
        .map(|mut style| {
            style.range = map_range(&style.range);
            style
        })
        .collect::<Vec<_>>();
    styles.extend(inserted_styles);

    let mut updated = render.clone();
    updated.text = text;
    updated.styles = styles;
    updated.kitty_ids = kitty_ids;
    updated.mappings = render
        .mappings
        .iter()
        .cloned()
        .map(|mut mapping| {
            mapping.output = map_range(&mapping.output);
            mapping
        })
        .collect();
    updated.anchors = render
        .anchors
        .iter()
        .map(|(anchor, position)| (anchor.clone(), map_position(*position, &edits, false)))
        .collect();
    updated.headings = render
        .headings
        .iter()
        .cloned()
        .map(|mut heading| {
            heading.output = map_position(heading.output, &edits, false);
            heading
        })
        .collect();
    updated.links = render
        .links
        .iter()
        .cloned()
        .map(|mut link| {
            link.output = map_range(&link.output);
            link
        })
        .collect();
    updated.code_blocks = render
        .code_blocks
        .iter()
        .cloned()
        .map(|mut block| {
            block.output = map_range(&block.output);
            block
        })
        .collect();
    updated.media = render
        .media
        .iter()
        .cloned()
        .map(|mut media| {
            media.output = map_range(&media.output);
            media
        })
        .collect();
    updated.formulas = render
        .formulas
        .iter()
        .cloned()
        .map(|mut formula| {
            formula.output = map_range(&formula.output);
            formula
        })
        .collect();
    updated
}

fn char_slice(value: &str, range: Range<usize>) -> &str {
    let start = value
        .char_indices()
        .nth(range.start)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    let end = value
        .char_indices()
        .nth(range.end)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    &value[start..end]
}

fn intersects(first: &Range<usize>, second: &Range<usize>) -> bool {
    first.start < second.end && second.start < first.end
}

fn map_position(position: usize, edits: &[MediaEdit], _end: bool) -> usize {
    let mut delta = 0isize;
    for edit in edits {
        let replacement = edit.text.chars().count();
        if position < edit.range.start {
            break;
        }
        if position >= edit.range.end {
            delta += replacement as isize - edit.range.len() as isize;
            continue;
        }
        let offset = position.saturating_sub(edit.range.start).min(replacement);
        return (edit.range.start as isize + delta) as usize + offset;
    }
    (position as isize + delta) as usize
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

fn markdown_render_release_media(render: &SteelMarkdownRender) -> usize {
    for id in &render.0.kitty_ids {
        tui::backend::queue_kitty_delete(*id);
    }
    render.0.kitty_ids.len()
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

fn markdown_render_formulas(render: &SteelMarkdownRender) -> SteelVal {
    list(render.0.formulas.iter().map(|formula| {
        list_value(vec![
            formula.tex.clone().into_steelval().unwrap(),
            formula.display.into_steelval().unwrap(),
            integer(formula.output.start),
            integer(formula.output.end),
            integer(formula.source.start),
            integer(formula.source.end),
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

fn markdown_render_local_media_async(
    cx: &mut Context,
    render: SteelMarkdownRender,
    mode: String,
    allow_remote: bool,
    raster_math: bool,
    callback: SteelVal,
) -> anyhow::Result<()> {
    let mode = MediaMode::parse(&mode)?;
    let background = theme_background(cx.editor.theme.get("ui.background").bg);
    let true_color = cx.editor.config.load().true_color || crate::true_color();
    let rooted = callback.as_rooted();
    let future = async move {
        let render = tokio::task::spawn_blocking(move || {
            render_local_media(
                render,
                mode,
                background,
                true_color,
                allow_remote,
                raster_math,
            )
        })
        .await
        .map_err(|error| helix_lsp::Error::Other(anyhow::Error::from(error)))?;
        Ok::<_, helix_lsp::Error>(MarkdownCallbackValue(render))
    };
    super::super::create_callback(cx, future, rooted)
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
        .register_fn_with_ctx(
            CTX,
            "markdown-render-local-media!",
            markdown_render_local_media_async,
        )
        .register_fn("markdown-render-source-for-output", source_for_output)
        .register_fn("markdown-render-output-for-source", output_for_source)
        .register_fn_with_ctx(CTX, "markdown-render-apply-focused!", apply_focused)
        .register_fn_with_ctx(CTX, "markdown-render-clear-focused!", clear_focused)
        .register_fn_with_ctx(CTX, "markdown-preview-open-target!", open_target)
        .register_fn("markdown-render-formulas", markdown_render_formulas)
        .register_fn(
            "markdown-render-release-media!",
            markdown_render_release_media,
        );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

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

    #[test]
    fn unicode_media_is_bounded_colored_and_remapped() {
        let temporary = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        let pixels = [255, 0, 0, 255, 0, 0, 255, 128];
        PngEncoder::new(File::create(temporary.path()).unwrap())
            .write_image(&pixels, 1, 2, ExtendedColorType::Rgba8)
            .unwrap();

        let placeholder = "image placeholder";
        let mut builder = Builder::new(40);
        let output = builder.emit_raw(placeholder, &["markup.link.url"], 0..4, "image:0");
        builder.media.push(Media {
            alt: "sample".into(),
            destination: temporary.path().to_string_lossy().into_owned(),
            resolved: true,
            remote: false,
            output: output.clone(),
            source: 0..4,
        });
        builder.links.push(Link {
            label: "sample".into(),
            destination: temporary.path().to_string_lossy().into_owned(),
            resolved: true,
            output,
            source: 0..4,
        });
        let render = SteelMarkdownRender(Arc::new(builder.finish("![x](sample.png)".into())));
        let rendered =
            render_local_media(render, MediaMode::Unicode, [0, 0, 0], true, false, false);

        assert!(rendered.0.text.contains('▀'));
        assert!(rendered.0.styles.iter().any(|style| matches!(
            style.style,
            RenderStyle::Concrete(Style {
                fg: Some(Color::Rgb(255, 0, 0)),
                ..
            })
        )));
        let text_length = rendered.0.text.chars().count();
        assert!(rendered
            .0
            .mappings
            .windows(2)
            .all(|pair| pair[0].output.start <= pair[1].output.start));
        assert!(rendered
            .0
            .links
            .iter()
            .all(|link| link.output.end <= text_length));
    }

    #[test]
    fn indexed_color_quantization_is_used_without_truecolor() {
        assert!(matches!(
            terminal_color([255, 0, 0], false),
            Color::Indexed(_)
        ));
        assert_eq!(composite(&Rgba([255, 255, 255, 0]), [3, 4, 5]), [3, 4, 5]);
    }

    #[test]
    fn remote_fetch_command_is_bounded_and_credential_free() {
        let command = bounded_curl_command("https://example.test/image.png", Path::new("/tmp/out"));
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for required in [
            "--disable",
            "--max-redirs",
            "--connect-timeout",
            "--max-time",
            "--proto",
            "--proto-redir",
            "--max-filesize",
        ] {
            assert!(arguments.iter().any(|argument| argument == required));
        }
        assert!(!arguments
            .iter()
            .any(|argument| { matches!(argument.as_str(), "--cookie" | "--user" | "--netrc") }));
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "PATH" && value.is_some()));
    }

    #[test]
    fn latex_process_has_no_shell_escape_and_a_hard_timeout() {
        let command = bounded_process(
            "latex",
            Path::new("/tmp"),
            ["-no-shell-escape", "formula.tex"],
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(arguments[0..3], ["--signal=KILL", "5", "latex"]);
        assert!(arguments
            .iter()
            .any(|argument| argument == "-no-shell-escape"));
        assert!(!arguments.iter().any(|argument| argument == "sh"));
    }

    #[test]
    fn trusted_math_rasterizes_when_tools_are_installed() {
        if !Path::new("/usr/bin/latex").exists() || !Path::new("/usr/bin/dvipng").exists() {
            return;
        }
        let formula = Formula {
            tex: r"\alpha + x_2".into(),
            display: false,
            output: 0..6,
            source: 0..12,
        };
        let edit = raster_math_edit(&formula, 80, MediaMode::Unicode, [0, 0, 0], true).unwrap();
        assert!(edit.text.contains('▀'));
        assert!(edit
            .styles
            .iter()
            .any(|style| matches!(style.style, RenderStyle::Concrete(_))));

        let invalid = Formula {
            tex: r"\definitelyMissingCommand".into(),
            ..formula
        };
        assert!(generate_math_png(&invalid).is_err());
    }
}

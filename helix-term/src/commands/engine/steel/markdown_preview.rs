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
    graphics::{Color, Modifier, Style},
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
// Unicode-placeholder cells are addressed by the diacritic table, so neither
// dimension can exceed its length.
const MAX_KITTY_COLUMNS: u32 = KITTY_DIACRITICS.len() as u32;
const MAX_KITTY_ROWS: u32 = KITTY_DIACRITICS.len() as u32;
/// Terminal cell height divided by cell width.  The terminal never reports its
/// cell size to us, so Kitty placements assume the near-universal 1:2 cell;
/// being wrong here skews proportions but cannot overflow the placement.
const CELL_ASPECT: f32 = 2.0;
const ASSUMED_CELL_WIDTH: u32 = 8;
const ASSUMED_CELL_HEIGHT: u32 = 16;
const KITTY_PLACEHOLDER: char = '\u{10eeee}';
/// Indentation alone reads poorly once lists nest; vary the marker by depth.
const BULLETS: [char; 5] = ['•', '◦', '▪', '‣', '⁃'];
/// Below roughly six cells of height a rasterized formula stops resolving its
/// glyphs, and readable text is the better answer.
const MIN_MATH_PIXEL_ROWS: u32 = 12;
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
    output_mapping_index: MappingIndex,
    source_mapping_index: MappingIndex,
    anchors: HashMap<String, usize>,
    headings: Vec<Heading>,
    links: Vec<Link>,
    code_blocks: Vec<CodeBlock>,
    media: Vec<Media>,
    formulas: Vec<Formula>,
    kitty_ids: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
struct MappingIndex {
    by_start: Vec<usize>,
    by_end: Vec<usize>,
    prefix_max_end: Vec<usize>,
}

impl MappingIndex {
    fn new(mappings: &[SourceMap], output: bool) -> Self {
        let range = |index: usize| mapping_range(&mappings[index], output);
        let mut by_start = (0..mappings.len()).collect::<Vec<_>>();
        by_start.sort_by_key(|index| (range(*index).start, range(*index).end, *index));
        let mut prefix_max_end = Vec::with_capacity(by_start.len());
        let mut maximum = 0;
        for index in &by_start {
            maximum = maximum.max(range(*index).end);
            prefix_max_end.push(maximum);
        }
        let mut by_end = (0..mappings.len()).collect::<Vec<_>>();
        by_end.sort_by_key(|index| (range(*index).end, *index));
        Self {
            by_start,
            by_end,
            prefix_max_end,
        }
    }
}

fn mapping_range(mapping: &SourceMap, output: bool) -> &Range<usize> {
    if output {
        &mapping.output
    } else {
        &mapping.source
    }
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
    text_chars: usize,
    width: usize,
    column: usize,
    /// Inline runs arrive as separate events, so the whitespace that separates
    /// them belongs to neither run.  Carry it between emissions instead of
    /// deriving it per call, or `a *b* c` collapses to `abc`.
    pending_space: bool,
    /// Weight applied to whatever is emitted next, on top of its theme scope.
    /// Emphasis that relies on the theme declaring `modifiers` renders as a
    /// bare colour under themes that only set one, so the renderer states it.
    emphasis: Modifier,
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
            text_chars: 0,
            width: width.clamp(MIN_WIDTH, MAX_WIDTH),
            column: 0,
            pending_space: false,
            emphasis: Modifier::empty(),
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
        self.text_chars
    }

    fn at_line_start(&self) -> bool {
        self.column == 0
    }

    fn newline(&mut self) {
        if !self.text.ends_with('\n') {
            self.text.push('\n');
            self.text_chars += 1;
        }
        self.column = 0;
        self.pending_space = false;
    }

    fn blank_line(&mut self) {
        self.newline();
        if !self.text.ends_with("\n\n") {
            self.text.push('\n');
            self.text_chars += 1;
        }
        self.column = 0;
        self.pending_space = false;
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
        self.text_chars += value.chars().count();
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
            // Carries no colour, so the theme still owns every colour, and
            // concrete styles are patched after scope overlays, so the modifier
            // cannot be dropped on the way to the terminal.
            if !self.emphasis.is_empty() {
                self.styles.push(StyledRange {
                    range: start..end,
                    style: RenderStyle::Concrete(Style::default().add_modifier(self.emphasis)),
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

    /// Emit inline content that participates in inter-run spacing.  Block
    /// furniture (bullets, borders, quote prefixes) must keep using `emit_raw`,
    /// which drops any pending space rather than materializing it.
    fn emit_inline(
        &mut self,
        value: &str,
        scopes: &[&str],
        source: Range<usize>,
        node: &str,
    ) -> Range<usize> {
        self.flush_pending_space(scopes, source.clone(), node);
        self.emit_raw(value, scopes, source, node)
    }

    fn flush_pending_space(&mut self, scopes: &[&str], source: Range<usize>, node: &str) {
        if self.pending_space && !self.at_line_start() {
            self.emit_raw(" ", scopes, source, node);
        }
        self.pending_space = false;
    }

    fn emit_wrapped(
        &mut self,
        value: &str,
        scopes: &[&str],
        source: Range<usize>,
        node: &str,
        prefix: &str,
    ) -> Range<usize> {
        let value = sanitize_controls(value);
        if value.starts_with(char::is_whitespace) {
            self.pending_space = true;
        }
        // Separators belong to the gap, not to the run, so the reported range
        // opens at the first real word.
        let mut start = self.char_len();
        let mut first = true;
        for word in value.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            let separator = usize::from((self.pending_space || !first) && !self.at_line_start());
            if self.column + separator + word_width > self.width && !self.at_line_start() {
                self.newline();
                if !prefix.is_empty() {
                    self.emit_raw(prefix, scopes, source.clone(), node);
                }
            } else if separator == 1 {
                self.emit_raw(" ", scopes, source.clone(), node);
            }
            self.pending_space = false;
            if first {
                start = self.char_len();
            }
            self.emit_raw(word, scopes, source.clone(), node);
            first = false;
        }
        if value.ends_with(char::is_whitespace) {
            self.pending_space = true;
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
            self.text_chars -= 1;
        }
        if !self.text.ends_with('\n') {
            self.text.push('\n');
            self.text_chars += 1;
        }
        self.styles
            .retain(|span| span.range.end <= self.text.chars().count());
        self.mappings
            .sort_by_key(|mapping| (mapping.output.start, mapping.output.end));
        self.links
            .sort_by_key(|link| (link.output.start, link.output.end));
        let output_mapping_index = MappingIndex::new(&self.mappings, true);
        let source_mapping_index = MappingIndex::new(&self.mappings, false);
        MarkdownRender {
            text: self.text,
            source_text,
            width: self.width,
            styles: self.styles,
            mappings: self.mappings,
            output_mapping_index,
            source_mapping_index,
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
    show_links: bool,
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
    // Block HTML arrives one line per event, so an unterminated comment has to
    // stay open across events.
    let mut html_comment = false;
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
                        let level = heading_level(level);
                        let node = builder.node("heading");
                        let scope = format!("markup.heading.{level}");
                        if level == 1 {
                            builder.emit_raw(
                                &heading_rule(builder.width, '━'),
                                &[scope.as_str()],
                                source_range.clone(),
                                &node,
                            );
                            builder.newline();
                            builder.emit_raw(" ", &[scope.as_str()], source_range.clone(), &node);
                        } else if let Some(prefix) = heading_prefix(level) {
                            builder.emit_raw(
                                prefix,
                                &[scope.as_str()],
                                source_range.clone(),
                                &node,
                            );
                        }
                        heading = Some(HeadingState {
                            level,
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
                            builder.emphasis = Modifier::BOLD;
                            builder.emit_raw(
                                label,
                                &[scope, "markup.bold"],
                                source_range.clone(),
                                &node,
                            );
                            builder.emphasis = Modifier::empty();
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
                            _ => (
                                format!("{} ", BULLETS[depth % BULLETS.len()]),
                                "markup.list.unnumbered",
                            ),
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
                            // `column` is the display width of the title's last
                            // line, which is what the underline should match.
                            let title_width = builder.column;
                            let node = builder.node("heading-rule");
                            let scope = format!("markup.heading.{}", state.level);
                            match state.level {
                                1 => {
                                    builder.newline();
                                    builder.emit_raw(
                                        &heading_rule(builder.width, '━'),
                                        &[scope.as_str()],
                                        state.source.clone(),
                                        &node,
                                    );
                                }
                                2 => {
                                    builder.newline();
                                    let width = title_width.clamp(3, builder.width);
                                    builder.emit_raw(
                                        &"─".repeat(width),
                                        &[scope.as_str()],
                                        state.source.clone(),
                                        &node,
                                    );
                                }
                                _ => {}
                            }
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
                            builder.flush_pending_space(
                                &["markup.link.url"],
                                state.source.clone(),
                                &node,
                            );
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
                            if show_links {
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
                            }
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
                builder.emphasis = active_modifier(&tags);
                builder.emit_wrapped(&text, &refs, source_range, &node, &"│ ".repeat(quote_depth));
                builder.emphasis = Modifier::empty();
            }
            Event::Code(text) => {
                if let Some(state) = heading.as_mut() {
                    state.title.push_str(&text);
                }
                if let Some(state) = link.as_mut() {
                    state.label.push_str(&text);
                }
                let node = builder.node("inline-code");
                builder.emphasis = active_modifier(&tags);
                builder.emit_inline(&text, &["markup.raw.inline"], source_range, &node);
                builder.emphasis = Modifier::empty();
            }
            Event::InlineMath(math) => {
                let rendered = terminal_math(&math);
                let node = builder.node("inline-math");
                let output = builder.emit_inline(
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
                let (text, breaks) = strip_html(&html, &mut html_comment);
                let node = builder.node("html");
                if !text.trim().is_empty() {
                    builder.emit_wrapped(&text, &["ui.text"], source_range, &node, "");
                }
                for _ in 0..breaks {
                    builder.newline();
                }
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
                let output = builder.emit_inline(
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
            // A soft break is a separator, not content: deferring it keeps the
            // wrap decision with the next word and avoids a zero-width mapping.
            Event::SoftBreak => builder.pending_space = true,
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
        // pulldown-cmark reports header cells directly inside `TableHead` with
        // no enclosing `TableRow`, so the header has to be committed here or
        // the first body row's `Start(TableRow)` discards it.
        Event::End(TagEnd::TableHead) => {
            table.in_head = false;
            if !table.row.is_empty() {
                table.rows.push(std::mem::take(&mut table.row));
            }
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
        // Adjacent inline runs carry their own spacing; inserting one here turns
        // `**a**b` into `a b`.
        Event::Text(text) | Event::Code(text) | Event::InlineMath(text) => {
            if let Some(cell) = table.cell.as_mut() {
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
    table_border(builder, TABLE_TOP, &widths, table.source.clone(), &node);
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
                let scope = if row_index < table.header_rows {
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
        // Only the header earns a divider.  A rule between every body row
        // buries the data it is supposed to separate.
        if table.header_rows > 0 && row_index + 1 == table.header_rows {
            table_border(builder, TABLE_HEADER, &widths, table.source.clone(), &node);
        }
    }
    table_border(builder, TABLE_BOTTOM, &widths, table.source, &node);
}

/// `[left, middle, right, fill]` corner and run glyphs for one table rule.
type BorderGlyphs = [char; 4];

const TABLE_TOP: BorderGlyphs = ['┌', '┬', '┐', '─'];
const TABLE_HEADER: BorderGlyphs = ['╞', '╪', '╡', '═'];
const TABLE_BOTTOM: BorderGlyphs = ['└', '┴', '┘', '─'];

fn table_border(
    builder: &mut Builder,
    glyphs: BorderGlyphs,
    widths: &[usize],
    source: Range<usize>,
    node: &str,
) {
    let [left, middle, right, fill] = glyphs;
    let mut border = String::new();
    border.push(left);
    for (index, width) in widths.iter().enumerate() {
        border.push_str(&fill.to_string().repeat(*width + 2));
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

/// One run of code characters that made it into the output contiguously.
/// Framing splits the code across lines, so highlight offsets have to be
/// translated through these rather than added to a single block start.
struct CodeSegment {
    code: Range<usize>,
    output: usize,
}

fn render_code(cx: &mut Context, builder: &mut Builder, state: NativeCodeState) {
    let node = builder.node("code");
    // Tabs would misalign the right border, and tree-sitter is indifferent to
    // the substitution, so highlight the expanded text too.
    let code = expand_tabs(&state.code);
    let lines = code.strip_suffix('\n').unwrap_or(&code);
    let lines = lines.split('\n').collect::<Vec<_>>();

    let label = if state.language.is_empty() {
        String::new()
    } else {
        format!("─ {} ", state.language)
    };
    let frame_budget = builder.width.saturating_sub(4).max(1);
    let inner = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(*line))
        .max()
        .unwrap_or(0)
        .max(UnicodeWidthStr::width(label.as_str()))
        .clamp(1, frame_budget);

    let start = builder.char_len();
    let border = ["ui.text.inactive"];
    // A language too long for the frame would push the top border past the
    // sides; drop the header rather than draw a ragged box.
    let show_label =
        !state.language.is_empty() && UnicodeWidthStr::width(label.as_str()) <= inner + 2;

    // Top border, with the language sitting in it as the box header.
    builder.emit_raw("╭", &border, state.source.clone(), &node);
    if !show_label {
        builder.emit_raw(&"─".repeat(inner + 2), &border, state.source.clone(), &node);
    } else {
        builder.emit_raw("─ ", &border, state.source.clone(), &node);
        builder.emphasis = Modifier::BOLD;
        builder.emit_raw(
            &state.language,
            &["ui.text.inactive", "markup.bold"],
            state.source.clone(),
            &node,
        );
        builder.emphasis = Modifier::empty();
        builder.emit_raw(" ", &border, state.source.clone(), &node);
        let used = UnicodeWidthStr::width(label.as_str());
        builder.emit_raw(
            &"─".repeat((inner + 2).saturating_sub(used)),
            &border,
            state.source.clone(),
            &node,
        );
    }
    builder.emit_raw("╮", &border, state.source.clone(), &node);
    builder.newline();

    let mut segments = Vec::new();
    let mut consumed = 0usize;
    for line in &lines {
        let line_start = consumed;
        consumed += line.chars().count() + 1; // include the newline we dropped
        for chunk in wrap_code_line(line, inner) {
            builder.emit_raw("│ ", &border, state.source.clone(), &node);
            let output = builder.char_len();
            builder.emit_raw(
                &chunk.text,
                &["markup.raw.block"],
                state.source.clone(),
                &node,
            );
            segments.push(CodeSegment {
                code: line_start + chunk.range.start..line_start + chunk.range.end,
                output,
            });
            let padding = inner.saturating_sub(UnicodeWidthStr::width(chunk.text.as_str()));
            builder.emit_raw(
                &format!("{} │", " ".repeat(padding)),
                &border,
                state.source.clone(),
                &node,
            );
            builder.newline();
        }
    }

    builder.emit_raw(
        &format!("╰{}╯", "─".repeat(inner + 2)),
        &border,
        state.source.clone(),
        &node,
    );
    builder.newline();
    let output = start..builder.char_len();

    if !state.language.is_empty() {
        for span in syntax_highlight::spans(cx, &code, &state.language) {
            let SteelVal::ListV(fields) = span else {
                continue;
            };
            let (Some(span_start), Some(span_end), Some(style)) = (
                steel_integer(fields.get(0)),
                steel_integer(fields.get(1)),
                fields
                    .get(2)
                    .and_then(|value| Style::as_ref(value).ok())
                    .map(|style| *style),
            ) else {
                continue;
            };
            if span_start >= span_end {
                continue;
            }
            for segment in &segments {
                let from = span_start.max(segment.code.start);
                let to = span_end.min(segment.code.end);
                if from >= to {
                    continue;
                }
                let offset = segment.output + (from - segment.code.start);
                builder.styles.push(StyledRange {
                    range: offset..offset + (to - from),
                    style: RenderStyle::Concrete(style),
                });
            }
        }
    }

    builder.code_blocks.push(CodeBlock {
        language: state.language,
        // The exact original text, so copying a block is byte-faithful.
        code: state.code,
        output,
        source: state.source,
    });
}

struct CodeChunk {
    text: String,
    range: Range<usize>,
}

/// Split one code line into pieces that fit the frame.  Code is not prose, so
/// break on width rather than on words.
fn wrap_code_line(line: &str, width: usize) -> Vec<CodeChunk> {
    let mut chunks = Vec::new();
    let mut text = String::new();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut used = 0usize;

    for ch in line.chars() {
        let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + ch_width > width && !text.is_empty() {
            chunks.push(CodeChunk {
                text: std::mem::take(&mut text),
                range: start..index,
            });
            start = index;
            used = 0;
        }
        text.push(ch);
        used += ch_width;
        index += 1;
    }
    chunks.push(CodeChunk {
        text,
        range: start..index,
    });
    chunks
}

fn expand_tabs(value: &str) -> String {
    const TAB: usize = 4;
    let mut result = String::with_capacity(value.len());
    let mut column = 0usize;
    for ch in value.chars() {
        match ch {
            '\t' => {
                let advance = TAB - column % TAB;
                result.push_str(&" ".repeat(advance));
                column += advance;
            }
            '\n' => {
                result.push('\n');
                column = 0;
            }
            ch => {
                result.push(ch);
                column += UnicodeWidthStr::width(ch.to_string().as_str());
            }
        }
    }
    result
}

/// The weight the open tags call for, independent of what the theme happens to
/// declare for the matching scopes.
fn active_modifier(tags: &[TagEnd]) -> Modifier {
    let mut modifier = Modifier::empty();
    for tag in tags {
        match tag {
            TagEnd::Emphasis => modifier |= Modifier::ITALIC,
            TagEnd::Strong => modifier |= Modifier::BOLD,
            TagEnd::Strikethrough => modifier |= Modifier::CROSSED_OUT,
            _ => {}
        }
    }
    modifier
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

fn heading_rule(width: usize, fill: char) -> String {
    fill.to_string().repeat(width.min(72))
}

/// Levels 1 and 2 are drawn with rules; the rest need a marker to stay
/// distinguishable when a theme gives them the same colour.
fn heading_prefix(level: usize) -> Option<&'static str> {
    match level {
        1 | 2 => None,
        3 => Some("▸ "),
        4 => Some("  ‣ "),
        5 => Some("    · "),
        _ => Some("      · "),
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

/// Reduce raw HTML to the text a reader cares about: comments disappear, `<br>`
/// becomes a line break, and every other tag is dropped while its content
/// survives.  Escaping instead of stripping is what put a literal
/// `&lt;!-- ... --&gt;` on screen.
///
/// `in_comment` carries an unterminated comment across events, since block HTML
/// is reported one line at a time.  Returns the visible text and the number of
/// explicit line breaks the markup asked for.
fn strip_html(value: &str, in_comment: &mut bool) -> (String, usize) {
    let value = sanitize_controls(value);
    let mut text = String::new();
    let mut breaks = 0usize;
    let mut rest = value.as_str();

    loop {
        if *in_comment {
            match rest.find("-->") {
                Some(index) => {
                    *in_comment = false;
                    rest = &rest[index + 3..];
                }
                None => break,
            }
            continue;
        }
        let Some(index) = rest.find('<') else {
            text.push_str(rest);
            break;
        };
        text.push_str(&rest[..index]);
        rest = &rest[index..];

        if rest.starts_with("<!--") {
            *in_comment = true;
            rest = &rest[4..];
            continue;
        }
        // A `<` with no closing `>` in this event is literal text, not a tag.
        let Some(end) = rest.find('>') else {
            text.push_str(rest);
            break;
        };
        let tag = rest[1..end].trim().trim_end_matches('/').trim();
        let name = tag
            .strip_prefix('/')
            .unwrap_or(tag)
            .split(|ch: char| ch.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if name == "br" {
            breaks += 1;
        }
        rest = &rest[end + 1..];
    }

    (text, breaks)
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

#[allow(clippy::too_many_arguments)]
fn render_local_media(
    render: SteelMarkdownRender,
    mode: MediaMode,
    background: [u8; 3],
    foreground: [u8; 3],
    true_color: bool,
    allow_remote: bool,
    raster_math: bool,
    show_links: bool,
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
            if let Ok(edit) = raster_math_edit(
                formula,
                render.0.width,
                math_mode,
                background,
                foreground,
                true_color,
            ) {
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
                Ok(media_status_edit(
                    media,
                    "remote item limit exceeded",
                    None,
                    show_links,
                ))
            } else {
                remote_count += 1;
                fetch_remote_media_edit(
                    media,
                    render.0.width,
                    media_mode,
                    background,
                    true_color,
                    show_links,
                )
            }
        } else if !has_scheme(&media.destination) {
            decode_media_edit(
                media,
                render.0.width,
                media_mode,
                background,
                true_color,
                show_links,
            )
        } else {
            continue;
        };
        edits.push(result.unwrap_or_else(|error| {
            media_status_edit(media, &format!("media failed: {error}"), None, show_links)
        }));
    }
    SteelMarkdownRender(Arc::new(apply_media_edits(render.0.as_ref(), edits)))
}

fn raster_math_edit(
    formula: &Formula,
    render_width: usize,
    mode: MediaMode,
    background: [u8; 3],
    foreground: [u8; 3],
    true_color: bool,
) -> anyhow::Result<MediaEdit> {
    anyhow::ensure!(
        formula.tex.len() <= MAX_TEX_BYTES,
        "formula exceeds TeX byte limit"
    );
    // An inline half-block run has exactly one cell of vertical room, so
    // anything taller would spill across the line it sits in.  One cell of
    // glyph is unreadable, so leave inline formulas as terminal text and let
    // Kitty — which scales inside the cell — handle them instead.
    anyhow::ensure!(
        formula.display || mode == MediaMode::Kitty,
        "inline math stays textual outside Kitty"
    );

    // The ink colour is baked into the PNG, so it belongs in the cache key.
    let key = format!("{}:{:?}:{}", formula.display, foreground, formula.tex);
    let png = cached_math_png(&key).unwrap_or_else(|| Arc::new(Vec::new()));
    let png = if png.is_empty() {
        let generated = Arc::new(generate_math_png(formula, foreground)?);
        cache_math_png(key, generated.clone());
        generated
    } else {
        png
    };

    let mut reader = ImageReader::new(std::io::Cursor::new(png.as_ref())).with_guessed_format()?;
    reader.limits(media_limits());
    let image = trim_transparent(&reader.decode()?.to_rgba8());
    let width_limit = render_width.clamp(MIN_WIDTH, MAX_WIDTH) as u32;

    if mode == MediaMode::Kitty {
        let max_rows = if formula.display { 20 } else { 1 };
        let (columns, rows) = kitty_cells(
            image.width(),
            image.height(),
            width_limit.min(MAX_KITTY_COLUMNS),
            max_rows,
        );
        anyhow::ensure!(columns > 0 && rows > 0, "raster math has no cells");
        let image = imageops::resize(
            &image,
            columns * ASSUMED_CELL_WIDTH,
            rows * ASSUMED_CELL_HEIGHT,
            imageops::FilterType::Lanczos3,
        );
        return kitty_image_edit(
            formula.output.clone(),
            &image,
            columns,
            rows,
            String::new(),
            formula.display,
        );
    }

    // Display math: half-block pixels are square on a 1:2 cell, so the pixel
    // grid keeps the source aspect directly.
    let (target_width, target_height) =
        fit_dimensions(image.width(), image.height(), width_limit, 40);
    anyhow::ensure!(target_width > 0, "raster math has no pixels");
    // A wide equation squeezed into a pane-width strip loses its glyphs
    // entirely.  Text beats a smear, so hand back to `terminal_math`.
    anyhow::ensure!(
        target_height >= MIN_MATH_PIXEL_ROWS,
        "formula is too wide to stay legible at this width"
    );
    let image = imageops::resize(
        &image,
        target_width,
        target_height,
        imageops::FilterType::Lanczos3,
    );
    let cells = image.width() as usize * image.height().div_ceil(2) as usize;
    anyhow::ensure!(cells <= MAX_MEDIA_CELLS, "raster math exceeds cell limit");

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

/// The TeX fed to `latex` for one formula.
///
/// Display math is wrapped in `$\displaystyle ... $` rather than `\[ ... \]`.
/// The latter typesets at the full text width, and `tightpage` crops the page
/// rather than the ink, so the formula would arrive as a sliver in a mostly
/// blank box — 959x92 for one whose ink is 74x89. `\displaystyle` keeps the
/// display typesetting, limits above and below included, at its natural width,
/// which is what leaves enough resolution to survive the downscale.
fn math_document(formula: &Formula) -> String {
    let expression = if formula.display {
        format!("$\\displaystyle {}$", formula.tex)
    } else {
        format!("${}$", formula.tex)
    };
    format!(
        "\\documentclass{{article}}\n\\usepackage[active,tightpage]{{preview}}\n\\usepackage{{amsmath,amssymb}}\n\\pagestyle{{empty}}\n\\begin{{document}}\n\\begin{{preview}}\n{expression}\n\\end{{preview}}\n\\end{{document}}\n"
    )
}

fn generate_math_png(formula: &Formula, foreground: [u8; 3]) -> anyhow::Result<Vec<u8>> {
    let directory = tempfile::Builder::new()
        .prefix("helix-markdown-math-")
        .tempdir()?;
    let document = math_document(formula);
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

    // Render well above the target size and downscale: the glyph strokes only
    // survive half-block quantization if they start out oversampled.  LaTeX ink
    // is black, which is invisible on a dark theme, so paint it in the theme's
    // own foreground instead.
    let ink = format!(
        "rgb {:.3} {:.3} {:.3}",
        f32::from(foreground[0]) / 255.0,
        f32::from(foreground[1]) / 255.0,
        f32::from(foreground[2]) / 255.0
    );
    let dvipng = bounded_process(
        "dvipng",
        directory.path(),
        [
            "-T",
            "tight",
            "-D",
            "200",
            "-bg",
            "Transparent",
            "-fg",
            ink.as_str(),
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
    show_links: bool,
) -> anyhow::Result<MediaEdit> {
    let path = PathBuf::from(
        media
            .destination
            .split_once('#')
            .map(|(path, _)| path)
            .unwrap_or(&media.destination),
    );
    decode_media_edit_from_path(
        media,
        &path,
        render_width,
        mode,
        background,
        true_color,
        show_links,
    )
}

fn decode_media_edit_from_path(
    media: &Media,
    path: &Path,
    render_width: usize,
    mode: MediaMode,
    background: [u8; 3],
    true_color: bool,
    show_links: bool,
) -> anyhow::Result<MediaEdit> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    {
        return Ok(media_status_edit(media, "SVG; external", None, show_links));
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
        return Ok(media_status_edit(
            media,
            "external",
            Some((width, height)),
            show_links,
        ));
    }

    let mut reader = ImageReader::open(&path)?.with_guessed_format()?;
    reader.limits(media_limits());
    let image = reader.decode()?.to_rgba8();
    let width_limit = render_width.clamp(MIN_WIDTH, MAX_WIDTH) as u32;

    if mode == MediaMode::Kitty {
        let (columns, rows) = kitty_cells(
            image.width(),
            image.height(),
            width_limit.min(MAX_KITTY_COLUMNS),
            MAX_KITTY_ROWS,
        );
        let fallback = || {
            Ok(media_status_edit(
                media,
                "Kitty upload unavailable; external",
                Some((width, height)),
                show_links,
            ))
        };
        if columns == 0 || rows == 0 {
            return fallback();
        }
        let image = imageops::resize(
            &image,
            columns * ASSUMED_CELL_WIDTH,
            rows * ASSUMED_CELL_HEIGHT,
            imageops::FilterType::Lanczos3,
        );
        let header = media_header(media, width, height, "kitty", show_links);
        return kitty_image_edit(media.output.clone(), &image, columns, rows, header, true)
            .or_else(|_| fallback());
    }

    // Half-block pixels are square on a 1:2 cell, so fitting the pixel grid to
    // the pane preserves the image's own proportions.
    let (target_width, target_height) = fit_dimensions(
        image.width(),
        image.height(),
        width_limit,
        (MAX_MEDIA_HEIGHT_CELLS * 2) as u32,
    );
    anyhow::ensure!(target_width > 0, "image has no pixels");
    let image = imageops::resize(
        &image,
        target_width,
        target_height,
        imageops::FilterType::Lanczos3,
    );
    let cells = image.width() as usize * image.height().div_ceil(2) as usize;
    anyhow::ensure!(
        cells <= MAX_MEDIA_CELLS,
        "rendered image exceeds cell limit"
    );

    let mut text = media_header(media, width, height, "unicode", show_links);
    let mut styles = Vec::with_capacity(cells + 1);
    if !text.is_empty() {
        styles.push(StyledRange {
            range: 0..text.chars().count(),
            style: RenderStyle::Scope("ui.text.inactive".to_string()),
        });
    }
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
    columns: u32,
    rows: u32,
    header: String,
    newline_after_last: bool,
) -> anyhow::Result<MediaEdit> {
    anyhow::ensure!(columns > 0 && rows > 0, "Kitty image has no cells");
    anyhow::ensure!(
        columns <= MAX_KITTY_COLUMNS && rows <= MAX_KITTY_ROWS,
        "Kitty placement exceeds cell limits"
    );

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
    show_links: bool,
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
        show_links,
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

/// Crop fully transparent borders.  The ink is what has to survive the
/// downscale, so padding around it is spent resolution.
fn trim_transparent(image: &RgbaImage) -> RgbaImage {
    let opaque = |x: u32, y: u32| image.get_pixel(x, y)[3] != 0;
    let mut left = image.width();
    let mut right = 0u32;
    let mut top = image.height();
    let mut bottom = 0u32;
    for y in 0..image.height() {
        for x in 0..image.width() {
            if opaque(x, y) {
                left = left.min(x);
                right = right.max(x);
                top = top.min(y);
                bottom = bottom.max(y);
            }
        }
    }
    if left > right || top > bottom {
        return image.clone();
    }
    imageops::crop_imm(image, left, top, right - left + 1, bottom - top + 1).to_image()
}

/// Scale `(width, height)` to fit inside the bounds while preserving the
/// aspect ratio, and never enlarge: an image smaller than the pane should stay
/// its own size rather than being blown up to fill it.
fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 {
        return (0, 0);
    }
    let scale = (max_width as f32 / width as f32)
        .min(max_height as f32 / height as f32)
        .min(1.0);
    (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    )
}

/// Choose the cell rectangle a Kitty placement should occupy.  Kitty stretches
/// the image to exactly fill it, so the aspect ratio has to live in the cell
/// counts, not in the pixels.
fn kitty_cells(width: u32, height: u32, max_columns: u32, max_rows: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let natural_columns = width.div_ceil(ASSUMED_CELL_WIDTH).max(1);
    let mut columns = natural_columns.min(max_columns).max(1);
    let rows_for = |columns: u32| {
        (((columns as f32 * height as f32) / (width as f32 * CELL_ASPECT)).round() as u32).max(1)
    };
    let mut rows = rows_for(columns);
    if rows > max_rows {
        rows = max_rows;
        columns = (((rows as f32 * CELL_ASPECT * width as f32) / height as f32).round() as u32)
            .clamp(1, max_columns);
    }
    (columns, rows)
}

fn media_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_MEDIA_DIMENSION);
    limits.max_image_height = Some(MAX_MEDIA_DIMENSION);
    limits.max_alloc = Some(MAX_MEDIA_PIXELS.saturating_mul(4));
    limits
}

/// The caption above a decoded image.  Once the pixels are on screen the alt
/// text and the URL are both noise, so unless links are explicitly requested
/// there is no caption at all.
fn media_header(media: &Media, width: u32, height: u32, mode: &str, show_links: bool) -> String {
    if show_links {
        format!(
            "🖼 {} — {} [{}×{}; {}]\n",
            media.alt, media.destination, width, height, mode
        )
    } else {
        String::new()
    }
}

fn media_status_edit(
    media: &Media,
    status: &str,
    dimensions: Option<(u32, u32)>,
    show_links: bool,
) -> MediaEdit {
    let dimensions = dimensions
        .map(|(width, height)| format!("{width}×{height}; "))
        .unwrap_or_default();
    // No pixels are shown here, so the alt text stays: it is the only
    // description the reader gets.
    let destination = if show_links {
        format!(" — {}", media.destination)
    } else {
        String::new()
    };
    let text = format!("🖼 {}{} [{}{}]", media.alt, destination, dimensions, status);
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

/// Same resolution as `theme_background`, but an unset foreground means "the
/// terminal's default text colour", which is light rather than black.
fn theme_foreground(color: Option<Color>) -> [u8; 3] {
    match color {
        Some(Color::Reset) | None => [220, 220, 220],
        color => theme_background(color),
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
    updated.output_mapping_index = MappingIndex::new(&updated.mappings, true);
    updated.source_mapping_index = MappingIndex::new(&updated.mappings, false);
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
    list(render.0.links.iter().map(link_value))
}

fn link_value(link: &Link) -> SteelVal {
    list_value(vec![
        link.label.clone().into_steelval().unwrap(),
        link.destination.clone().into_steelval().unwrap(),
        link.resolved.into_steelval().unwrap(),
        integer(link.output.start),
        integer(link.output.end),
        integer(link.source.start),
        integer(link.source.end),
    ])
}

fn markdown_render_link_at_output(render: &SteelMarkdownRender, output: usize) -> Option<SteelVal> {
    link_at_output(&render.0.links, output).map(link_value)
}

fn link_at_output(links: &[Link], output: usize) -> Option<&Link> {
    let index = links.partition_point(|link| link.output.end <= output);
    links
        .get(index)
        .filter(|link| link.output.contains(&output))
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
    show_links: bool,
    callback: SteelVal,
) -> anyhow::Result<()> {
    let mode = MediaMode::parse(&mode)?;
    let background = theme_background(cx.editor.theme.get("ui.background").bg);
    // Rasterized math is painted in the theme's own text colour so it stays
    // legible on light and dark themes alike.
    let foreground = theme_foreground(cx.editor.theme.get("ui.text").fg);
    let true_color = cx.editor.config.load().true_color || crate::true_color();
    let rooted = callback.as_rooted();
    let future = async move {
        let render = tokio::task::spawn_blocking(move || {
            render_local_media(
                render,
                mode,
                background,
                foreground,
                true_color,
                allow_remote,
                raster_math,
                show_links,
            )
        })
        .await
        .map_err(|error| helix_lsp::Error::Other(anyhow::Error::from(error)))?;
        Ok::<_, helix_lsp::Error>(MarkdownCallbackValue(render))
    };
    super::super::create_callback(cx, future, rooted)
}

fn source_for_output(render: &SteelMarkdownRender, output: usize) -> Option<usize> {
    nearest_mapping(
        &render.0.mappings,
        &render.0.output_mapping_index,
        output,
        true,
    )
}

fn output_for_source(render: &SteelMarkdownRender, source: usize) -> Option<usize> {
    nearest_mapping(
        &render.0.mappings,
        &render.0.source_mapping_index,
        source,
        false,
    )
}

fn nearest_mapping(
    mappings: &[SourceMap],
    index: &MappingIndex,
    position: usize,
    output_to_source: bool,
) -> Option<usize> {
    let start_position = index.by_start.partition_point(|mapping| {
        mapping_range(&mappings[*mapping], output_to_source).start <= position
    });

    // Find containing ranges. Source ranges may overlap, so the prefix maximum
    // lets the backward walk stop as soon as no earlier interval can contain
    // the position. Ties retain the original mapping order.
    let mut containing = None;
    let mut cursor = start_position;
    while cursor > 0 && index.prefix_max_end[cursor - 1] > position {
        cursor -= 1;
        let mapping = index.by_start[cursor];
        if mapping_range(&mappings[mapping], output_to_source).contains(&position) {
            containing = Some(containing.map_or(mapping, |current: usize| current.min(mapping)));
        }
    }

    let mapping_index = if let Some(mapping) = containing {
        mapping
    } else {
        let mut candidates = Vec::with_capacity(2);
        let end_position = index.by_end.partition_point(|mapping| {
            mapping_range(&mappings[*mapping], output_to_source).end <= position
        });
        if end_position > 0 {
            let best_end =
                mapping_range(&mappings[index.by_end[end_position - 1]], output_to_source).end;
            let first = index.by_end[..end_position].partition_point(|mapping| {
                mapping_range(&mappings[*mapping], output_to_source).end < best_end
            });
            if let Some(mapping) = index.by_end[first..end_position].iter().min() {
                candidates.push(*mapping);
            }
        }
        if start_position < index.by_start.len() {
            let best_start =
                mapping_range(&mappings[index.by_start[start_position]], output_to_source).start;
            let end = start_position
                + index.by_start[start_position..].partition_point(|mapping| {
                    mapping_range(&mappings[*mapping], output_to_source).start == best_start
                });
            if let Some(mapping) = index.by_start[start_position..end].iter().min() {
                candidates.push(*mapping);
            }
        }
        *candidates.iter().min_by_key(|mapping| {
            let range = mapping_range(&mappings[**mapping], output_to_source);
            (
                range
                    .start
                    .abs_diff(position)
                    .min(range.end.abs_diff(position)),
                **mapping,
            )
        })?
    };
    let mapping = &mappings[mapping_index];
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

/// Character offset of the first line the focused view has scrolled to.
/// Restoring it is what keeps a re-rendered preview visually still; the cursor
/// alone only guarantees the cursor stays on screen, not where the page sits.
fn view_anchor(cx: &mut Context) -> usize {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    cx.editor
        .documents
        .get(&doc_id)
        .map(|doc| doc.view_offset(view_id).anchor)
        .unwrap_or(0)
}

fn set_view_anchor(cx: &mut Context, anchor: usize) -> bool {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return false;
    };
    let anchor = anchor.min(doc.text().len_chars());
    let mut offset = doc.view_offset(view_id);
    offset.anchor = anchor;
    doc.set_view_offset(view_id, offset);
    true
}

/// Scroll the focused view so its cursor is visible.  Helix scrolls per command
/// rather than on every selection change, so a selection set from Steel leaves
/// the viewport where it was; this is the same opt-in the native `goto`
/// adapters make in `navigation.rs`.  `center` is for deliberate long jumps,
/// where merely clipping the cursor into view leaves no context around it.
fn ensure_visible(cx: &mut Context, center: bool) -> bool {
    let scrolloff = cx.editor.config().scrolloff;
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    if !cx.editor.documents.contains_key(&doc_id) {
        return false;
    }
    let (view, doc) = current!(cx.editor);
    if center {
        view.ensure_cursor_in_view_center(doc, scrolloff);
    } else {
        view.ensure_cursor_in_view(doc, scrolloff);
    }
    true
}

/// Keep the focused buffer even though it is unmodified and pathless, which
/// `Action::Replace` otherwise reads as "disposable scratch".
fn pin_focused(cx: &mut Context) -> bool {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return false;
    };
    doc.pinned = true;
    true
}

/// Put text straight into the yank register.  The preview's code blocks are
/// framed, so there is no buffer range that holds the code and nothing else.
fn copy_text(cx: &mut Context, text: String) -> anyhow::Result<()> {
    let register = cx.editor.config().default_yank_register;
    cx.editor.registers.write(register, vec![text])
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
        .register_fn(
            "markdown-render-link-at-output",
            markdown_render_link_at_output,
        )
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
        .register_fn_with_ctx(CTX, "markdown-render-view-anchor", view_anchor)
        .register_fn_with_ctx(CTX, "markdown-render-set-view-anchor!", set_view_anchor)
        .register_fn_with_ctx(CTX, "markdown-render-ensure-visible!", ensure_visible)
        .register_fn_with_ctx(CTX, "markdown-preview-pin-focused!", pin_focused)
        .register_fn_with_ctx(CTX, "markdown-preview-copy-text!", copy_text)
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
    fn separators_between_inline_runs_survive() {
        let mut builder = Builder::new(80);
        // `a *b* c` reaches the builder as three runs whose spaces sit at the
        // edges that `split_whitespace` used to discard.
        builder.emit_wrapped("a ", &[], 0..2, "text:0", "");
        builder.emit_wrapped("b", &[], 2..3, "text:1", "");
        builder.emit_wrapped(" c", &[], 3..5, "text:2", "");
        assert_eq!(builder.text, "a b c");

        // Runs that abut in the source must not gain one.
        let mut builder = Builder::new(80);
        builder.emit_wrapped("a", &[], 0..1, "text:0", "");
        builder.emit_wrapped("b", &[], 1..2, "text:1", "");
        assert_eq!(builder.text, "ab");
    }

    #[test]
    fn a_pending_space_never_starts_a_line() {
        let mut builder = Builder::new(80);
        builder.emit_wrapped("a ", &[], 0..2, "text:0", "");
        builder.newline();
        builder.emit_wrapped("b", &[], 2..3, "text:1", "");
        assert_eq!(builder.text, "a\nb");
    }

    #[test]
    fn html_comments_are_removed_across_events() {
        let mut open = false;
        assert_eq!(strip_html("<!-- hidden -->", &mut open).0, "");
        assert!(!open);

        // A block comment is reported one line per event.
        assert_eq!(strip_html("<!-- start", &mut open).0, "");
        assert!(open);
        assert_eq!(strip_html("still hidden", &mut open).0, "");
        assert_eq!(strip_html("end --> after", &mut open).0, " after");
        assert!(!open);
    }

    #[test]
    fn html_tags_are_stripped_rather_than_escaped() {
        let mut open = false;
        assert_eq!(strip_html("<b>bold</b>", &mut open).0, "bold");
        assert_eq!(strip_html("a<br/>b", &mut open), ("ab".to_string(), 1));
        // A stray `<` is text, not the start of a tag.
        assert_eq!(strip_html("2 < 3", &mut open).0, "2 < 3");
    }

    #[test]
    fn header_cells_survive_a_headless_table_row_event() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let mut options = Options::ENABLE_GFM;
        options.insert(Options::ENABLE_TABLES);
        let mut table: Option<TableState> = None;
        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            match (&event, table.is_some()) {
                (Event::Start(Tag::Table(alignments)), _) => {
                    table = Some(TableState {
                        alignments: alignments.clone(),
                        rows: Vec::new(),
                        row: Vec::new(),
                        cell: None,
                        header_rows: 0,
                        in_head: false,
                        source: range,
                    })
                }
                (Event::End(TagEnd::Table), _) => break,
                (_, true) => capture_table_event(table.as_mut().unwrap(), &event, range),
                _ => {}
            }
        }
        let table = table.expect("table was parsed");
        assert_eq!(table.header_rows, 1);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0][0].text, "a");
        assert_eq!(table.rows[1][0].text, "1");
    }

    #[test]
    fn fitting_preserves_aspect_and_never_enlarges() {
        // A wide image constrained by width keeps its proportions.
        assert_eq!(fit_dimensions(400, 100, 80, 120), (80, 20));
        // Constrained by height instead.
        assert_eq!(fit_dimensions(100, 400, 80, 40), (10, 40));
        // Smaller than the bounds: left alone rather than blown up.
        assert_eq!(fit_dimensions(30, 20, 80, 120), (30, 20));
    }

    #[test]
    fn kitty_cells_account_for_the_cell_aspect() {
        // A square image needs half as many rows as columns on a 1:2 cell.
        let (columns, rows) = kitty_cells(256, 256, 64, 64);
        assert_eq!(columns, 32);
        assert_eq!(rows, 16);
        // Row-limited images shed columns instead of stretching.
        let (columns, rows) = kitty_cells(100, 4000, 64, 10);
        assert_eq!(rows, 10);
        assert!(
            columns < 13,
            "columns {columns} should shrink with the rows"
        );
        assert!(columns >= 1);
    }

    #[test]
    fn code_blocks_are_framed_and_padded_evenly() {
        let mut builder = Builder::new(40);
        let node = builder.node("code");
        // Exercise the framing without a Context by driving the same helpers.
        let code = expand_tabs("fn a() {\n\tb();\n}\n");
        assert!(!code.contains('\t'));
        let chunks = wrap_code_line("aaaaaa", 4);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "aaaa");
        assert_eq!(chunks[0].range, 0..4);
        assert_eq!(chunks[1].range, 4..6);
        // `node` is consumed only to keep the builder's counter honest.
        assert!(node.starts_with("code:"));
    }

    #[test]
    fn emphasis_carries_a_modifier_of_its_own() {
        assert_eq!(active_modifier(&[TagEnd::Strong]), Modifier::BOLD);
        assert_eq!(active_modifier(&[TagEnd::Emphasis]), Modifier::ITALIC);
        assert_eq!(
            active_modifier(&[TagEnd::Strong, TagEnd::Emphasis]),
            Modifier::BOLD | Modifier::ITALIC
        );

        // The modifier is emitted as a concrete style beside the theme scope,
        // and carries no colour so the theme keeps owning colour.
        let mut builder = Builder::new(80);
        builder.emphasis = Modifier::BOLD;
        builder.emit_raw("bold", &["markup.bold"], 0..4, "text:0");
        let concrete: Vec<_> = builder
            .styles
            .iter()
            .filter_map(|span| match &span.style {
                RenderStyle::Concrete(style) => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(concrete.len(), 1);
        assert!(concrete[0].add_modifier.contains(Modifier::BOLD));
        assert!(concrete[0].fg.is_none() && concrete[0].bg.is_none());
    }

    #[test]
    fn transparent_margins_are_cropped_away() {
        // Ink in the middle, fully transparent border all around.
        let mut image = RgbaImage::from_pixel(10, 6, Rgba([0, 0, 0, 0]));
        image.put_pixel(4, 2, Rgba([255, 255, 255, 255]));
        image.put_pixel(5, 3, Rgba([255, 255, 255, 255]));
        let trimmed = trim_transparent(&image);
        assert_eq!(trimmed.dimensions(), (2, 2));

        // A fully transparent image has no ink to centre on; leave it alone
        // rather than returning a zero-sized buffer.
        let empty = RgbaImage::from_pixel(3, 3, Rgba([0, 0, 0, 0]));
        assert_eq!(trim_transparent(&empty).dimensions(), (3, 3));
    }

    #[test]
    fn display_math_is_typeset_at_its_natural_width() {
        // `\[ ... \]` fills the text width and `tightpage` crops the page, not
        // the ink, so the formula would arrive as a sliver of blank paper.
        let formula = Formula {
            tex: r"\sum_1^9 x_i".into(),
            display: true,
            output: 0..6,
            source: 0..12,
        };
        let document = math_document(&formula);
        assert!(document.contains(r"$\displaystyle \sum_1^9 x_i$"));
        assert!(!document.contains(r"\["));

        let inline = Formula {
            display: false,
            ..formula
        };
        assert!(math_document(&inline).contains(r"$\sum_1^9 x_i$"));
    }

    #[test]
    fn a_hidden_destination_leaves_only_the_caption() {
        let media = Media {
            alt: "diagram".into(),
            destination: "/tmp/a.png".into(),
            resolved: true,
            remote: false,
            output: 0..4,
            source: 0..4,
        };
        // Decoded pixels need no caption at all.
        assert_eq!(media_header(&media, 10, 10, "unicode", false), "");
        assert!(media_header(&media, 10, 10, "unicode", true).contains("/tmp/a.png"));
        // A placeholder keeps the alt text, since nothing else describes it.
        let status = media_status_edit(&media, "external", None, false);
        assert!(status.text.contains("diagram"));
        assert!(!status.text.contains("/tmp/a.png"));
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
    fn builder_tracks_character_offsets_without_rescanning_text() {
        let mut builder = Builder::new(80);
        builder.emit_raw("a界", &["ui.text"], 0..2, "text:0");
        builder.newline();
        builder.emit_raw("β", &["ui.text"], 2..3, "text:1");
        assert_eq!(builder.char_len(), 4);
        let render = builder.finish("a界\nβ".into());
        assert_eq!(render.text.chars().count(), 5);
        assert_eq!(render.mappings[1].output, 3..4);
    }

    #[test]
    fn indexed_mapping_matches_linear_nearest_semantics() {
        let mappings = vec![
            SourceMap {
                output: 0..4,
                source: 20..24,
                node: "first".into(),
            },
            SourceMap {
                output: 8..12,
                source: 0..10,
                node: "second".into(),
            },
            SourceMap {
                output: 16..20,
                source: 5..7,
                node: "third".into(),
            },
        ];

        let linear = |position: usize, output: bool| {
            let mapping = mappings
                .iter()
                .min_by_key(|mapping| {
                    let range = if output {
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
                })
                .unwrap();
            let (from, to) = if output {
                (&mapping.output, &mapping.source)
            } else {
                (&mapping.source, &mapping.output)
            };
            let offset = position
                .saturating_sub(from.start)
                .min(from.end.saturating_sub(from.start));
            to.start + offset.min(to.end.saturating_sub(to.start))
        };

        for output in [true, false] {
            let index = MappingIndex::new(&mappings, output);
            for position in 0..32 {
                assert_eq!(
                    nearest_mapping(&mappings, &index, position, output),
                    Some(linear(position, output)),
                    "position {position}, output={output}"
                );
            }
        }
    }

    #[test]
    fn link_lookup_uses_half_open_output_ranges() {
        let links = vec![
            Link {
                label: "first".into(),
                destination: "one".into(),
                resolved: true,
                output: 4..8,
                source: 0..4,
            },
            Link {
                label: "second".into(),
                destination: "two".into(),
                resolved: true,
                output: 12..16,
                source: 5..9,
            },
        ];
        assert!(link_at_output(&links, 3).is_none());
        assert_eq!(link_at_output(&links, 4).unwrap().label, "first");
        assert_eq!(link_at_output(&links, 7).unwrap().label, "first");
        assert!(link_at_output(&links, 8).is_none());
        assert_eq!(link_at_output(&links, 15).unwrap().label, "second");
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
        let rendered = render_local_media(
            render,
            MediaMode::Unicode,
            [0, 0, 0],
            [255, 255, 255],
            true,
            false,
            false,
            false,
        );

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
            // Only display math rasterizes outside Kitty: an inline half-block
            // run has a single cell of vertical room.
            display: true,
            output: 0..6,
            source: 0..12,
        };
        let edit = raster_math_edit(
            &formula,
            80,
            MediaMode::Unicode,
            [0, 0, 0],
            [255, 255, 255],
            true,
        )
        .unwrap();
        assert!(edit.text.contains('▀'));
        assert!(edit
            .styles
            .iter()
            .any(|style| matches!(style.style, RenderStyle::Concrete(_))));

        let inline = Formula {
            display: false,
            ..formula.clone()
        };
        assert!(raster_math_edit(
            &inline,
            80,
            MediaMode::Unicode,
            [0, 0, 0],
            [255, 255, 255],
            true
        )
        .is_err());

        let invalid = Formula {
            tex: r"\definitelyMissingCommand".into(),
            ..formula
        };
        assert!(generate_math_png(&invalid, [255, 255, 255]).is_err());
    }
}

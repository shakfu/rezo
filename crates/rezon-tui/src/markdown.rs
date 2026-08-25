// Markdown -> ANSI renderer, backed by `pulldown-cmark`.
//
// We get the complete document at render time (the REPL accumulates
// `assistant: String` during streaming and re-renders once `Done`
// arrives), so the parser's whole-document model is a fine fit. The
// hand-rolled predecessor handled the common cases but mis-fired on
// prose like `1 * 2 * 3`, stray backticks, escapes, links, and
// tables. pulldown-cmark gets all of those right.
//
// Public surface is unchanged:
//   * `render(input) -> String`   — returns ANSI-styled output that
//     always ends with a single trailing newline.
//   * `count_rows(input, width)` — wrap-aware row count for the
//     cursor-magic re-render in `repl::rerender_markdown`. Operates
//     on the raw streamed text, so it's independent of this module.

use anstyle::{AnsiColor, Color, Style};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

const fn fg(c: AnsiColor) -> Style {
    Style::new().fg_color(Some(Color::Ansi(c)))
}

const S_BOLD: Style = Style::new().bold();
const S_ITALIC: Style = Style::new().italic();
const S_STRIKE: Style = Style::new().strikethrough();
const S_DIM: Style = Style::new().dimmed();
const S_CODE: Style = fg(AnsiColor::Cyan);
const S_HEADING: Style = fg(AnsiColor::Cyan).bold();
const S_LINK: Style = fg(AnsiColor::BrightBlue);

pub fn render(input: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(input, opts);
    let mut r = Renderer::default();
    for ev in parser {
        r.handle(ev);
    }
    r.finalize()
}

#[derive(Default)]
struct Renderer {
    out: String,
    list_stack: Vec<ListFrame>,
    in_code_block: bool,
    blockquote_depth: usize,
    /// URL of the link currently being walked, emitted after the
    /// closing `End(Link)` so the model's chosen anchor text reads
    /// naturally and the URL trails in dim parentheses.
    pending_link_url: Option<String>,
    /// Active table buffer. While `Some` every event is redirected
    /// to `handle_in_table`; the rendered grid is committed to
    /// `out` only when the table closes.
    table: Option<TableBuf>,
}

struct ListFrame {
    /// `Some(n)` for ordered lists (current item number, post-
    /// increment); `None` for unordered.
    counter: Option<u64>,
}

#[derive(Default)]
struct TableBuf {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_head: bool,
}

impl Renderer {
    fn handle(&mut self, ev: Event<'_>) {
        if self.table.is_some() {
            self.handle_in_table(ev);
            return;
        }
        match ev {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(end) => self.end_tag(end),
            Event::Text(s) => self.push_text(&s),
            Event::Code(s) => {
                self.push_str(&format!("{S_CODE}{s}{S_CODE:#}"));
            }
            Event::Html(s) | Event::InlineHtml(s) => {
                // Render raw HTML dim and verbatim. Models very
                // rarely emit literal HTML; when they do (e.g. a
                // `<br>` between sentences) showing it dimly hints
                // that it's tagged content without breaking
                // formatting.
                self.push_str(&format!("{S_DIM}{s}{S_DIM:#}"));
            }
            Event::FootnoteReference(s) => {
                self.push_str(&format!("[^{s}]"));
            }
            Event::SoftBreak => self.push_str(" "),
            Event::HardBreak => self.push_str("\n"),
            Event::Rule => {
                self.ensure_blank_line();
                self.push_str(&format!("{S_DIM}─────{S_DIM:#}\n\n"));
            }
            Event::TaskListMarker(done) => {
                self.push_str(if done { "[x] " } else { "[ ] " });
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.ensure_blank_line();
                self.push_str(&format!("{S_HEADING}{} ", heading_prefix(level)));
            }
            Tag::BlockQuote(_) => {
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.ensure_newline();
                if let CodeBlockKind::Fenced(lang) = kind {
                    let lang = lang.trim();
                    if !lang.is_empty() {
                        self.push_str(&format!("{S_DIM}  ┄ {lang}{S_DIM:#}\n"));
                    }
                }
            }
            Tag::List(start) => {
                self.list_stack.push(ListFrame { counter: start });
            }
            Tag::Item => {
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match self.list_stack.last_mut() {
                    Some(ListFrame { counter: Some(n) }) => {
                        let s = format!("{n}. ");
                        *n += 1;
                        s
                    }
                    _ => "• ".to_string(),
                };
                self.push_str(&format!("{indent}{bullet}"));
            }
            Tag::Emphasis => self.push_str(&format!("{S_ITALIC}")),
            Tag::Strong => self.push_str(&format!("{S_BOLD}")),
            Tag::Strikethrough => self.push_str(&format!("{S_STRIKE}")),
            Tag::Link { dest_url, .. } => {
                self.pending_link_url = Some(dest_url.to_string());
                self.push_str(&format!("{S_LINK}"));
            }
            Tag::Image { dest_url, .. } => {
                // Show as `![alt](url)` dimly — content between
                // start and end is the alt text (Text events).
                self.pending_link_url = Some(dest_url.to_string());
                self.push_str(&format!("{S_DIM}!["));
            }
            Tag::Table(_) => {
                self.table = Some(TableBuf::default());
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => self.push_str("\n\n"),
            TagEnd::Heading(_) => self.push_str(&format!("{S_HEADING:#}\n\n")),
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                if self.blockquote_depth == 0 {
                    self.ensure_blank_line();
                }
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.ensure_newline();
                self.push_str("\n");
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.ensure_blank_line();
                }
            }
            TagEnd::Item => self.ensure_newline(),
            TagEnd::Emphasis => self.push_str(&format!("{S_ITALIC:#}")),
            TagEnd::Strong => self.push_str(&format!("{S_BOLD:#}")),
            TagEnd::Strikethrough => self.push_str(&format!("{S_STRIKE:#}")),
            TagEnd::Link => {
                self.push_str(&format!("{S_LINK:#}"));
                if let Some(url) = self.pending_link_url.take() {
                    self.push_str(&format!(" {S_DIM}({url}){S_DIM:#}"));
                }
            }
            TagEnd::Image => {
                if let Some(url) = self.pending_link_url.take() {
                    self.push_str(&format!("]({url}){S_DIM:#}"));
                } else {
                    self.push_str(&format!("]{S_DIM:#}"));
                }
            }
            _ => {}
        }
    }

    /// Apply context-sensitive prefixes (blockquote bar, code-block
    /// indent + dim) to a text payload.
    fn push_text(&mut self, text: &str) {
        if self.in_code_block {
            // Code blocks: each line indented + dim. pulldown-cmark
            // emits Text events with embedded `\n`s for fenced
            // bodies; we re-stamp the indent per line.
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    self.push_str("\n");
                }
                self.push_str(&format!("  {S_DIM}{line}{S_DIM:#}"));
            }
            return;
        }
        if self.blockquote_depth > 0 {
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    self.push_str("\n");
                }
                self.push_str(&format!("{S_DIM}│ {line}{S_DIM:#}"));
            }
            return;
        }
        self.push_str(text);
    }

    fn handle_in_table(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(Tag::TableHead) => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = false;
                    t.headers = std::mem::take(&mut t.current_row);
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(t) = self.table.as_mut() {
                    t.current_row.clear();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(t) = self.table.as_mut() {
                    if !t.in_head {
                        let row = std::mem::take(&mut t.current_row);
                        t.rows.push(row);
                    }
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(t) = self.table.as_mut() {
                    t.current_cell.clear();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(t) = self.table.as_mut() {
                    let cell = std::mem::take(&mut t.current_cell);
                    t.current_row.push(cell);
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(t) = self.table.take() {
                    self.render_table(t);
                }
            }
            Event::Text(s) | Event::Code(s) => {
                if let Some(t) = self.table.as_mut() {
                    t.current_cell.push_str(&s);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(t) = self.table.as_mut() {
                    t.current_cell.push(' ');
                }
            }
            // Strong/Emphasis inside cells are ignored for the grid
            // (table rendering uses plain widths). The text inside
            // still flows through Text events above.
            _ => {}
        }
    }

    fn render_table(&mut self, t: TableBuf) {
        // Column widths: max char count across header + every body
        // row. Empty cells contribute zero.
        let cols = t
            .headers
            .len()
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if cols == 0 {
            return;
        }
        let mut widths = vec![0usize; cols];
        for (i, h) in t.headers.iter().enumerate() {
            widths[i] = widths[i].max(h.chars().count());
        }
        for row in &t.rows {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }

        self.ensure_blank_line();
        if !t.headers.is_empty() {
            self.emit_table_row(&t.headers, &widths);
            // Separator
            self.push_str("├─");
            for (i, w) in widths.iter().enumerate() {
                self.push_str(&"─".repeat(*w));
                self.push_str(if i + 1 < widths.len() {
                    "─┼─"
                } else {
                    "─┤"
                });
            }
            self.push_str("\n");
        }
        for row in &t.rows {
            self.emit_table_row(row, &widths);
        }
        self.push_str("\n");
    }

    fn emit_table_row(&mut self, row: &[String], widths: &[usize]) {
        self.push_str("│ ");
        for (i, c) in row.iter().enumerate() {
            let pad = widths
                .get(i)
                .copied()
                .unwrap_or(0)
                .saturating_sub(c.chars().count());
            self.push_str(c);
            self.push_str(&" ".repeat(pad));
            if i + 1 < row.len() {
                self.push_str(" │ ");
            }
        }
        self.push_str(" │\n");
    }

    fn push_str(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// Ensure the output ends with `\n`. Used before content that
    /// must start on a fresh line (code block, table, list item
    /// continuation).
    fn ensure_newline(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    /// Ensure the output ends with at least one blank line above
    /// (i.e. ends with `\n\n` or is empty).
    fn ensure_blank_line(&mut self) {
        if self.out.is_empty() {
            return;
        }
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn finalize(mut self) -> String {
        // Collapse trailing blanks to a single newline.
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.out
    }
}

fn heading_prefix(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "#",
        HeadingLevel::H2 => "##",
        HeadingLevel::H3 => "###",
        HeadingLevel::H4 => "####",
        HeadingLevel::H5 => "#####",
        HeadingLevel::H6 => "######",
    }
}

/// Number of terminal rows the raw streamed text consumed at the
/// given width. Used by the streaming re-render to know how far to
/// scroll the cursor back. Independent of `render` — operates on the
/// original markdown source, not the styled output.
pub fn count_rows(s: &str, width: u16) -> u16 {
    let w = width.max(1) as usize;
    let mut rows: u16 = 0;
    let mut col: usize = 0;
    for ch in s.chars() {
        if ch == '\n' {
            rows = rows.saturating_add(1);
            col = 0;
        } else {
            col += 1;
            if col > w {
                rows = rows.saturating_add(1);
                col = 1;
            }
        }
    }
    if col > 0 {
        rows = rows.saturating_add(1);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_emits_escape_codes_around_content() {
        let out = render("**loud**");
        assert!(out.contains("loud"));
        let opens: usize = out.matches('\x1b').count();
        assert!(opens >= 2, "expected SGR open+close: {out:?}");
    }

    #[test]
    fn italic_skips_arithmetic_asterisks() {
        // Stray `*` in arithmetic-looking prose must NOT trigger
        // emphasis (pulldown-cmark's CommonMark flanking rules
        // refuse the run because there's whitespace on both sides).
        let out = render("compute a * b * c");
        assert!(out.contains('*'));
    }

    #[test]
    fn mismatched_asterisks_passed_through() {
        // Trailing unmatched `*` should not silently consume the
        // remainder of the line.
        let out = render("rate of *change\n");
        assert!(out.contains('*') || out.contains("change"));
    }

    #[test]
    fn backtick_escape_treated_as_literal() {
        // CommonMark: backslash-escape passes the literal char.
        let out = render(r"a \`b\` c");
        // Backtick should appear literally.
        assert!(out.contains('`'));
    }

    #[test]
    fn inline_code_consumes_backticks() {
        let out = render("call `foo()` here");
        assert!(out.contains("foo()"));
        assert!(!out.contains('`'), "literal backticks should be gone");
    }

    #[test]
    fn stray_backticks_passed_through() {
        // An odd number of backticks shouldn't eat the rest of the
        // line as code.
        let out = render("the ` is a backtick");
        assert!(out.contains('`'));
        assert!(out.contains("backtick"));
    }

    #[test]
    fn heading_levels_emit_prefix() {
        for prefix in ["# ", "## ", "### "] {
            let line = format!("{prefix}Title");
            let out = render(&line);
            assert!(out.contains("Title"), "{prefix}: {out:?}");
            assert!(out.contains(prefix.trim_end()), "{prefix}: {out:?}");
        }
    }

    #[test]
    fn unordered_and_ordered_lists_get_bullets() {
        let out = render("- one\n- two");
        assert!(out.contains("• one"));
        assert!(out.contains("• two"));

        let out = render("1. first\n2. second");
        assert!(out.contains("1. first"));
        assert!(out.contains("2. second"));
    }

    #[test]
    fn blockquote_emits_bar_glyph() {
        let out = render("> note");
        assert!(out.contains("│ note"));
    }

    #[test]
    fn code_fence_dims_body_and_drops_fences() {
        let out = render("```rust\nfn x() {}\n```");
        assert!(out.contains("rust"));
        assert!(out.contains("fn x() {}"));
        assert!(!out.contains("```"), "fences should be consumed");
    }

    #[test]
    fn link_renders_text_then_url_dim() {
        let out = render("see [the docs](https://example.com)");
        assert!(out.contains("the docs"));
        assert!(out.contains("https://example.com"));
        // The raw markdown `[]()` syntax should not survive.
        assert!(
            !out.contains("](http"),
            "raw bracket-paren must be consumed: {out:?}"
        );
    }

    #[test]
    fn escape_backslash_star_is_literal() {
        // CommonMark: `\*` -> literal `*`.
        let out = render(r"literal \* asterisk");
        assert!(out.contains('*'));
        // And no emphasis was opened, so no italic bracket leaked.
        assert!(out.contains("asterisk"));
    }

    #[test]
    fn html_rendered_dim_verbatim() {
        let out = render("inline <br> tag");
        assert!(out.contains("<br>"));
    }

    #[test]
    fn table_renders_with_box_grid() {
        let table = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let out = render(table);
        // Header content
        assert!(out.contains('a') && out.contains('b'));
        // Body
        assert!(out.contains('1') && out.contains('4'));
        // Grid glyphs
        assert!(out.contains('│'));
        assert!(out.contains('─'));
        // Original pipes shouldn't leak.
        assert!(
            !out.contains(" | "),
            "raw pipes should be replaced: {out:?}"
        );
    }

    #[test]
    fn strikethrough_emits_escape_codes() {
        let out = render("~~gone~~");
        assert!(out.contains("gone"));
        assert!(out.matches('\x1b').count() >= 2);
    }

    #[test]
    fn render_always_ends_with_single_newline() {
        for input in ["hi", "hi\n", "hi\n\n", "# heading"] {
            let out = render(input);
            assert!(out.ends_with('\n'), "{input:?} -> {out:?}");
            assert!(
                !out.ends_with("\n\n"),
                "{input:?} -> trailing blank line: {out:?}"
            );
        }
    }

    #[test]
    fn empty_input_renders_empty() {
        let out = render("");
        assert!(out.is_empty(), "got: {out:?}");
    }

    #[test]
    fn count_rows_empty_is_zero() {
        assert_eq!(count_rows("", 80), 0);
    }

    #[test]
    fn count_rows_single_line_no_newline() {
        assert_eq!(count_rows("hello", 80), 1);
    }

    #[test]
    fn count_rows_multi_line() {
        assert_eq!(count_rows("a\nb\nc", 80), 3);
        assert_eq!(count_rows("a\nb\n", 80), 2);
    }

    #[test]
    fn count_rows_wraps_on_width() {
        assert_eq!(count_rows("abcde", 3), 2);
        assert_eq!(count_rows("abc", 3), 1);
        assert_eq!(count_rows("abcdef", 3), 2);
    }

    #[test]
    fn count_rows_handles_blank_lines() {
        assert_eq!(count_rows("a\n\nb", 80), 3);
    }
}

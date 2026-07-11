//! Model-card (README) parsing: lower the Hub's mix of Markdown and presentational HTML into a typed block model both frontends render natively.

use serde::{Deserialize, Serialize};

/// A parsed model card: YAML frontmatter (set aside verbatim) plus the body as
/// renderable blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Readme {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<String>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    Code {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
        code: String,
    },
    ListItem {
        /// Nesting depth (0 = top level).
        indent: u8,
        /// `Some(n)` for ordered lists, `None` for bullets.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ordered: Option<u32>,
        spans: Vec<Span>,
    },
    Quote {
        spans: Vec<Span>,
    },
    Table {
        header: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Image {
        src: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        alt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
    },
    Divider,
}

/// One inline run of styled text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Span {
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub code: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strike: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Span {
    #[cfg(test)]
    fn plain(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            ..Default::default()
        }
    }
}

/// Parse a raw model card into blocks. Heavy enough that callers should run it
/// off the UI thread.
pub fn parse(raw: &str) -> Readme {
    let raw = raw.replace("\r\n", "\n");
    let (frontmatter, body) = split_frontmatter(&raw);
    let normalized = normalize(body);
    Readme {
        frontmatter: frontmatter.map(|s| s.to_string()),
        blocks: parse_blocks(&normalized),
    }
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

fn split_frontmatter(s: &str) -> (Option<&str>, &str) {
    let rest = s.strip_prefix("---\n").or_else(|| {
        s.strip_prefix("---")
            .filter(|r| r.is_empty() || r.starts_with('\n'))
    });
    let Some(rest) = rest else {
        return (None, s);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(fm.trim_end()), body);
        }
        offset += line.len();
    }
    (None, s)
}

// ---------------------------------------------------------------------------
// Normalization: HTML -> Markdown lowering.
//
// Placeholder tokens are built from U+FFFC + name + index + U+FFFC: they contain
// no angle brackets and no pipes, so every later pass leaves them untouched.
// ---------------------------------------------------------------------------

const OBJ: char = '\u{FFFC}';

fn placeholder(kind: &str, i: usize) -> String {
    format!("{OBJ}{kind}{i}{OBJ}")
}

struct Fenced {
    info: String,
    body: String,
}

struct HtmlCode {
    lang: Option<String>,
    body: String,
}

// The pass order is load-bearing: each step assumes the previous ones ran.
fn normalize(src: &str) -> String {
    let mut fenced = Vec::new();
    let s = extract_fenced_blocks(src, &mut fenced);
    let s = strip_html_comments(&s);
    let mut md_code = Vec::new();
    let s = extract_md_code_spans(&s, &mut md_code);
    let mut pre_blocks = Vec::new();
    let mut inline_code = Vec::new();
    let s = protect_code_html(&s, &mut pre_blocks, &mut inline_code);
    let s = html_tables_to_markdown(&s);
    let s = rewrite_tags(&s);
    let s = decode_entities(&s);
    let s = collapse_blank_lines(&s);
    let s = restore_code_html(&s, &pre_blocks, &inline_code);
    let s = restore_placeholders(&s, "MDCODE", &md_code);
    restore_fenced_blocks(&s, &fenced)
}

/// Pull ``` / ~~~ fenced blocks out so nothing downstream rewrites their contents.
fn extract_fenced_blocks(src: &str, out: &mut Vec<Fenced>) -> String {
    let mut result = String::with_capacity(src.len());
    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some((fence_char, fence_len, info)) = fence_open(line) {
            let mut body = String::new();
            let mut closed = false;
            for inner in lines.by_ref() {
                if fence_close(inner, fence_char, fence_len) {
                    closed = true;
                    break;
                }
                body.push_str(inner);
                body.push('\n');
            }
            let _ = closed; // an unclosed fence swallows the rest of the doc, per CommonMark
            if body.ends_with('\n') {
                body.pop();
            }
            result.push_str(&placeholder("FENCE", out.len()));
            result.push('\n');
            out.push(Fenced {
                info: info.to_string(),
                body,
            });
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

fn fence_open(line: &str) -> Option<(char, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let t = &line[indent..];
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let len = t.chars().take_while(|&ch| ch == c).count();
    if len < 3 {
        return None;
    }
    let info = t[len..].trim();
    // An info string containing a backtick is not a fence opener (CommonMark).
    if c == '`' && info.contains('`') {
        return None;
    }
    Some((c, len, info))
}

fn fence_close(line: &str, fence_char: char, fence_len: usize) -> bool {
    let t = line.trim();
    let len = t.chars().take_while(|&ch| ch == fence_char).count();
    len >= fence_len && t[len..].trim().is_empty()
}

fn restore_fenced_blocks(s: &str, fenced: &[Fenced]) -> String {
    let mut out = s.to_string();
    for (i, f) in fenced.iter().enumerate() {
        let lang = f.info.split_whitespace().next().unwrap_or("");
        let fence = "`".repeat(3.max(longest_run(&f.body, '`') + 1));
        let block = format!("{fence}{lang}\n{}\n{fence}", f.body);
        out = out.replace(&placeholder("FENCE", i), &block);
    }
    out
}

fn longest_run(s: &str, c: char) -> usize {
    let mut best = 0;
    let mut cur = 0;
    for ch in s.chars() {
        if ch == c {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

fn strip_html_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(off) = s[i..].find("<!--") {
        out.push_str(&s[i..i + off]);
        match s[i + off + 4..].find("-->") {
            Some(end) => i = i + off + 4 + end + 3,
            None => return out, // unclosed comment swallows the rest, per HTML
        }
    }
    out.push_str(&s[i..]);
    out
}

/// Protect Markdown backtick spans so tag-stripping and entity decoding can't
/// rewrite literal markup inside them (e.g. `<think>` or a `|`).
fn extract_md_code_spans(s: &str, out: &mut Vec<String>) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            let next = s[i..].find('`').map(|o| i + o).unwrap_or(s.len());
            result.push_str(&s[i..next]);
            i = next;
            continue;
        }
        let run = s[i..].bytes().take_while(|&b| b == b'`').count();
        // The closing run must be exactly the same length and on this side of a
        // blank line.
        let limit = s[i + run..]
            .find("\n\n")
            .map(|o| i + run + o)
            .unwrap_or(s.len());
        let mut j = i + run;
        let mut close = None;
        while j < limit {
            if bytes[j] == b'`' {
                let r = s[j..limit].bytes().take_while(|&b| b == b'`').count();
                if r == run {
                    close = Some(j);
                    break;
                }
                j += r;
            } else {
                j += 1;
            }
        }
        match close {
            Some(j) => {
                result.push_str(&placeholder("MDCODE", out.len()));
                out.push(s[i..j + run].to_string());
                i = j + run;
            }
            None => {
                result.push_str(&s[i..i + run]);
                i += run;
            }
        }
    }
    result
}

fn restore_placeholders(s: &str, kind: &str, items: &[String]) -> String {
    let mut out = s.to_string();
    for (i, item) in items.iter().enumerate() {
        out = out.replace(&placeholder(kind, i), item);
    }
    out
}

/// Pull `<pre>` (block) and `<code>/<kbd>/<samp>/<tt>` (inline) out into
/// placeholders. Block code first, so `<pre><code>` collapses into one block.
fn protect_code_html(
    s: &str,
    pre_blocks: &mut Vec<HtmlCode>,
    inline_code: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(lt) = find_open_tag(s, i, "pre") {
        out.push_str(&s[i..lt]);
        let Some(open_end) = tag_end(s, lt) else {
            out.push_str(&s[lt..]);
            return protect_inline_code(&out, inline_code);
        };
        let (body, after) = match find_matching_close(s, open_end, "pre") {
            Some((cs, ce)) => (&s[open_end..cs], ce),
            None => (&s[open_end..], s.len()),
        };
        let mut lang = attr(&s[lt..open_end], "class").and_then(|c| lang_from_class(&c));
        let mut body = body.trim_matches('\n').to_string();
        // Collapse an inner <code> wrapper (and take its language class).
        if let Some(code_lt) = find_open_tag(&body, 0, "code") {
            if body[..code_lt].trim().is_empty() {
                if let Some(code_end) = tag_end(&body, code_lt) {
                    if lang.is_none() {
                        lang = attr(&body[code_lt..code_end], "class")
                            .and_then(|c| lang_from_class(&c));
                    }
                    let inner = &body[code_end..];
                    let inner = match find_matching_close(inner, 0, "code") {
                        Some((cs, _)) => &inner[..cs],
                        None => inner,
                    };
                    body = inner.trim_matches('\n').to_string();
                }
            }
        }
        out.push('\n');
        out.push_str(&placeholder("PRE", pre_blocks.len()));
        out.push('\n');
        pre_blocks.push(HtmlCode { lang, body });
        i = after;
    }
    out.push_str(&s[i..]);
    protect_inline_code(&out, inline_code)
}

fn protect_inline_code(s: &str, out: &mut Vec<String>) -> String {
    let mut cur = s.to_string();
    for tag in ["code", "kbd", "samp", "tt"] {
        let mut rebuilt = String::with_capacity(cur.len());
        let mut i = 0;
        while let Some(lt) = find_open_tag(&cur, i, tag) {
            rebuilt.push_str(&cur[i..lt]);
            let Some(open_end) = tag_end(&cur, lt) else {
                i = lt;
                break;
            };
            let Some((cs, ce)) = find_matching_close(&cur, open_end, tag) else {
                // Unbalanced open tag: leave it; the stray-tag pass drops it.
                rebuilt.push_str(&cur[lt..open_end]);
                i = open_end;
                continue;
            };
            rebuilt.push_str(&placeholder("CODE", out.len()));
            out.push(cur[open_end..cs].to_string());
            i = ce;
        }
        rebuilt.push_str(&cur[i..]);
        cur = rebuilt;
    }
    cur
}

fn lang_from_class(class: &str) -> Option<String> {
    class.split_whitespace().find_map(|c| {
        c.strip_prefix("language-")
            .or_else(|| c.strip_prefix("lang-"))
            .map(|l| l.to_string())
    })
}

fn restore_code_html(s: &str, pre_blocks: &[HtmlCode], inline_code: &[String]) -> String {
    let mut out = s.to_string();
    for (i, p) in pre_blocks.iter().enumerate() {
        // HTML text nodes are entity-encoded; decode now that the global pass
        // can no longer touch this content.
        let body = decode_entities(&p.body);
        let fence = "`".repeat(3.max(longest_run(&body, '`') + 1));
        let block = format!(
            "{fence}{}\n{body}\n{fence}",
            p.lang.as_deref().unwrap_or("")
        );
        out = out.replace(&placeholder("PRE", i), &block);
    }
    for (i, c) in inline_code.iter().enumerate() {
        let body = decode_entities(c).replace('\n', " ");
        out = out.replace(&placeholder("CODE", i), &backtick_span(&body));
    }
    out
}

/// Wrap `body` in a backtick span, widening the fence when the body itself
/// contains backticks. An empty body yields nothing rather than a bare ``.
fn backtick_span(body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    let longest = longest_run(body, '`');
    if longest == 0 {
        return format!("`{body}`");
    }
    let fence = "`".repeat(longest + 1);
    format!("{fence} {body} {fence}")
}

// ---------------------------------------------------------------------------
// HTML tables -> Markdown pipe tables
// ---------------------------------------------------------------------------

fn html_tables_to_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(lt) = find_open_tag(s, i, "table") {
        let Some(open_end) = tag_end(s, lt) else {
            break;
        };
        let (inner, after) = match find_matching_close(s, open_end, "table") {
            Some((cs, ce)) => (&s[open_end..cs], ce),
            None => (&s[open_end..], s.len()),
        };
        match table_to_markdown(inner) {
            Some(md) => {
                out.push_str(&s[i..lt]);
                out.push_str("\n\n");
                out.push_str(&md);
                out.push_str("\n\n");
            }
            // Not table-shaped enough (< 2x2): leave it for the generic strip.
            None => out.push_str(&s[i..after]),
        }
        i = after;
    }
    out.push_str(&s[i..]);
    out
}

fn table_to_markdown(inner: &str) -> Option<String> {
    let mut grid: Vec<Vec<String>> = Vec::new();
    let mut i = 0;
    while let Some(lt) = find_open_tag(inner, i, "tr") {
        let Some(open_end) = tag_end(inner, lt) else {
            break;
        };
        let (row_html, after) = match find_matching_close(inner, open_end, "tr") {
            Some((cs, ce)) => (&inner[open_end..cs], ce),
            None => match find_open_tag(inner, open_end, "tr") {
                Some(next) => (&inner[open_end..next], next),
                None => (&inner[open_end..], inner.len()),
            },
        };
        let mut row: Vec<String> = Vec::new();
        for (cell_html, colspan) in table_cells(row_html) {
            row.push(sanitize_cell(&cell_html));
            // A colspan=N cell is padded with N-1 empties so columns stay aligned.
            for _ in 1..colspan {
                row.push(String::new());
            }
        }
        if !row.is_empty() {
            grid.push(row);
        }
        i = after;
    }
    let cols = grid.iter().map(|r| r.len()).max().unwrap_or(0);
    if grid.len() < 2 || cols < 2 {
        return None;
    }
    for row in &mut grid {
        row.resize(cols, String::new());
    }
    let mut md = String::new();
    let fmt_row = |cells: &[String]| {
        let mut line = String::from("|");
        for c in cells {
            line.push(' ');
            line.push_str(if c.is_empty() { " " } else { c });
            line.push_str(" |");
        }
        line
    };
    md.push_str(&fmt_row(&grid[0]));
    md.push('\n');
    md.push_str(&"| --- ".repeat(cols));
    md.push('|');
    for row in &grid[1..] {
        md.push('\n');
        md.push_str(&fmt_row(row));
    }
    Some(md)
}

fn table_cells(row_html: &str) -> Vec<(String, usize)> {
    let mut cells = Vec::new();
    let mut i = 0;
    loop {
        let td = find_open_tag(row_html, i, "td");
        let th = find_open_tag(row_html, i, "th");
        let (lt, tag) = match (td, th) {
            (Some(a), Some(b)) if a < b => (a, "td"),
            (Some(_) | None, Some(b)) => (b, "th"),
            (Some(a), None) => (a, "td"),
            (None, None) => break,
        };
        let Some(open_end) = tag_end(row_html, lt) else {
            break;
        };
        let colspan = attr(&row_html[lt..open_end], "colspan")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| (1..=32).contains(&n))
            .unwrap_or(1);
        // Cells may be left unclosed; the next cell-open (or row end) ends them.
        let close = find_matching_close(row_html, open_end, tag).map(|(cs, _)| cs);
        let next_td = find_open_tag(row_html, open_end, "td");
        let next_th = find_open_tag(row_html, open_end, "th");
        let next_open = match (next_td, next_th) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let end = [close, next_open]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(row_html.len());
        cells.push((row_html[open_end..end].to_string(), colspan));
        i = end.max(open_end + 1);
    }
    cells
}

/// Flatten cell HTML to one line of inline Markdown: keep bold / italic / links,
/// drop everything else, and swap literal pipes for a lookalike so the pipe
/// splitter can't misread them as column separators.
fn sanitize_cell(html: &str) -> String {
    // Decode pipe-producing entities early: after the swap below they'd
    // otherwise decode into real pipes during the global entity pass.
    let s = html
        .replace("&#124;", "\u{00A6}")
        .replace("&#x7c;", "\u{00A6}")
        .replace("&vert;", "\u{00A6}")
        .replace("&VerticalLine;", "\u{00A6}");
    let s = rewrite_tags(&s);
    let s = s.replace('|', "\u{00A6}");
    let mut flat = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                flat.push(' ');
            }
            last_space = true;
        } else {
            flat.push(ch);
            last_space = false;
        }
    }
    flat.trim().to_string()
}

// ---------------------------------------------------------------------------
// Generic tag rewriting (headings, links, emphasis, images, structure strip)
// ---------------------------------------------------------------------------

const STRUCTURAL_TAGS: &[&str] = &[
    "p",
    "div",
    "details",
    "summary",
    "center",
    "section",
    "article",
    "main",
    "header",
    "footer",
    "figure",
    "figcaption",
    "picture",
    "source",
    "video",
    "audio",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "th",
    "td",
    "ul",
    "ol",
    "blockquote",
    "nav",
    "aside",
    "form",
    "button",
    "colgroup",
    "col",
    "caption",
    "dl",
    "dt",
    "dd",
];

const INLINE_STRIP_TAGS: &[&str] = &[
    "span", "font", "small", "big", "sup", "sub", "u", "abbr", "mark", "ins", "time", "wbr",
];

fn rewrite_tags(s: &str) -> String {
    rewrite_tags_depth(s, 0)
}

// A README is attacker-publishable content: cap nesting-driven recursion so a
// pathological document can't overflow the stack.
const MAX_NEST: usize = 24;

fn rewrite_tags_depth(s: &str, depth: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(lt) = s[i..].find('<').map(|o| i + o) {
        out.push_str(&s[i..lt]);
        let Some((name, closing, _)) = tag_name(s, lt) else {
            out.push('<');
            i = lt + 1;
            continue;
        };
        let Some(end) = tag_end(s, lt) else {
            out.push('<');
            i = lt + 1;
            continue;
        };
        let tag_src = &s[lt..end];
        match name.as_str() {
            "br" => out.push('\n'),
            "hr" => out.push_str("\n\n---\n\n"),
            "img" if !closing => {
                if let Some(md) = img_to_markdown(tag_src) {
                    out.push_str(&md);
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if closing {
                    out.push_str("\n\n");
                } else {
                    let level = name[1..].parse::<usize>().unwrap_or(1);
                    out.push_str("\n\n");
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
            }
            "a" if !closing && depth < MAX_NEST => {
                let href = attr(tag_src, "href").unwrap_or_default();
                // Unbalanced <a> (no matching close) falls through: drop the tag, keep the flow.
                if let Some((cs, ce)) = find_matching_close(s, end, "a") {
                    let inner = rewrite_tags_depth(&s[end..cs], depth + 1);
                    let inner = inner.split_whitespace().collect::<Vec<_>>().join(" ");
                    if href.is_empty() {
                        out.push_str(&inner);
                    } else if inner.is_empty() {
                        out.push_str(&format!("[{href}]({href})"));
                    } else {
                        out.push_str(&format!("[{inner}]({href})"));
                    }
                    i = ce;
                    continue;
                }
            }
            "a" => {} // stray </a>
            "b" | "strong" => out.push_str("**"),
            "i" | "em" => out.push('*'),
            "s" | "del" | "strike" => out.push_str("~~"),
            "li" => {
                if !closing {
                    out.push_str("\n- ");
                } else {
                    out.push('\n');
                }
            }
            "script" | "style" | "svg" | "iframe" | "noscript" => {
                if !closing {
                    // Drop the content, not just the tags.
                    i = match find_matching_close(s, end, &name) {
                        Some((_, ce)) => ce,
                        None => s.len(),
                    };
                    continue;
                }
            }
            "code" | "pre" | "kbd" | "samp" | "tt" => {} // stray leftovers: drop
            n if STRUCTURAL_TAGS.contains(&n) => out.push('\n'),
            n if INLINE_STRIP_TAGS.contains(&n) => {}
            // Unknown tags (<think>, <|im_start|>-adjacent markup…) stay verbatim.
            _ => {
                out.push_str(tag_src);
            }
        }
        i = end;
    }
    out.push_str(&s[i..]);
    out
}

/// `<img>` -> `![alt](src)`, smuggling any width hint through as a URL-fragment
/// marker peeled back off when the block parser builds the Image block.
fn img_to_markdown(tag_src: &str) -> Option<String> {
    let src = attr(tag_src, "src")?;
    if src.is_empty() {
        return None;
    }
    let alt = attr(tag_src, "alt").unwrap_or_default();
    let alt = alt.replace(['[', ']'], " ");
    let width = attr(tag_src, "width")
        .and_then(|w| w.trim().trim_end_matches("px").trim().parse::<u32>().ok());
    match width {
        Some(w) => Some(format!("![{alt}]({src}#noema-w={w})")),
        None => Some(format!("![{alt}]({src})")),
    }
}

// ---------------------------------------------------------------------------
// Low-level tag scanning (regex can't balance nested tags; a depth counter can)
// ---------------------------------------------------------------------------

/// Byte index just past the closing `>` of the tag starting at `open` (which
/// must point at `<`). Walks respecting quote context.
fn tag_end(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = open + 1;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        match (quote, b[i]) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"') | (None, b'\'') => quote = Some(b[i]),
            (None, b'>') => return Some(i + 1),
            (None, _) => {}
        }
        i += 1;
    }
    None
}

/// Lowercased tag name at `open`, plus whether it's a closing tag.
fn tag_name(s: &str, open: usize) -> Option<(String, bool, usize)> {
    let b = s.as_bytes();
    let mut i = open + 1;
    let closing = b.get(i) == Some(&b'/');
    if closing {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((s[start..i].to_ascii_lowercase(), closing, i))
}

/// Next opening tag named `name` at or after `from`. Matches `<name>` /
/// `<name ...>` but not `<names>`.
fn find_open_tag(s: &str, from: usize, name: &str) -> Option<usize> {
    let mut i = from;
    while let Some(lt) = s[i..].find('<').map(|o| i + o) {
        if let Some((n, closing, name_end)) = tag_name(s, lt) {
            if !closing && n == name {
                match s.as_bytes().get(name_end) {
                    Some(b'>') | Some(b'/') | None => return Some(lt),
                    Some(c) if c.is_ascii_whitespace() => return Some(lt),
                    _ => {}
                }
            }
        }
        i = lt + 1;
    }
    None
}

/// Matching close tag for `name` starting at `from`: tracks nesting depth and
/// skips `<!-- -->` comments. Returns (close_tag_start, close_tag_end).
fn find_matching_close(s: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let mut i = from;
    let mut depth = 0usize;
    while let Some(lt) = s[i..].find('<').map(|o| i + o) {
        if s[lt..].starts_with("<!--") {
            i = s[lt + 4..]
                .find("-->")
                .map(|o| lt + 4 + o + 3)
                .unwrap_or(s.len());
            continue;
        }
        if let Some((n, closing, _)) = tag_name(s, lt) {
            if n == name {
                let end = tag_end(s, lt).unwrap_or(s.len());
                if closing {
                    if depth == 0 {
                        return Some((lt, end));
                    }
                    depth -= 1;
                } else if !s[lt..end].trim_end_matches('>').ends_with('/') {
                    depth += 1;
                }
                i = end;
                continue;
            }
        }
        i = lt + 1;
    }
    None
}

/// Value of `name="..."` inside a tag's source text (quote-aware, case-insensitive).
fn attr(tag_src: &str, name: &str) -> Option<String> {
    let lower = tag_src.to_ascii_lowercase();
    let b = tag_src.as_bytes();
    let mut from = 0;
    while let Some(off) = lower[from..].find(name) {
        let at = from + off;
        from = at + 1;
        // Word boundary before, then optional spaces, '=', optional spaces.
        if at == 0 || b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'-' {
            continue;
        }
        let mut i = at + name.len();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        return Some(if b[i] == b'"' || b[i] == b'\'' {
            let q = b[i];
            let start = i + 1;
            let end = tag_src[start..]
                .find(q as char)
                .map(|o| start + o)
                .unwrap_or(tag_src.len());
            decode_entities(&tag_src[start..end])
        } else {
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
                i += 1;
            }
            decode_entities(&tag_src[start..i])
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("&lt;", "<"),
    ("&gt;", ">"),
    ("&quot;", "\""),
    ("&apos;", "'"),
    ("&#39;", "'"),
    ("&nbsp;", "\u{00A0}"),
    ("&mdash;", "—"),
    ("&ndash;", "–"),
    ("&hellip;", "…"),
    ("&copy;", "©"),
    ("&reg;", "®"),
    ("&trade;", "™"),
    ("&deg;", "°"),
    ("&times;", "×"),
    ("&divide;", "÷"),
    ("&plusmn;", "±"),
    ("&larr;", "←"),
    ("&rarr;", "→"),
    ("&uarr;", "↑"),
    ("&darr;", "↓"),
    ("&harr;", "↔"),
    ("&bull;", "•"),
    ("&middot;", "·"),
    ("&laquo;", "«"),
    ("&raquo;", "»"),
    ("&ldquo;", "\u{201C}"),
    ("&rdquo;", "\u{201D}"),
    ("&lsquo;", "\u{2018}"),
    ("&rsquo;", "\u{2019}"),
    ("&sect;", "§"),
    ("&para;", "¶"),
    ("&dagger;", "†"),
    ("&check;", "✓"),
    ("&cross;", "✗"),
    ("&star;", "☆"),
    ("&infin;", "∞"),
    ("&asymp;", "≈"),
    ("&ne;", "≠"),
    ("&le;", "≤"),
    ("&ge;", "≥"),
];

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    'outer: while i < bytes.len() {
        if bytes[i] != b'&' {
            let next = s[i..].find('&').map(|o| i + o).unwrap_or(s.len());
            out.push_str(&s[i..next]);
            i = next;
            continue;
        }
        // &amp; is resolved last (below) so it can't re-form another entity.
        if s[i..].starts_with("&amp;") {
            out.push_str("\u{FFFC}AMP\u{FFFC}");
            i += 5;
            continue;
        }
        for (name, repl) in NAMED_ENTITIES {
            if s[i..].starts_with(name) {
                out.push_str(repl);
                i += name.len();
                continue 'outer;
            }
        }
        if let Some(rest) = s[i..].strip_prefix("&#") {
            let (digits, radix) = match rest.strip_prefix(['x', 'X']) {
                Some(hex) => (hex, 16),
                None => (rest, 10),
            };
            let len = digits
                .bytes()
                .take_while(|b| b.is_ascii_hexdigit())
                .count()
                .min(8);
            if len > 0 && digits[len..].starts_with(';') {
                if let Ok(cp) = u32::from_str_radix(&digits[..len], radix) {
                    if let Some(ch) = char::from_u32(cp).filter(|c| !c.is_control() || *c == '\n') {
                        out.push(ch);
                        let prefix = if radix == 16 { 3 } else { 2 };
                        i += prefix + len + 1;
                        continue;
                    }
                }
            }
        }
        out.push('&');
        i += 1;
    }
    out.replace("\u{FFFC}AMP\u{FFFC}", "&")
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_matches('\n').to_string()
}

// ---------------------------------------------------------------------------
// Block scanner
// ---------------------------------------------------------------------------

fn parse_blocks(s: &str) -> Vec<Block> {
    let lines: Vec<&str> = s.lines().collect();
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if para.is_empty() {
            return;
        }
        let text = para.join(" ");
        para.clear();
        let spans = inline_spans(&text);
        if !spans.is_empty() {
            blocks.push(Block::Paragraph { spans });
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            flush(&mut para, &mut blocks);
            i += 1;
            continue;
        }
        if let Some((fence_char, fence_len, info)) = fence_open(line) {
            flush(&mut para, &mut blocks);
            let mut body = String::new();
            i += 1;
            while i < lines.len() && !fence_close(lines[i], fence_char, fence_len) {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            i += 1; // past the closing fence (or EOF)
            if body.ends_with('\n') {
                body.pop();
            }
            let lang = info.split_whitespace().next().map(|l| l.to_string());
            blocks.push(Block::Code { lang, code: body });
            continue;
        }
        if let Some((level, rest)) = heading_line(trimmed) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Heading {
                level,
                spans: inline_spans(rest),
            });
            i += 1;
            continue;
        }
        if is_divider_line(trimmed) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Divider);
            i += 1;
            continue;
        }
        if trimmed.contains('|') && i + 1 < lines.len() && is_table_divider(lines[i + 1]) {
            flush(&mut para, &mut blocks);
            let header_cells = split_table_row(trimmed);
            let width = header_cells.len();
            let header: Vec<Vec<Span>> = header_cells.iter().map(|c| inline_spans(c)).collect();
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() {
                let t = lines[i].trim();
                if t.is_empty() || !t.contains('|') {
                    break;
                }
                let mut cells = split_table_row(t);
                cells.resize(width.max(cells.len()), String::new());
                cells.truncate(width);
                rows.push(cells.iter().map(|c| inline_spans(c)).collect());
                i += 1;
            }
            blocks.push(Block::Table { header, rows });
            continue;
        }
        if trimmed.starts_with('>') {
            flush(&mut para, &mut blocks);
            let mut quote_lines: Vec<&str> = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                let Some(stripped) = t.strip_prefix('>') else {
                    break;
                };
                let stripped = stripped.strip_prefix(' ').unwrap_or(stripped);
                if stripped.is_empty() {
                    if !quote_lines.is_empty() {
                        let spans = inline_spans(&quote_lines.join(" "));
                        if !spans.is_empty() {
                            blocks.push(Block::Quote { spans });
                        }
                        quote_lines.clear();
                    }
                } else {
                    quote_lines.push(stripped);
                }
                i += 1;
            }
            if !quote_lines.is_empty() {
                let spans = inline_spans(&quote_lines.join(" "));
                if !spans.is_empty() {
                    blocks.push(Block::Quote { spans });
                }
            }
            continue;
        }
        if let Some(img) = standalone_image(trimmed) {
            flush(&mut para, &mut blocks);
            blocks.push(img);
            i += 1;
            continue;
        }
        if let Some((indent, ordered, rest)) = list_item_line(line) {
            flush(&mut para, &mut blocks);
            let spans = inline_spans(rest);
            if !spans.is_empty() {
                blocks.push(Block::ListItem {
                    indent,
                    ordered,
                    spans,
                });
            }
            i += 1;
            continue;
        }
        para.push(trimmed);
        i += 1;
    }
    flush(&mut para, &mut blocks);
    blocks
}

fn heading_line(t: &str) -> Option<(u8, &str)> {
    let level = t.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &t[level..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let rest = rest.trim().trim_end_matches('#').trim_end();
    Some((level as u8, rest))
}

fn is_divider_line(t: &str) -> bool {
    if t.len() < 3 {
        return false;
    }
    for c in ['-', '*', '_'] {
        if t.chars().all(|ch| ch == c || ch == ' ') && t.chars().filter(|&ch| ch == c).count() >= 3
        {
            return true;
        }
    }
    false
}

fn is_table_divider(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('|') || !t.contains('-') {
        return false;
    }
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    // Per-cell `:?-+:?`; a single dash is accepted (HF tables often write |-|-|).
    inner.split('|').all(|cell| {
        let c = cell.trim();
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        !c.is_empty() && c.bytes().all(|b| b == b'-')
    })
}

/// Backtick-aware pipe splitter: pipes inside a code span are literal cell text.
fn split_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut chars = t.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' => {
                // A run of backticks toggles code-span state once.
                cur.push(ch);
                while chars.peek() == Some(&'`') {
                    cur.push(chars.next().unwrap());
                }
                in_code = !in_code;
            }
            '\\' if chars.peek() == Some(&'|') => {
                cur.push(chars.next().unwrap());
            }
            '|' if !in_code => {
                cells.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(ch),
        }
    }
    cells.push(cur.trim().to_string());
    if t.starts_with('|') && cells.first().is_some_and(|c| c.is_empty()) {
        cells.remove(0);
    }
    if t.ends_with('|') && cells.last().is_some_and(|c| c.is_empty()) {
        cells.pop();
    }
    cells
}

fn standalone_image(t: &str) -> Option<Block> {
    let rest = t.strip_prefix("![")?;
    let close = find_balanced(rest, '[', ']')?;
    let alt = &rest[..close];
    let after = &rest[close + 1..];
    let paren = after.strip_prefix('(')?;
    let end = find_balanced(paren, '(', ')')?;
    if !paren[end + 1..].trim().is_empty() {
        return None;
    }
    let (src, width) = peel_width(paren[..end].trim());
    Some(Block::Image {
        src: src.to_string(),
        alt: alt.trim().to_string(),
        width,
    })
}

fn peel_width(src: &str) -> (&str, Option<u32>) {
    match src.rsplit_once("#noema-w=") {
        Some((s, w)) => (s, w.parse().ok()),
        None => (src, None),
    }
}

/// Index of the closer matching depth 0, scanning `s` which starts inside the
/// construct (openers nest).
fn find_balanced(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                return Some(idx);
            }
            depth -= 1;
        }
    }
    None
}

fn list_item_line(line: &str) -> Option<(u8, Option<u32>, &str)> {
    let mut ws = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => ws += 1,
            '\t' => ws += 4,
            _ => break,
        }
    }
    let t = line.trim_start();
    let indent = (ws / 2).min(8) as u8;
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            let rest = match rest.trim_start() {
                r if r.starts_with("[ ]") => return Some((indent, None, rest)),
                _ => rest.trim_start(),
            };
            return Some((indent, None, rest));
        }
    }
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && digits <= 9 {
        let after = &t[digits..];
        if let Some(rest) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            let n = t[..digits].parse::<u32>().ok()?;
            return Some((indent, Some(n), rest.trim_start()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Inline formatting
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    link: Option<String>,
}

impl Style {
    fn span(&self, text: String) -> Span {
        Span {
            text,
            bold: self.bold,
            italic: self.italic,
            code: false,
            strike: self.strike,
            link: self.link.clone(),
        }
    }
}

/// Parse inline Markdown (bold / italic / code / links / strikethrough) into
/// styled spans.
pub fn inline_spans(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    walk_inline(text, &Style::default(), &mut out, 0);
    coalesce(out)
}

fn walk_inline(text: &str, style: &Style, out: &mut Vec<Span>, depth: usize) {
    if depth >= MAX_NEST {
        out.push(style.span(text.to_string()));
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lit = String::new();
    let mut i = 0;
    macro_rules! flush {
        () => {
            if !lit.is_empty() {
                out.push(style.span(std::mem::take(&mut lit)));
            }
        };
    }
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() && is_md_punct(chars[i + 1]) => {
                lit.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let run = run_len(&chars, i, '`');
                match find_code_close(&chars, i + run, run) {
                    Some(j) => {
                        flush!();
                        let body: String = chars[i + run..j].iter().collect();
                        let body = trim_code_padding(&body);
                        out.push(Span {
                            text: body,
                            code: true,
                            bold: style.bold,
                            italic: style.italic,
                            strike: style.strike,
                            link: style.link.clone(),
                        });
                        i = j + run;
                    }
                    None => {
                        for _ in 0..run {
                            lit.push('`');
                        }
                        i += run;
                    }
                }
            }
            '!' if chars.get(i + 1) == Some(&'[') => {
                match parse_bracket_construct(&chars, i + 1) {
                    Some((alt, target, next)) => {
                        flush!();
                        let (src, _w) = peel_width(target.trim());
                        // Inline images are almost always badges; an empty alt
                        // carries nothing textual, so it renders as nothing.
                        if !alt.trim().is_empty() {
                            let mut st = style.clone();
                            st.link = Some(src.to_string());
                            out.push(st.span(alt.trim().to_string()));
                        }
                        i = next;
                    }
                    None => {
                        lit.push('!');
                        i += 1;
                    }
                }
            }
            '[' => match parse_bracket_construct(&chars, i) {
                Some((inner, target, next)) => {
                    flush!();
                    let mut st = style.clone();
                    st.link = Some(strip_link_title(&target));
                    walk_inline(&inner, &st, out, depth + 1);
                    i = next;
                }
                None => {
                    lit.push('[');
                    i += 1;
                }
            },
            '*' | '_' => {
                let run = run_len(&chars, i, c);
                let take = run.min(3);
                let matched = (1..=take)
                    .rev()
                    .find_map(|n| find_emphasis_close(&chars, i + n, c, n).map(|j| (n, j)));
                match matched {
                    Some((n, j)) if c != '_' || underscore_boundary(&chars, i, j, n) => {
                        flush!();
                        let inner: String = chars[i + n..j].iter().collect();
                        let mut st = style.clone();
                        match n {
                            1 => st.italic = true,
                            2 => st.bold = true,
                            _ => {
                                st.bold = true;
                                st.italic = true;
                            }
                        }
                        walk_inline(&inner, &st, out, depth + 1);
                        i = j + n;
                    }
                    _ => {
                        for _ in 0..run {
                            lit.push(c);
                        }
                        i += run;
                    }
                }
            }
            '~' if run_len(&chars, i, '~') >= 2 => match find_seq(&chars, i + 2, &['~', '~']) {
                Some(j) => {
                    flush!();
                    let inner: String = chars[i + 2..j].iter().collect();
                    let mut st = style.clone();
                    st.strike = true;
                    walk_inline(&inner, &st, out, depth + 1);
                    i = j + 2;
                }
                None => {
                    lit.push('~');
                    i += 1;
                }
            },
            '<' => {
                // Autolink: <https://…>
                let rest: String = chars[i + 1..].iter().collect();
                if rest.starts_with("http://") || rest.starts_with("https://") {
                    if let Some(gt) = rest.find('>') {
                        let url = &rest[..gt];
                        if !url.contains(char::is_whitespace) {
                            flush!();
                            let mut st = style.clone();
                            st.link = Some(url.to_string());
                            out.push(st.span(url.to_string()));
                            i += 1 + gt + 1;
                            continue;
                        }
                    }
                }
                lit.push('<');
                i += 1;
            }
            'h' if style.link.is_none() && at_word_start(&chars, i) && is_bare_url(&chars, i) => {
                let mut j = i;
                while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '<' {
                    j += 1;
                }
                let mut url: String = chars[i..j].iter().collect();
                while url.ends_with(['.', ',', ';', ':', '!', '?', ')']) {
                    url.pop();
                }
                if url.len() > "https://x".len() {
                    flush!();
                    let mut st = style.clone();
                    st.link = Some(url.clone());
                    out.push(st.span(url.clone()));
                    i += url.chars().count();
                } else {
                    lit.push('h');
                    i += 1;
                }
            }
            OBJ => i += 1, // defensive: no placeholder should survive to here
            ' ' | '\t' => {
                // Collapse whitespace runs in prose (code spans are verbatim
                // via their own branch above).
                if !lit.ends_with(' ') {
                    lit.push(' ');
                }
                i += 1;
            }
            _ => {
                lit.push(c);
                i += 1;
            }
        }
    }
    if !lit.is_empty() {
        out.push(style.span(lit));
    }
}

fn is_md_punct(c: char) -> bool {
    matches!(
        c,
        '\\' | '`'
            | '*'
            | '_'
            | '{'
            | '}'
            | '['
            | ']'
            | '('
            | ')'
            | '#'
            | '+'
            | '-'
            | '.'
            | '!'
            | '|'
            | '~'
            | '<'
            | '>'
    )
}

fn run_len(chars: &[char], i: usize, c: char) -> usize {
    chars[i..].iter().take_while(|&&ch| ch == c).count()
}

/// Closing backtick run of exactly `n` (not part of a longer run).
fn find_code_close(chars: &[char], from: usize, n: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '`' {
            let r = run_len(chars, i, '`');
            if r == n {
                return Some(i);
            }
            i += r;
        } else {
            i += 1;
        }
    }
    None
}

fn trim_code_padding(s: &str) -> String {
    // CommonMark strips one space of padding when both ends have it.
    if s.len() >= 2 && s.starts_with(' ') && s.ends_with(' ') && s.trim() != "" {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// `[text](target)` starting at `chars[i] == '['`. Returns (text, target, next_index).
fn parse_bracket_construct(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    // Window-capped: no real link outgrows this, and it bounds a hostile
    // megabyte of '[' to linear work instead of quadratic.
    const WINDOW: usize = 8192;
    let cap = (i + 1).saturating_add(WINDOW).min(chars.len());
    let rest: String = chars[i + 1..cap].iter().collect();
    let close = find_balanced(&rest, '[', ']')?;
    let close_chars = rest[..close].chars().count();
    let text = rest[..close].to_string();
    let after_bracket = i + 1 + close_chars + 1;
    if chars.get(after_bracket) != Some(&'(') {
        return None;
    }
    let pcap = (after_bracket + 1).saturating_add(WINDOW).min(chars.len());
    let paren_rest: String = chars[after_bracket + 1..pcap].iter().collect();
    let end = find_balanced(&paren_rest, '(', ')')?;
    let end_chars = paren_rest[..end].chars().count();
    let target = paren_rest[..end].to_string();
    Some((text, target, after_bracket + 1 + end_chars + 1))
}

/// Drop an optional `"title"` after the URL.
fn strip_link_title(target: &str) -> String {
    let t = target.trim();
    match t.split_once(char::is_whitespace) {
        Some((url, rest)) if rest.trim().starts_with('"') || rest.trim().starts_with('\'') => {
            url.to_string()
        }
        _ => t.to_string(),
    }
}

fn find_emphasis_close(chars: &[char], from: usize, c: char, n: usize) -> Option<usize> {
    // The opener must be immediately followed by non-space content.
    if chars.get(from).is_none_or(|ch| ch.is_whitespace()) {
        return None;
    }
    let mut i = from;
    while i < chars.len() {
        if chars[i] == c {
            let r = run_len(chars, i, c);
            if r >= n && i > from && !chars[i - 1].is_whitespace() {
                return Some(i);
            }
            i += r;
        } else {
            i += 1;
        }
    }
    None
}

/// `_emphasis_` only applies at word boundaries (snake_case stays literal).
fn underscore_boundary(chars: &[char], open: usize, close: usize, n: usize) -> bool {
    let before_ok = open == 0 || !chars[open - 1].is_alphanumeric();
    let after = close + n;
    let after_ok = after >= chars.len() || !chars[after].is_alphanumeric();
    before_ok && after_ok
}

fn find_seq(chars: &[char], from: usize, seq: &[char]) -> Option<usize> {
    if seq.is_empty() {
        return None;
    }
    let mut i = from;
    while i + seq.len() <= chars.len() {
        if chars[i..i + seq.len()] == *seq {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn at_word_start(chars: &[char], i: usize) -> bool {
    i == 0 || chars[i - 1].is_whitespace() || matches!(chars[i - 1], '(' | '[' | ',' | ':' | ';')
}

fn is_bare_url(chars: &[char], i: usize) -> bool {
    let rest: String = chars[i..].iter().take(8).collect();
    rest.starts_with("http://") || rest.starts_with("https://")
}

fn coalesce(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for s in spans {
        if s.text.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.bold == s.bold
                && last.italic == s.italic
                && last.code == s.code
                && last.strike == s.strike
                && last.link == s.link
                && !last.code
            {
                last.text.push_str(&s.text);
                continue;
            }
        }
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn frontmatter_is_split_off() {
        let doc = "---\nlicense: apache-2.0\ntags:\n  - gguf\n---\n# Hello\n";
        let r = parse(doc);
        assert_eq!(
            r.frontmatter.as_deref(),
            Some("license: apache-2.0\ntags:\n  - gguf")
        );
        assert_eq!(
            r.blocks,
            vec![Block::Heading {
                level: 1,
                spans: vec![Span::plain("Hello")]
            }]
        );
    }

    #[test]
    fn no_frontmatter_when_unclosed() {
        let doc = "---\nnot frontmatter";
        let r = parse(doc);
        assert!(r.frontmatter.is_none());
    }

    #[test]
    fn fenced_code_is_verbatim() {
        let doc = "```python\nx = \"<div>&lt;\" | y\n```\n";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Code {
                lang: Some("python".into()),
                code: "x = \"<div>&lt;\" | y".into()
            }]
        );
    }

    #[test]
    fn unclosed_fence_swallows_rest() {
        let doc = "```\ncode line\nstill code";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Code {
                lang: None,
                code: "code line\nstill code".into()
            }]
        );
    }

    #[test]
    fn html_pre_code_becomes_fenced_block() {
        let doc = "<pre><code class=\"language-bash\">pip install &lt;pkg&gt;\n</code></pre>";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Code {
                lang: Some("bash".into()),
                code: "pip install <pkg>".into()
            }]
        );
    }

    #[test]
    fn inline_html_code_survives_markup_and_pipes() {
        let doc = "Use <code>&lt;think&gt;</code> and <code>a | b</code> here.";
        let r = parse(doc);
        let Block::Paragraph { spans } = &r.blocks[0] else {
            panic!("expected paragraph, got {:?}", r.blocks);
        };
        let codes: Vec<&Span> = spans.iter().filter(|s| s.code).collect();
        assert_eq!(codes.len(), 2);
        assert_eq!(codes[0].text, "<think>");
        assert_eq!(codes[1].text, "a | b");
    }

    #[test]
    fn markdown_inline_code_protected_from_tag_strip() {
        let doc = "The `<div>` tag and `a | b` pipe.";
        let r = parse(doc);
        let Block::Paragraph { spans } = &r.blocks[0] else {
            panic!("expected paragraph");
        };
        let codes: Vec<&Span> = spans.iter().filter(|s| s.code).collect();
        assert_eq!(codes[0].text, "<div>");
        assert_eq!(codes[1].text, "a | b");
    }

    #[test]
    fn html_table_to_table_block() {
        let doc = r#"<table>
  <tr><th>Model</th><th>Score</th></tr>
  <tr><td><b>Ours</b></td><td>98.5</td></tr>
  <tr><td>Baseline</td><td>90 | 91</td></tr>
</table>"#;
        let r = parse(doc);
        let Block::Table { header, rows } = &r.blocks[0] else {
            panic!("expected table, got {:?}", r.blocks);
        };
        assert_eq!(spans_text(&header[0]), "Model");
        assert_eq!(spans_text(&header[1]), "Score");
        assert_eq!(rows.len(), 2);
        assert!(rows[0][0][0].bold);
        assert_eq!(spans_text(&rows[0][0]), "Ours");
        // Literal pipe in a cell became the lookalike, not a column split.
        assert_eq!(spans_text(&rows[1][1]), "90 \u{00A6} 91");
    }

    #[test]
    fn html_table_colspan_pads_columns() {
        let doc = r#"<table>
<tr><th colspan="3">Benchmarks</th></tr>
<tr><td>A</td><td>B</td><td>C</td></tr>
<tr><td>1</td><td>2</td><td>3</td></tr>
</table>"#;
        let r = parse(doc);
        let Block::Table { header, rows } = &r.blocks[0] else {
            panic!("expected table");
        };
        assert_eq!(header.len(), 3);
        assert_eq!(spans_text(&header[0]), "Benchmarks");
        assert!(header[1].is_empty() || spans_text(&header[1]).trim().is_empty());
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn tiny_html_table_falls_through_to_text() {
        let doc = "<table><tr><td>just one cell</td></tr></table>";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Paragraph {
                spans: vec![Span::plain("just one cell")]
            }]
        );
    }

    #[test]
    fn markdown_table_single_dash_divider() {
        let doc = "| A | B |\n|-|-|\n| 1 | 2 |\n";
        let r = parse(doc);
        let Block::Table { header, rows } = &r.blocks[0] else {
            panic!("expected table, got {:?}", r.blocks);
        };
        assert_eq!(header.len(), 2);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn table_row_pipes_in_code_are_literal() {
        let doc = "| Cmd | Desc |\n| --- | --- |\n| `a \\| b` | pipe |\n";
        let r = parse(doc);
        let Block::Table { rows, .. } = &r.blocks[0] else {
            panic!("expected table");
        };
        assert_eq!(rows[0].len(), 2);
        assert!(rows[0][0][0].code);
    }

    #[test]
    fn centered_div_with_badges() {
        let doc = r#"<div align="center">
  <img src="https://example.com/logo.png" alt="Logo" width="400"/>
  <br>
  <a href="https://example.com"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://arxiv.org/abs/1234"><b>Paper</b></a>
</div>"#;
        let r = parse(doc);
        assert!(matches!(
            &r.blocks[0],
            Block::Image { src, alt, width: Some(400) }
                if src == "https://example.com/logo.png" && alt == "Logo"
        ));
        let all: String = r
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { spans } => Some(spans_text(spans)),
                _ => None,
            })
            .collect();
        assert!(all.contains("License: MIT"));
        assert!(all.contains("Paper"));
    }

    #[test]
    fn heading_tags_and_hr_and_entities() {
        let doc = "<h2>Model &amp;lt; Card</h2><hr><p>a &le; b &amp; c</p>";
        let r = parse(doc);
        assert_eq!(
            r.blocks[0],
            Block::Heading {
                level: 2,
                // &amp;lt; decodes to the literal text "&lt;" — amp resolves last.
                spans: vec![Span::plain("Model &lt; Card")]
            }
        );
        assert_eq!(r.blocks[1], Block::Divider);
        assert_eq!(
            r.blocks[2],
            Block::Paragraph {
                spans: vec![Span::plain("a ≤ b & c")]
            }
        );
    }

    #[test]
    fn html_comments_stripped() {
        let doc = "before <!-- hidden <table><tr> --> after";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Paragraph {
                spans: vec![Span::plain("before after")]
            }]
        );
    }

    #[test]
    fn nested_divs_do_not_confuse_stripper() {
        let doc = "<div><div style=\"display:flex\"><p>inner</p></div>tail</div>";
        let r = parse(doc);
        let text: String = r
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { spans } => Some(spans_text(spans)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("inner"));
        assert!(text.contains("tail"));
    }

    #[test]
    fn unknown_tags_stay_verbatim() {
        let doc = "Wrap output in <answer> tags.";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Paragraph {
                spans: vec![Span::plain("Wrap output in <answer> tags.")]
            }]
        );
    }

    #[test]
    fn inline_styles() {
        let spans = inline_spans("**bold** and *it* and ~~gone~~ and `code`");
        assert!(spans.iter().any(|s| s.bold && s.text == "bold"));
        assert!(spans.iter().any(|s| s.italic && s.text == "it"));
        assert!(spans.iter().any(|s| s.strike && s.text == "gone"));
        assert!(spans.iter().any(|s| s.code && s.text == "code"));
    }

    #[test]
    fn bold_italic_nesting() {
        let spans = inline_spans("***both*** and **outer *inner* end**");
        assert!(spans.iter().any(|s| s.bold && s.italic && s.text == "both"));
        assert!(spans
            .iter()
            .any(|s| s.bold && !s.italic && s.text == "outer "));
        assert!(spans
            .iter()
            .any(|s| s.bold && s.italic && s.text == "inner"));
    }

    #[test]
    fn snake_case_not_italicized() {
        let spans = inline_spans("use model_name_here for config");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "use model_name_here for config");
    }

    #[test]
    fn links_and_bare_urls() {
        let spans =
            inline_spans("See [the docs](https://example.com/a_(b)) or https://foo.bar/baz.");
        assert!(spans.iter().any(
            |s| s.link.as_deref() == Some("https://example.com/a_(b)") && s.text == "the docs"
        ));
        assert!(spans
            .iter()
            .any(|s| s.link.as_deref() == Some("https://foo.bar/baz")));
    }

    #[test]
    fn link_title_is_dropped() {
        let spans = inline_spans("[x](https://e.com \"tooltip\")");
        assert_eq!(spans[0].link.as_deref(), Some("https://e.com"));
    }

    #[test]
    fn badge_image_with_empty_alt_disappears() {
        let spans = inline_spans("![](https://img.shields.io/badge/x.svg) tail");
        assert_eq!(spans_text(&spans).trim(), "tail");
    }

    #[test]
    fn image_in_link_keeps_alt_text() {
        let spans = inline_spans("[![Alt](https://x/img.png)](https://target)");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "Alt");
        assert!(spans[0].link.is_some());
    }

    #[test]
    fn standalone_image_block_with_width() {
        let doc = "<img src=\"https://x/hero.png\" width=\"600px\" alt=\"Hero\">";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Image {
                src: "https://x/hero.png".into(),
                alt: "Hero".into(),
                width: Some(600),
            }]
        );
    }

    #[test]
    fn lists_ordered_unordered_nested() {
        let doc = "- top\n  - nested\n1. first\n2) second\n";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![
                Block::ListItem {
                    indent: 0,
                    ordered: None,
                    spans: vec![Span::plain("top")]
                },
                Block::ListItem {
                    indent: 1,
                    ordered: None,
                    spans: vec![Span::plain("nested")]
                },
                Block::ListItem {
                    indent: 0,
                    ordered: Some(1),
                    spans: vec![Span::plain("first")]
                },
                Block::ListItem {
                    indent: 0,
                    ordered: Some(2),
                    spans: vec![Span::plain("second")]
                },
            ]
        );
    }

    #[test]
    fn html_list_items() {
        let doc = "<ul><li>alpha</li><li><b>beta</b></li></ul>";
        let r = parse(doc);
        assert_eq!(r.blocks.len(), 2);
        assert!(
            matches!(&r.blocks[0], Block::ListItem { spans, .. } if spans_text(spans) == "alpha")
        );
        assert!(matches!(&r.blocks[1], Block::ListItem { spans, .. } if spans[0].bold));
    }

    #[test]
    fn blockquote() {
        let doc = "> quoted text\n> more\n";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Quote {
                spans: vec![Span::plain("quoted text more")]
            }]
        );
    }

    #[test]
    fn paragraph_soft_wrap_joins_lines() {
        let doc = "line one\nline two\n\nnext para\n";
        let r = parse(doc);
        assert_eq!(r.blocks.len(), 2);
        assert!(
            matches!(&r.blocks[0], Block::Paragraph { spans } if spans_text(spans) == "line one line two")
        );
    }

    #[test]
    fn details_summary_flattened() {
        let doc = "<details><summary>Click to expand</summary>\nhidden body\n</details>";
        let r = parse(doc);
        let text: String = r
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { spans } => Some(spans_text(spans)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Click to expand"));
        assert!(text.contains("hidden body"));
    }

    #[test]
    fn script_and_style_content_dropped() {
        let doc = "before<style>.x{color:red}</style><script>alert(1)</script>after";
        let r = parse(doc);
        assert_eq!(
            r.blocks,
            vec![Block::Paragraph {
                spans: vec![Span::plain("beforeafter")]
            }]
        );
    }

    #[test]
    fn code_placeholder_inside_table_cell() {
        let doc = "<table><tr><th>Cmd</th><th>What</th></tr><tr><td><code>a | b</code></td><td>pipes</td></tr></table>";
        let r = parse(doc);
        let Block::Table { rows, .. } = &r.blocks[0] else {
            panic!("expected table, got {:?}", r.blocks);
        };
        assert!(rows[0][0][0].code);
        assert_eq!(rows[0][0][0].text, "a | b");
        assert_eq!(spans_text(&rows[0][1]), "pipes");
    }

    #[test]
    fn block_json_shape_is_stable() {
        let blocks = parse("# Hi\n\nsome **bold**\n").blocks;
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("\"kind\":\"heading\""));
        assert!(json.contains("\"kind\":\"paragraph\""));
        let round: Vec<Block> = serde_json::from_str(&json).unwrap();
        assert_eq!(round, blocks);
    }

    #[test]
    fn realistic_model_card() {
        let doc = r#"---
license: apache-2.0
---
<div align="center">
<h1>SuperModel-7B</h1>
<img src="https://x/logo.png" width="300" alt="logo">
<br>
<a href="https://arxiv.org/abs/1"><img src="https://img.shields.io/badge/arXiv-1-red" alt="arXiv"></a>
</div>

## Highlights

- **Fast**: 2x faster than *baseline*
- Supports `<think>` tags

## Benchmarks

<table>
<tr><th colspan="2">Category</th><th>Score</th></tr>
<tr><td>MMLU</td><td>5-shot</td><td>78.5</td></tr>
<tr><td>GSM8K</td><td>8-shot</td><td>91.2</td></tr>
</table>

## Quickstart

```bash
pip install supermodel
```

> Note: needs 16 GB RAM.
"#;
        let r = parse(doc);
        assert!(r.frontmatter.is_some());
        assert!(r.blocks.iter().any(|b| matches!(b, Block::Heading { level: 1, spans } if spans_text(spans) == "SuperModel-7B")));
        assert!(r.blocks.iter().any(|b| matches!(
            b,
            Block::Image {
                width: Some(300),
                ..
            }
        )));
        assert!(r.blocks.iter().any(
            |b| matches!(b, Block::Table { header, rows } if header.len() == 3 && rows.len() == 2)
        ));
        assert!(r.blocks.iter().any(
            |b| matches!(b, Block::Code { lang, code } if lang.as_deref() == Some("bash") && code == "pip install supermodel")
        ));
        assert!(r.blocks.iter().any(|b| matches!(b, Block::Quote { .. })));
        assert!(r.blocks.iter().any(|b| matches!(b, Block::ListItem { spans, .. } if spans.iter().any(|s| s.code && s.text == "<think>"))));
    }
}

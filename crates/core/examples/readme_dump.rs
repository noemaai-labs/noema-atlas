//! Dev harness: parse a model card and check renderer-facing invariants.
//! `cargo run -p noema-core --example readme_dump -- <file.md>...`

use noema_core::readme::{Block, Span};

fn spans_text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn check_spans(path: &str, ctx: &str, spans: &[Span], problems: &mut Vec<String>) {
    let text = spans_text(spans);
    if text.contains('\u{FFFC}') {
        problems.push(format!("{path}: leftover placeholder in {ctx}: {text:?}"));
    }
    for tag in [
        "<div", "</div", "<p>", "</p>", "<table", "<tr", "<td", "<br", "<img", "<a href", "<b>",
        "</b>", "<h1", "<h2", "<center", "<sup", "</sup", "&lt;", "&amp;", "&nbsp;", "&#",
    ] {
        for s in spans {
            if !s.code && s.text.contains(tag) {
                problems.push(format!(
                    "{path}: raw {tag:?} outside code in {ctx}: {:?}",
                    s.text.chars().take(120).collect::<String>()
                ));
            }
        }
    }
}

fn main() {
    let mut problems = Vec::new();
    for path in std::env::args().skip(1) {
        let raw = std::fs::read_to_string(&path).expect("read fixture");
        let doc = noema_core::readme::parse(&raw);
        let mut counts = std::collections::BTreeMap::new();
        for (i, b) in doc.blocks.iter().enumerate() {
            let ctx = format!("block {i}");
            match b {
                Block::Heading { spans, .. } => {
                    *counts.entry("heading").or_insert(0) += 1;
                    check_spans(&path, &ctx, spans, &mut problems);
                }
                Block::Paragraph { spans } => {
                    *counts.entry("paragraph").or_insert(0) += 1;
                    check_spans(&path, &ctx, spans, &mut problems);
                }
                Block::Code { code, .. } => {
                    *counts.entry("code").or_insert(0) += 1;
                    if code.contains('\u{FFFC}') {
                        problems.push(format!("{path}: placeholder inside code block {i}"));
                    }
                }
                Block::ListItem { spans, .. } => {
                    *counts.entry("list_item").or_insert(0) += 1;
                    check_spans(&path, &ctx, spans, &mut problems);
                }
                Block::Quote { spans } => {
                    *counts.entry("quote").or_insert(0) += 1;
                    check_spans(&path, &ctx, spans, &mut problems);
                }
                Block::Table { header, rows } => {
                    *counts.entry("table").or_insert(0) += 1;
                    let w = header.len();
                    if w < 2 {
                        problems.push(format!("{path}: table {i} has {w} column(s)"));
                    }
                    for (r, row) in rows.iter().enumerate() {
                        if row.len() != w {
                            problems.push(format!(
                                "{path}: table {i} row {r} width {} != header {w}",
                                row.len()
                            ));
                        }
                        for cell in row {
                            check_spans(&path, &format!("table {i} row {r}"), cell, &mut problems);
                        }
                    }
                    for cell in header {
                        check_spans(&path, &format!("table {i} header"), cell, &mut problems);
                    }
                }
                Block::Image { src, .. } => {
                    *counts.entry("image").or_insert(0) += 1;
                    if src.contains("#noema-w=") {
                        problems.push(format!("{path}: unpeeled width marker in image {i}"));
                    }
                }
                Block::Divider => *counts.entry("divider").or_insert(0) += 1,
            }
        }
        println!(
            "{path}: frontmatter={} blocks={} {:?}",
            doc.frontmatter.is_some(),
            doc.blocks.len(),
            counts
        );
        if std::env::var("NOEMA_DUMP_TABLES").is_ok() {
            for b in &doc.blocks {
                if let Block::Table { header, rows } = b {
                    let fmt = |cells: &[Vec<Span>]| {
                        cells
                            .iter()
                            .map(|c| spans_text(c))
                            .collect::<Vec<_>>()
                            .join(" | ")
                    };
                    println!("  TABLE: {}", fmt(header));
                    for row in rows.iter().take(2) {
                        println!("       | {}", fmt(row));
                    }
                }
            }
        }
    }
    if problems.is_empty() {
        println!("\nAll invariants hold.");
    } else {
        println!("\n{} problem(s):", problems.len());
        for p in &problems {
            println!("  - {p}");
        }
        std::process::exit(1);
    }
}

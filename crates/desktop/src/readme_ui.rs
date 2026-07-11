//! Native egui renderer for parsed model cards (`noema_core::readme`): each
//! typed block gets a purpose-built widget — no webview, no HTML.

use eframe::egui;
use noema_core::hf::HfModelDetail;
use noema_core::readme::{Block, Span};

use crate::{pal_of, Action, App, Palette, ReadmeState};

/// Blocks past this render as a "read the rest on Hugging Face" link; keeps a
/// pathological card from freezing the frame.
const MAX_BLOCKS: usize = 600;

/// The collapsible "Model card" section in the Discover detail view. Fetches
/// lazily: the README is only requested the first time the header is expanded.
pub fn draw_readme_section(
    ui: &mut egui::Ui,
    app: &App,
    detail: &HfModelDetail,
    actions: &mut Vec<Action>,
) {
    let pal = pal_of(ui);
    let key = crate::readme_key(&detail.id, &detail.revision);
    egui::CollapsingHeader::new(egui::RichText::new("Model card").strong())
        .id_source(("readme", &key))
        .default_open(false)
        .show(ui, |ui| {
            ui.add_space(2.0);
            match &app.readme {
                ReadmeState::Loading { key: k } if *k == key => {
                    loading_row(ui);
                }
                ReadmeState::Ready { key: k, doc } if *k == key => {
                    blocks_ui(ui, &pal, &doc.blocks, &detail.id, actions);
                }
                ReadmeState::Missing { key: k } if *k == key => {
                    ui.label(
                        egui::RichText::new("This model has no README.")
                            .small()
                            .weak(),
                    );
                }
                ReadmeState::Failed { key: k, error } if *k == key => {
                    ui.label(egui::RichText::new(error).small().color(pal.red));
                    if ui.small_button("Try again").clicked() {
                        actions.push(Action::FetchReadme {
                            id: detail.id.clone(),
                            revision: detail.revision.clone(),
                        });
                    }
                }
                _ => {
                    // First expand for this model: kick off the fetch. apply()
                    // flips the state to Loading, so this pushes at most twice.
                    actions.push(Action::FetchReadme {
                        id: detail.id.clone(),
                        revision: detail.revision.clone(),
                    });
                    loading_row(ui);
                }
            }
        });
}

fn loading_row(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(egui::RichText::new("Loading model card…").small().weak());
    });
}

fn blocks_ui(
    ui: &mut egui::Ui,
    pal: &Palette,
    blocks: &[Block],
    repo_id: &str,
    actions: &mut Vec<Action>,
) {
    ui.spacing_mut().item_spacing.y = 6.0;
    for (i, block) in blocks.iter().take(MAX_BLOCKS).enumerate() {
        block_ui(ui, pal, i, block, actions);
    }
    if blocks.len() > MAX_BLOCKS {
        ui.add_space(4.0);
        ui.hyperlink_to(
            egui::RichText::new("This card is very long — read the rest on Hugging Face").small(),
            format!("https://huggingface.co/{repo_id}"),
        );
    }
}

fn block_ui(
    ui: &mut egui::Ui,
    pal: &Palette,
    idx: usize,
    block: &Block,
    actions: &mut Vec<Action>,
) {
    match block {
        Block::Heading { level, spans } => {
            ui.add_space(match level {
                1 => 8.0,
                2 => 6.0,
                _ => 4.0,
            });
            let size = match level {
                1 => 19.0,
                2 => 16.5,
                3 => 15.0,
                _ => 13.5,
            };
            inline_text(ui, pal, spans, Some(size), true);
            if *level <= 2 {
                ui.separator();
            }
        }
        Block::Paragraph { spans } => {
            inline_text(ui, pal, spans, None, false);
        }
        Block::Code { lang, code } => {
            egui::Frame::none()
                .fill(ui.visuals().extreme_bg_color)
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(lang.as_deref().unwrap_or(""))
                                .small()
                                .weak(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Copy").clicked() {
                                actions.push(Action::CopyText {
                                    text: code.clone(),
                                    what: "Code".into(),
                                });
                            }
                        });
                    });
                    egui::ScrollArea::horizontal()
                        .id_source(("readme_code", idx))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(code).monospace().size(12.5));
                        });
                });
        }
        Block::ListItem {
            indent,
            ordered,
            spans,
        } => {
            ui.horizontal(|ui| {
                ui.add_space(4.0 + f32::from(*indent) * 16.0);
                let marker = match ordered {
                    Some(n) => format!("{n}."),
                    None => "•".to_string(),
                };
                ui.label(egui::RichText::new(marker).color(pal.muted));
                ui.vertical(|ui| inline_text(ui, pal, spans, None, false));
            });
        }
        Block::Quote { spans } => {
            let resp = ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.vertical(|ui| inline_text(ui, pal, spans, None, false));
            });
            let r = resp.response.rect;
            ui.painter().rect_filled(
                egui::Rect::from_min_max(r.left_top(), egui::pos2(r.left() + 3.0, r.bottom())),
                1.5,
                pal.faint,
            );
        }
        Block::Table { header, rows } => {
            egui::ScrollArea::horizontal()
                .id_source(("readme_table", idx))
                .show(ui, |ui| {
                    egui::Grid::new(("readme_grid", idx))
                        .striped(true)
                        .spacing([16.0, 5.0])
                        .min_col_width(36.0)
                        .max_col_width(280.0)
                        .show(ui, |ui| {
                            for cell in header {
                                inline_text(ui, pal, cell, None, true);
                            }
                            ui.end_row();
                            for row in rows {
                                for cell in row {
                                    inline_text(ui, pal, cell, None, false);
                                }
                                ui.end_row();
                            }
                        });
                });
        }
        Block::Image { src, alt, .. } => {
            // No remote image loading (no webview, no surprise network fetches).
            if src.starts_with("http://") || src.starts_with("https://") {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Image ·").small().weak());
                    let text = if alt.is_empty() { "view" } else { alt.as_str() };
                    ui.hyperlink_to(egui::RichText::new(text).small(), src);
                });
            } else if !alt.is_empty() {
                ui.label(egui::RichText::new(alt).small().weak());
            }
        }
        Block::Divider => {
            ui.separator();
        }
    }
}

/// One wrapped run of styled inline spans.
fn inline_text(
    ui: &mut egui::Ui,
    pal: &Palette,
    spans: &[Span],
    size: Option<f32>,
    strong_all: bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in spans {
            span_widget(ui, pal, span, size, strong_all);
        }
    });
}

fn span_widget(ui: &mut egui::Ui, pal: &Palette, span: &Span, size: Option<f32>, strong_all: bool) {
    let mut rt = egui::RichText::new(&span.text);
    if let Some(s) = size {
        rt = rt.size(s);
    }
    if span.bold || strong_all {
        rt = rt.strong();
    }
    if span.italic {
        rt = rt.italics();
    }
    if span.strike {
        rt = rt.strikethrough();
    }
    if span.code {
        rt = rt.monospace().background_color(ui.visuals().code_bg_color);
        if size.is_none() {
            rt = rt.size(12.5);
        }
    }
    match &span.link {
        Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
            ui.hyperlink_to(rt, url);
        }
        _ => {
            let _ = pal;
            ui.label(rt);
        }
    }
}

use eframe::egui;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TransportKind {
    Iroh,
    Https,
    HuggingFace,
    BitTorrent,
    File,
    Unknown,
}

impl TransportKind {
    pub fn from_source_id(source_id: &str) -> Self {
        kind_from_source_id(source_id)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Iroh => "Iroh",
            Self::Https => "HTTPS",
            Self::HuggingFace => "Hugging Face",
            Self::BitTorrent => "BitTorrent",
            Self::File => "Local file",
            Self::Unknown => "Source",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Iroh => "Iroh",
            Self::Https => "HTTPS",
            Self::HuggingFace => "HF",
            Self::BitTorrent => "BT",
            Self::File => "File",
            Self::Unknown => "SRC",
        }
    }

    pub fn color(self) -> egui::Color32 {
        match self {
            Self::Iroh => egui::Color32::from_rgb(0x6c, 0x9c, 0xff),
            Self::Https => egui::Color32::from_rgb(0x5d, 0xb0, 0xff),
            Self::HuggingFace => egui::Color32::from_rgb(0xff, 0xb3, 0x47),
            // Deliberately NOT green: green is the app-wide success/upload
            // color, and a protocol brand must not read as a status.
            Self::BitTorrent => egui::Color32::from_rgb(0xd8, 0x7a, 0xde),
            Self::File => egui::Color32::from_rgb(0x9a, 0x9a, 0x9a),
            Self::Unknown => egui::Color32::from_rgb(0x8a, 0x8a, 0x8a),
        }
    }

    /// One-line lay explanation, used as hover text on every pill.
    pub fn description(self) -> &'static str {
        match self {
            Self::Iroh => {
                "Iroh — Noema's worldwide peer network. Verified pieces are striped from many peers at once."
            }
            Self::Https => "A direct mirror download, verified against the same hash.",
            Self::HuggingFace => {
                "Hugging Face — the original host. Used as a fallback, verified against the same hash."
            }
            Self::BitTorrent => {
                "BitTorrent — the public torrent network. Extra seeders beyond Noema users."
            }
            Self::File => "A file already on this machine.",
            Self::Unknown => "An additional download source.",
        }
    }

    /// The brand color, darkened on light backgrounds to preserve contrast.
    pub fn color_on(self, dark: bool) -> egui::Color32 {
        let c = self.color();
        if dark {
            c
        } else {
            c.gamma_multiply(0.62)
        }
    }
}

pub fn kind_from_source_id(source_id: &str) -> TransportKind {
    let s = source_id.trim().to_ascii_lowercase();
    if s.starts_with("iroh:") {
        TransportKind::Iroh
    } else if s.starts_with("https:") {
        TransportKind::Https
    } else if s.starts_with("hf:") {
        TransportKind::HuggingFace
    } else if s.starts_with("btv2:") || s.starts_with("bittorrent") || s.starts_with("magnet:") {
        // The real BitTorrent source id is `btv2:<magnet-uri>` (Source::source_id);
        // without the `btv2:` arm it fell through to Unknown and leaked the raw magnet.
        TransportKind::BitTorrent
    } else if s.starts_with("file:") {
        TransportKind::File
    } else {
        TransportKind::Unknown
    }
}

pub fn transport_badge(
    ui: &mut egui::Ui,
    text_or_source: impl AsRef<str>,
    kind: impl Into<Option<TransportKind>>,
) -> egui::Response {
    transport_pill(
        ui,
        text_or_source.as_ref(),
        kind.into(),
        false,
        PillSize::Badge,
    )
}

pub fn transport_chip(
    ui: &mut egui::Ui,
    label_or_source: impl AsRef<str>,
    kind: impl Into<Option<TransportKind>>,
    muted: bool,
) -> egui::Response {
    transport_pill(
        ui,
        label_or_source.as_ref(),
        kind.into(),
        muted,
        PillSize::Chip,
    )
}

pub fn paint_transport_glyph(
    painter: &egui::Painter,
    rect: egui::Rect,
    kind: TransportKind,
    color: egui::Color32,
) {
    let stroke = egui::Stroke::new((rect.width() / 9.0).clamp(1.2, 2.0), color);
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.42;

    match kind {
        TransportKind::Iroh => {
            let a = center + egui::vec2(-radius * 0.70, -radius * 0.25);
            let b = center + egui::vec2(radius * 0.70, -radius * 0.25);
            let c = center + egui::vec2(0.0, radius * 0.66);
            painter.line_segment([a, b], stroke);
            painter.line_segment([b, c], stroke);
            painter.line_segment([c, a], stroke);
            painter.circle_stroke(center, radius * 0.30, stroke);
        }
        TransportKind::Https => {
            let body = egui::Rect::from_center_size(
                center + egui::vec2(0.0, radius * 0.22),
                egui::vec2(radius * 1.35, radius * 0.90),
            );
            let shackle_center = center + egui::vec2(0.0, -radius * 0.15);
            painter.rect_stroke(body, 2.0, stroke);
            painter.circle_stroke(shackle_center, radius * 0.43, stroke);
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(body.left() - stroke.width, body.top() - stroke.width),
                    egui::pos2(body.right() + stroke.width, center.y),
                ),
                0.0,
                ui_bg_fill(painter),
            );
            painter.rect_stroke(body, 2.0, stroke);
        }
        TransportKind::HuggingFace => {
            painter.circle_stroke(center, radius * 0.78, stroke);
            let eye_y = center.y - radius * 0.12;
            painter.circle_filled(
                center + egui::vec2(-radius * 0.30, -radius * 0.14),
                radius * 0.08,
                color,
            );
            painter.circle_filled(
                center + egui::vec2(radius * 0.30, -radius * 0.14),
                radius * 0.08,
                color,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - radius * 0.34, eye_y + radius * 0.42),
                    egui::pos2(center.x + radius * 0.34, eye_y + radius * 0.42),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-radius * 0.76, radius * 0.08),
                    center + egui::vec2(-radius * 0.98, -radius * 0.22),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(radius * 0.76, radius * 0.08),
                    center + egui::vec2(radius * 0.98, -radius * 0.22),
                ],
                stroke,
            );
        }
        TransportKind::BitTorrent => {
            // A swarm glyph: a central node with peers around it feeding in.
            painter.circle_stroke(center, radius * 0.30, stroke);
            for (dx, dy) in [
                (-0.78_f32, -0.62_f32),
                (0.78, -0.62),
                (-0.78, 0.62),
                (0.78, 0.62),
            ] {
                let node = center + egui::vec2(radius * dx, radius * dy);
                painter.circle_filled(node, radius * 0.16, color);
                let dir = (center - node).normalized();
                painter.line_segment(
                    [node + dir * radius * 0.20, center - dir * radius * 0.34],
                    stroke,
                );
            }
        }
        TransportKind::File => {
            let page =
                egui::Rect::from_center_size(center, egui::vec2(radius * 1.16, radius * 1.50));
            let fold = radius * 0.38;
            let pts = vec![
                page.left_top(),
                egui::pos2(page.right() - fold, page.top()),
                page.right_top() + egui::vec2(0.0, fold),
                page.right_bottom(),
                page.left_bottom(),
            ];
            painter.add(egui::Shape::closed_line(pts, stroke));
            painter.line_segment(
                [
                    egui::pos2(page.right() - fold, page.top()),
                    egui::pos2(page.right() - fold, page.top() + fold),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(page.right() - fold, page.top() + fold),
                    egui::pos2(page.right(), page.top() + fold),
                ],
                stroke,
            );
        }
        TransportKind::Unknown => {
            painter.circle_stroke(center, radius * 0.76, stroke);
            painter.line_segment(
                [
                    center + egui::vec2(-radius * 0.28, -radius * 0.18),
                    center + egui::vec2(0.0, -radius * 0.42),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(0.0, -radius * 0.42),
                    center + egui::vec2(radius * 0.28, -radius * 0.18),
                ],
                stroke,
            );
            painter.line_segment([center, center + egui::vec2(0.0, radius * 0.32)], stroke);
            painter.circle_filled(
                center + egui::vec2(0.0, radius * 0.56),
                radius * 0.08,
                color,
            );
        }
    }
}

#[derive(Clone, Copy)]
enum PillSize {
    Badge,
    Chip,
}

fn transport_pill(
    ui: &mut egui::Ui,
    text_or_source: &str,
    kind: Option<TransportKind>,
    muted: bool,
    size: PillSize,
) -> egui::Response {
    let kind = kind.unwrap_or_else(|| kind_from_source_id(text_or_source));
    let text = label_text(text_or_source, kind);
    let dark = ui.visuals().dark_mode;
    let base = kind.color_on(dark);
    let color = if muted { subdued(base) } else { base };
    let text_color = if muted {
        ui.visuals().weak_text_color()
    } else {
        color
    };

    let font_id = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text, font_id, text_color);
    let (height, icon_size, x_pad, gap) = match size {
        PillSize::Badge => (20.0, 12.0, 6.0, 4.0),
        PillSize::Chip => (26.0, 15.0, 8.0, 5.0),
    };
    let desired = egui::vec2(x_pad * 2.0 + icon_size + gap + galley.size().x, height);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let fill = if muted {
            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 18)
        } else if response.hovered() {
            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 34)
        } else {
            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 22)
        };
        let stroke = egui::Stroke::new(1.0, color);
        ui.painter().rect(rect, height * 0.5, fill, stroke);

        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + x_pad, rect.center().y - icon_size * 0.5),
            egui::vec2(icon_size, icon_size),
        );
        paint_transport_glyph(ui.painter(), icon_rect, kind, color);

        let text_pos = egui::pos2(
            icon_rect.right() + gap,
            rect.center().y - galley.size().y * 0.5,
        );
        ui.painter().galley(text_pos, galley, visuals.text_color());
    }

    response.on_hover_text(kind.description())
}

fn label_text(text_or_source: &str, kind: TransportKind) -> String {
    let text = text_or_source.trim();
    if text.is_empty() || kind_from_source_id(text) != TransportKind::Unknown {
        kind.short_label().to_owned()
    } else {
        text.to_owned()
    }
}

fn subdued(color: egui::Color32) -> egui::Color32 {
    egui::Color32::from_rgb(
        ((color.r() as u16 + 0x8a) / 2) as u8,
        ((color.g() as u16 + 0x8a) / 2) as u8,
        ((color.b() as u16 + 0x8a) / 2) as u8,
    )
}

fn ui_bg_fill(painter: &egui::Painter) -> egui::Color32 {
    painter.ctx().style().visuals.extreme_bg_color
}

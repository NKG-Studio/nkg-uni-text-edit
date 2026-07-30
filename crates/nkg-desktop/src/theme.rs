use eframe::egui::{self, Color32, FontFamily, FontId, TextStyle, Visuals};

pub const BACKGROUND: Color32 = Color32::from_rgb(30, 30, 30);
pub const PANEL: Color32 = Color32::from_rgb(37, 37, 38);
pub const SIDEBAR: Color32 = Color32::from_rgb(24, 24, 24);
pub const BORDER: Color32 = Color32::from_rgb(55, 55, 55);
pub const TEXT: Color32 = Color32::from_rgb(212, 212, 212);
pub const MUTED: Color32 = Color32::from_rgb(142, 142, 142);
pub const ACCENT: Color32 = Color32::from_rgb(0, 122, 204);
pub const SELECTION: Color32 = Color32::from_rgb(9, 71, 113);
pub const TEXT_ON_SELECTION: Color32 = Color32::WHITE;
pub const MUTED_ON_SELECTION: Color32 = Color32::from_rgb(214, 233, 248);
pub const HIGHLIGHT: Color32 = Color32::from_rgb(97, 74, 15);
pub const SELECTED_LINE: Color32 = Color32::from_rgb(18, 59, 93);
pub const STATUS: Color32 = SELECTION;
pub const DIFF_REPLACE: Color32 = Color32::from_rgb(63, 57, 25);
pub const DIFF_DELETE: Color32 = Color32::from_rgb(64, 35, 35);
pub const DIFF_INSERT: Color32 = Color32::from_rgb(33, 58, 38);

pub fn configure(context: &egui::Context) {
    context.set_theme(egui::Theme::Dark);
    let mut visuals = Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(60, 60, 60);
    visuals.faint_bg_color = PANEL;
    visuals.code_bg_color = BACKGROUND;
    visuals.selection.bg_fill = SELECTION;
    visuals.selection.stroke = egui::Stroke::new(1.0, TEXT_ON_SELECTION);
    visuals.widgets.noninteractive.bg_stroke.color = BORDER;
    visuals.widgets.inactive.bg_stroke.color = BORDER;
    visuals.widgets.hovered.bg_stroke.color = Color32::from_rgb(75, 75, 75);
    visuals.widgets.active.bg_fill = Color32::from_rgb(55, 55, 55);
    context.set_visuals_of(egui::Theme::Dark, visuals);
    context.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(7.0, 4.0);
        style.visuals.menu_corner_radius = egui::CornerRadius::ZERO;
        style.visuals.window_corner_radius = egui::CornerRadius::ZERO;
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(2);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(2);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(2);
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(13.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        );
    });

    install_system_cjk_font(context);
}

fn install_system_cjk_font(context: &egui::Context) {
    let candidates = if cfg!(windows) {
        vec![
            std::path::PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"),
            std::path::PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![std::path::PathBuf::from(
            "/System/Library/Fonts/PingFang.ttc",
        )]
    } else {
        vec![
            std::path::PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            std::path::PathBuf::from("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"),
        ]
    };

    let Some((path, bytes)) = candidates
        .into_iter()
        .find_map(|path| std::fs::read(&path).ok().map(|bytes| (path, bytes)))
    else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    let font_name = format!("nkg-cjk-{}", path.display());
    fonts
        .font_data
        .insert(font_name.clone(), egui::FontData::from_owned(bytes).into());
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(font_name.clone());
    }
    context.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relative_luminance(color: Color32) -> f32 {
        fn linear(channel: u8) -> f32 {
            let channel = channel as f32 / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        }

        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    fn contrast_ratio(left: Color32, right: Color32) -> f32 {
        let left = relative_luminance(left);
        let right = relative_luminance(right);
        let (lighter, darker) = if left >= right {
            (left, right)
        } else {
            (right, left)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn text_on_blue_backgrounds_meets_normal_text_contrast() {
        for (foreground, background) in [
            (TEXT_ON_SELECTION, SELECTION),
            (TEXT_ON_SELECTION, SELECTED_LINE),
            (MUTED_ON_SELECTION, SELECTED_LINE),
            (TEXT_ON_SELECTION, STATUS),
        ] {
            assert!(contrast_ratio(foreground, background) >= 4.5);
        }
    }
}

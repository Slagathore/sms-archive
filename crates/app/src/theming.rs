//! Window theming and the procedural app icon.

/// Apply the app's cohesive dark theme. The theme is deliberately locked to
/// dark: many hand-painted analytics charts hard-code dark-background colors,
/// so following the OS into light mode produced white-on-light contrast bugs.
pub(crate) fn configure_style(ctx: &egui::Context) {
    use egui::{Color32, FontFamily, FontId, Margin, Rounding, Stroke, TextStyle};

    let accent = Color32::from_rgb(122, 162, 247); // soft indigo
    let accent_dim = Color32::from_rgb(88, 116, 180);

    let mut visuals = egui::Visuals::dark();
    visuals.selection.bg_fill = accent.linear_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0_f32, accent);
    visuals.hyperlink_color = accent;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, accent_dim);
    visuals.widgets.active.bg_stroke = Stroke::new(1.5_f32, accent);
    visuals.panel_fill = Color32::from_gray(24);
    visuals.window_fill = Color32::from_gray(28);
    visuals.extreme_bg_color = Color32::from_gray(16);
    let rounding = Rounding::same(6.0);
    visuals.widgets.noninteractive.rounding = rounding;
    visuals.widgets.inactive.rounding = rounding;
    visuals.widgets.hovered.rounding = rounding;
    visuals.widgets.active.rounding = rounding;
    visuals.window_rounding = Rounding::same(8.0);
    visuals.menu_rounding = rounding;

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(9.0, 5.0);
    style.spacing.menu_margin = Margin::same(6.0);
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(20.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Monospace,
            FontId::new(13.5, FontFamily::Monospace),
        ),
        (
            TextStyle::Button,
            FontId::new(14.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
    ]
    .into();
    ctx.set_style(style);
}

/// Procedurally-generated 64×64 app/taskbar icon (a chat bubble on an indigo
/// tile) so the window isn't stuck with the default blank icon.
pub(crate) fn app_icon() -> egui::IconData {
    const S: i32 = 64;
    let accent = [122u8, 162, 247, 255];
    let bubble = [236u8, 239, 246, 255];
    let mut rgba = vec![0u8; (S * S * 4) as usize];
    let mut put = |x: i32, y: i32, c: [u8; 4]| {
        if (0..S).contains(&x) && (0..S).contains(&y) {
            let i = ((y * S + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&c);
        }
    };
    let in_rounded = |x: i32, y: i32, x0: i32, y0: i32, x1: i32, y1: i32, r: i32| -> bool {
        if x < x0 || x > x1 || y < y0 || y > y1 {
            return false;
        }
        let (in_l, in_r) = (x < x0 + r, x > x1 - r);
        let (in_t, in_b) = (y < y0 + r, y > y1 - r);
        if (in_l || in_r) && (in_t || in_b) {
            let cx = if in_l { x0 + r } else { x1 - r };
            let cy = if in_t { y0 + r } else { y1 - r };
            let (dx, dy) = ((x - cx) as f32, (y - cy) as f32);
            return dx * dx + dy * dy <= (r * r) as f32;
        }
        true
    };
    for y in 0..S {
        for x in 0..S {
            if in_rounded(x, y, 0, 0, S - 1, S - 1, 13) {
                put(x, y, accent);
            }
        }
    }
    let (bx0, by0, bx1, by1) = (13, 15, 50, 41);
    for y in by0..=by1 {
        for x in bx0..=bx1 {
            if in_rounded(x, y, bx0, by0, bx1, by1, 8) {
                put(x, y, bubble);
            }
        }
    }
    for k in 0..7 {
        for x in (22 - k)..22 {
            put(x, by1 + k, bubble);
        }
    }
    for (ly, x0, x1) in [(23, 19, 44), (30, 19, 38)] {
        for x in x0..=x1 {
            for dy in 0..3 {
                put(x, ly + dy, accent);
            }
        }
    }
    egui::IconData {
        rgba,
        width: S as u32,
        height: S as u32,
    }
}

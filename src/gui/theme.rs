//! Palette et style de l'application : thème sombre (par défaut) et thème clair.
//!
//! Une base neutre à peine bleu-vert, une seule couleur d'accent (lagon) pour tout ce qu'on peut
//! toucher, « soleil » pour ce qui est allumé, et trois couleurs d'état (alerte, critique, hors
//! ligne). Chaque texte garde un contraste d'au moins 4,5 : 1 sur son fond, dans les deux thèmes.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Theme};
use homesmart::kinds::Kind;
use std::sync::atomic::{AtomicBool, Ordering};

/// Couleurs fixes des données (graphiques, pièces, zones) : les mêmes dans les deux thèmes.
pub const AMBER: Color32 = Color32::from_rgb(0xf5, 0xa6, 0x23);
pub const GREEN: Color32 = Color32::from_rgb(0x2f, 0xbf, 0x8a);
pub const BLUE: Color32 = Color32::from_rgb(0x4c, 0x9b, 0xf0);
pub const VIOLET: Color32 = Color32::from_rgb(0xa7, 0x8b, 0xfa);
pub const RED: Color32 = Color32::from_rgb(0xf0, 0x55, 0x66);

/// Rôles de couleur d'un thème.
struct Palette {
    bg: Color32,
    nav: Color32,
    surface: Color32,
    surface_hi: Color32,
    raised: Color32,
    raised_hi: Color32,
    line: Color32,
    text: Color32,
    muted: Color32,
    faint: Color32,
    accent: Color32,
    on_accent: Color32,
    sun: Color32,
    warn: Color32,
    crit: Color32,
    off: Color32,
    tile_off: Color32,
    tile_on: Color32,
    tile_ink: Color32,
    tile_ink_2: Color32,
    field: Color32,
    scrim: Color32,
}

const fn c(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const DARK: Palette = Palette {
    bg: c(0x0d1014),
    nav: c(0x12161c),
    surface: c(0x181d24),
    surface_hi: c(0x1c222b),
    raised: c(0x212833),
    raised_hi: c(0x2b3340),
    line: c(0x262d38),
    text: c(0xeef2f6),
    muted: c(0x98a2b1),
    faint: c(0x6b7584),
    accent: c(0x22b8b0),
    on_accent: c(0x04201e),
    sun: c(0xffc145),
    warn: c(0xff8a3d),
    crit: c(0xff5c6c),
    off: c(0x7c8594),
    tile_off: c(0x181d24),
    tile_on: c(0xf7f5f0),
    tile_ink: c(0x15181d),
    tile_ink_2: c(0x5f6672),
    field: c(0x0a0d11),
    scrim: Color32::from_rgba_premultiplied(2, 3, 5, 150),
};

const LIGHT: Palette = Palette {
    bg: c(0xf3f5f7),
    nav: c(0xffffff),
    surface: c(0xffffff),
    surface_hi: c(0xffffff),
    raised: c(0xe8ebef),
    raised_hi: c(0xd3d8df),
    line: c(0xdde2e8),
    text: c(0x131820),
    muted: c(0x5b6574),
    faint: c(0x6a7483),
    accent: c(0x0b7f7a),
    on_accent: c(0xffffff),
    sun: c(0xf5a623),
    warn: c(0xc2510e),
    crit: c(0xd92d3f),
    off: c(0x6a7483),
    tile_off: c(0xe8ebef),
    tile_on: c(0xffffff),
    tile_ink: c(0x131820),
    tile_ink_2: c(0x5b6574),
    field: c(0xeef1f4),
    scrim: Color32::from_rgba_premultiplied(7, 9, 12, 90),
};

static IS_LIGHT: AtomicBool = AtomicBool::new(false);

fn p() -> &'static Palette {
    if is_light() {
        &LIGHT
    } else {
        &DARK
    }
}

pub fn is_light() -> bool {
    IS_LIGHT.load(Ordering::Relaxed)
}

/// Passe d'un thème à l'autre (les deux styles egui sont préparés par `apply`).
pub fn set_light(ctx: &egui::Context, light: bool) {
    IS_LIGHT.store(light, Ordering::Relaxed);
    ctx.set_theme(if light { Theme::Light } else { Theme::Dark });
    ctx.request_repaint();
}

/// Fond de la fenêtre.
pub fn bg() -> Color32 {
    p().bg
}
/// Navigation latérale.
pub fn nav() -> Color32 {
    p().nav
}
/// Panneaux, cartes, fenêtres.
pub fn surface() -> Color32 {
    p().surface
}
/// Surface légèrement relevée (haut des cartes).
pub fn surface_hi() -> Color32 {
    p().surface_hi
}
/// Boutons, puces, pistes des curseurs.
pub fn raised() -> Color32 {
    p().raised
}
/// Relief appuyé (interrupteur éteint, survol).
pub fn raised_hi() -> Color32 {
    p().raised_hi
}
pub fn line() -> Color32 {
    p().line
}
pub fn text() -> Color32 {
    p().text
}
/// Texte secondaire.
pub fn muted() -> Color32 {
    p().muted
}
/// Texte discret (aides, unités).
pub fn faint() -> Color32 {
    p().faint
}
/// Lagon : tout ce qu'on peut toucher (boutons, sélection, interrupteurs), le direct.
pub fn accent() -> Color32 {
    p().accent
}
/// Texte posé sur l'accent.
pub fn on_accent() -> Color32 {
    p().on_accent
}
/// Soleil : un appareil allumé.
pub fn sun() -> Color32 {
    p().sun
}
/// Orange : alerte, à regarder bientôt.
pub fn warn() -> Color32 {
    p().warn
}
/// Framboise : critique (chute, intrusion, coupure).
pub fn crit() -> Color32 {
    p().crit
}
/// Ardoise : hors ligne.
pub fn off() -> Color32 {
    p().off
}
/// Tuile d'un appareil éteint.
pub fn tile_off() -> Color32 {
    p().tile_off
}
/// Tuile d'un appareil allumé (claire dans les deux thèmes), et ses textes.
pub fn tile_on() -> Color32 {
    p().tile_on
}
pub fn tile_ink() -> Color32 {
    p().tile_ink
}
pub fn tile_ink_2() -> Color32 {
    p().tile_ink_2
}
/// Voile derrière un panneau superposé.
pub fn scrim() -> Color32 {
    p().scrim
}
pub fn violet() -> Color32 {
    VIOLET
}

/// Couleur d'un appareil allumé : la même pour tous (soleil), quel que soit son type.
pub fn kind_accent(_kind: &Kind) -> Color32 {
    sun()
}

/// Texte lisible sur un fond donné : presque noir sur un fond clair, blanc sur un fond sombre.
pub fn ink_on(fill: Color32) -> Color32 {
    let lum = 0.2126 * fill.r() as f32 + 0.7152 * fill.g() as f32 + 0.0722 * fill.b() as f32;
    if lum > 150.0 {
        c(0x11151b)
    } else {
        Color32::WHITE
    }
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Couleur avec opacité (0‥1).
pub fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Police grasse (titres, chiffres).
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// Police fine (grands nombres).
pub fn light(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("light".into()))
}

/// Polices système : Segoe UI sous Windows ; Inter, Noto Sans ou DejaVu Sans sous Linux.
/// Les polices d'icônes d'egui restent en secours. Rien n'est téléchargé.
fn install_fonts(ctx: &egui::Context) {
    let win = std::env::var("WINDIR").map(|w| format!("{w}/Fonts")).unwrap_or_else(|_| "C:/Windows/Fonts".into());
    let find = |names: &[&str]| -> Option<Vec<u8>> {
        let dirs = [
            win.as_str(),
            "/usr/share/fonts/truetype/inter",
            "/usr/share/fonts/opentype/inter",
            "/usr/share/fonts/inter",
            "/usr/share/fonts/truetype/noto",
            "/usr/share/fonts/noto",
            "/usr/share/fonts/google-noto",
            "/usr/share/fonts/truetype/dejavu",
            "/usr/share/fonts/dejavu",
        ];
        dirs.iter().flat_map(|d| names.iter().map(move |n| format!("{d}/{n}"))).find_map(|p| std::fs::read(p).ok())
    };
    let mut fonts = egui::FontDefinitions::default();
    let base = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut add = |key: &str, names: &[&str]| -> Option<String> {
        let data = find(names)?;
        fonts.font_data.insert(key.to_string(), std::sync::Arc::new(egui::FontData::from_owned(data)));
        Some(key.to_string())
    };
    let regular = add("ui", &["segoeui.ttf", "Inter-Regular.ttf", "Inter-Regular.otf", "NotoSans-Regular.ttf", "DejaVuSans.ttf"]);
    let strong = add("ui-bold", &["seguisb.ttf", "segoeuib.ttf", "Inter-SemiBold.ttf", "Inter-Bold.ttf", "Inter-SemiBold.otf", "NotoSans-SemiBold.ttf", "NotoSans-Bold.ttf", "DejaVuSans-Bold.ttf"]);
    let thin = add("ui-light", &["segoeuisl.ttf", "segoeuil.ttf", "Inter-Light.ttf", "Inter-Light.otf", "NotoSans-Light.ttf", "DejaVuSans-ExtraLight.ttf"]);
    let family = |first: Option<String>| -> Vec<String> { first.into_iter().chain(base.iter().cloned()).collect() };
    fonts.families.insert(FontFamily::Proportional, family(regular.clone()));
    fonts.families.insert(FontFamily::Name("bold".into()), family(strong.or(regular.clone())));
    fonts.families.insert(FontFamily::Name("light".into()), family(thin.or(regular)));
    ctx.set_fonts(fonts);
}

/// Prépare les deux styles (sombre et clair) ; `set_light` choisit ensuite l'un ou l'autre.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    for (theme, pal) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.button_padding = egui::vec2(14.0, 7.0);
            style.spacing.interact_size.y = 32.0;
            style.spacing.slider_width = 220.0;
            style.spacing.slider_rail_height = 6.0;

            let v = &mut style.visuals;
            *v = if theme == Theme::Dark { egui::Visuals::dark() } else { egui::Visuals::light() };
            v.override_text_color = Some(pal.text);
            v.panel_fill = pal.bg;
            v.window_fill = pal.surface;
            v.extreme_bg_color = pal.field;
            v.faint_bg_color = pal.surface;
            v.code_bg_color = pal.raised;
            v.window_stroke = Stroke::new(1.0, pal.line);
            v.window_corner_radius = CornerRadius::same(20);
            v.menu_corner_radius = CornerRadius::same(14);
            v.selection.bg_fill = alpha(pal.accent, 0.45);
            v.selection.stroke = Stroke::new(1.0, pal.accent);
            v.hyperlink_color = pal.accent;
            v.warn_fg_color = pal.warn;
            v.error_fg_color = pal.crit;
            let r = CornerRadius::same(12);
            for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
                w.corner_radius = r;
                w.fg_stroke = Stroke::new(1.0, pal.text);
            }
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.line);
            v.widgets.noninteractive.bg_fill = pal.surface;
            v.widgets.noninteractive.weak_bg_fill = pal.surface;
            v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, pal.muted);
            v.widgets.inactive.bg_fill = pal.raised;
            v.widgets.inactive.weak_bg_fill = pal.raised;
            v.widgets.inactive.bg_stroke = Stroke::NONE;
            v.widgets.hovered.bg_fill = pal.raised_hi;
            v.widgets.hovered.weak_bg_fill = pal.raised_hi;
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, pal.raised_hi);
            v.widgets.active.bg_fill = mix(pal.raised_hi, pal.accent, 0.25);
            v.widgets.active.weak_bg_fill = mix(pal.raised_hi, pal.accent, 0.25);
            v.widgets.open.bg_fill = pal.raised_hi;
            v.widgets.open.weak_bg_fill = pal.raised_hi;
            v.popup_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(if theme == Theme::Dark { 120 } else { 40 }) };
            v.window_shadow = v.popup_shadow;
        });
    }
    set_light(ctx, is_light());
}

/// Icône de fenêtre : une maison claire sur un carré lagon arrondi.
pub fn window_icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    let inside_round = |x: f32, y: f32| {
        let r = 14.0;
        let cx = x.clamp(r, N as f32 - r);
        let cy = y.clamp(r, N as f32 - r);
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    };
    let in_house = |x: f32, y: f32| {
        let roof = y >= 14.0 && y <= 32.0 && (x - 32.0).abs() <= (y - 14.0) * 1.15;
        let body = (19.0..=45.0).contains(&x) && (30.0..=50.0).contains(&y);
        let door = (28.0..=36.0).contains(&x) && (38.0..=50.0).contains(&y);
        (roof || body) && !door
    };
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let i = (y * N + x) * 4;
            if !inside_round(fx, fy) {
                continue;
            }
            let col = if in_house(fx, fy) {
                [0xf7, 0xf5, 0xf0]
            } else {
                let t = fy / N as f32;
                let m = mix(c(0x2bc4bb), c(0x0b7f7a), t);
                [m.r(), m.g(), m.b()]
            };
            rgba[i..i + 4].copy_from_slice(&[col[0], col[1], col[2], 255]);
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}

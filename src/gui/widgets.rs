//! Widgets maison, animés, aux couleurs de la palette (sombre ou claire).

use super::fx;
use super::icons::{self, Icon};
use super::theme;
use eframe::egui::{self, pos2, vec2, Align2, Color32, CornerRadius, CursorIcon, FontId, Rect, Response, RichText, Sense, Stroke, Ui};

/// Interrupteur façon téléphone : piste en relief, lagon (ou `accent`) quand il est activé.
pub fn toggle(ui: &mut Ui, on: bool, accent: Color32, enabled: bool) -> Response {
    let size = vec2(48.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(resp.id, on, 0.16);
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), enabled && resp.hovered(), 0.12);
        let p = ui.painter();
        let mut bg = theme::mix(theme::raised_hi(), accent, t);
        if !enabled {
            bg = theme::mix(bg, theme::surface(), 0.55);
        }
        p.rect_filled(rect, CornerRadius::same(14), bg);
        let r = size.y / 2.0 - 3.0 + hover * 0.6;
        let x = egui::lerp((rect.left() + 3.0 + r)..=(rect.right() - 3.0 - r), fx::ease_out(t));
        let knob = if enabled { Color32::WHITE } else { theme::mix(Color32::WHITE, theme::surface(), 0.4) };
        p.circle_filled(pos2(x, rect.center().y + 1.0), r, Color32::from_black_alpha(45));
        p.circle_filled(pos2(x, rect.center().y), r, knob);
        if enabled && resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }
    resp
}

/// Pastille ronde avec l'icône de l'appareil ; pleine (soleil) quand l'appareil est allumé.
pub fn icon_bubble(ui: &mut Ui, icon: Icon, size: f32, on: f32, accent: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_bubble(ui, rect, icon, on, accent);
    resp
}

/// Dessine une pastille d'icône dans `rect` (sans réserver de place).
pub fn paint_bubble(ui: &Ui, rect: Rect, icon: Icon, on: f32, accent: Color32) {
    let p = ui.painter();
    let size = rect.width();
    let fill = theme::mix(theme::raised(), accent, on);
    p.circle_filled(rect.center(), size / 2.0, fill);
    let color = theme::mix(theme::muted(), theme::ink_on(accent), on);
    icons::paint(p, Rect::from_center_size(rect.center(), vec2(size * 0.5, size * 0.5)), icon, color);
}

/// Petit bouton icône sans fond (fermer…).
pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    if h > 0.0 {
        ui.painter().circle_filled(rect.center(), size / 2.0, theme::alpha(theme::raised_hi(), h));
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let color = theme::mix(theme::muted(), theme::text(), h);
    icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(size * 0.5, size * 0.5)), icon, color);
    resp
}

/// Bouton rond de 40 px (en-têtes de page) : en relief, ou lagon s'il est l'action principale.
pub fn round_button(ui: &mut Ui, icon: Icon, primary: bool) -> Response {
    let size = 40.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let (fill, fg) = if primary {
        (theme::mix(theme::accent(), theme::text(), 0.12 * h), theme::on_accent())
    } else {
        (theme::mix(theme::raised(), theme::raised_hi(), h), theme::text())
    };
    ui.painter().circle_filled(rect.center(), size / 2.0, fill);
    icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(19.0, 19.0)), icon, fg);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

/// Point d'état.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 6.0, theme::alpha(color, 0.18));
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// Étiquette colorée ; le texte prend la couleur lisible sur `bg` (le premier argument est ignoré).
pub fn pill(ui: &mut Ui, text: &str, _fg: Color32, bg: Color32) {
    let fg = theme::ink_on(bg);
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::bold(10.5), fg);
    let size = galley.size() + vec2(16.0, 6.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), bg);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
}

/// Tuile de mesure ; le nombre défile en douceur jusqu'à sa nouvelle valeur. Un petit écart
/// (moins de 2 %, comme la puissance qui fluctue d'un watt) s'affiche tout de suite : sinon la
/// tuile s'animerait, et la fenêtre se redessinerait, à chaque mesure.
pub fn stat_value(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, label: &str, value: f64, decimals: usize, unit: &str, color: Color32, width: f32) {
    let key = egui::Id::unique((&id, "stat-last"));
    let last: Option<f32> = ui.ctx().data(|d| d.get_temp(key));
    let small = last.is_none_or(|l| (value as f32 - l).abs() <= 0.02 * l.abs().max(value.abs() as f32));
    ui.ctx().data_mut(|d| d.insert_temp(key, value as f32));
    let shown = fx::tween(ui, (id, "stat"), value as f32, if small { 0.0 } else { 0.7 }) as f64;
    stat(ui, label, &fr(shown, decimals), unit, color, width);
}

/// Tuile de mesure (texte déjà formaté).
pub fn stat(ui: &mut Ui, label: &str, value: &str, unit: &str, color: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 66.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(14), theme::raised());
    p.text(rect.left_top() + vec2(12.0, 10.0), Align2::LEFT_TOP, label, FontId::proportional(11.5), theme::muted());
    let g = p.layout_no_wrap(value.to_string(), theme::light(25.0), color);
    let base = rect.left_bottom() + vec2(12.0, -10.0);
    let w = g.size().x;
    p.galley(base - vec2(0.0, g.size().y), g, color);
    p.text(base + vec2(w + 4.0, -4.0), Align2::LEFT_BOTTOM, unit, FontId::proportional(13.0), theme::muted());
}

/// Contrôle segmenté sobre : piste en relief, l'option choisie se détache en clair (le fond de
/// la pastille glisse d'une option à l'autre). `_accent` n'est plus utilisé : même allure partout.
pub fn segmented(ui: &mut Ui, options: &[&str], selected: Option<usize>, _accent: Color32) -> Option<usize> {
    let font = FontId::proportional(13.5);
    let widths: Vec<f32> = options
        .iter()
        .map(|o| (ui.painter().layout_no_wrap(o.to_string(), theme::bold(13.5), theme::text()).size().x + 28.0).max(56.0))
        .collect();
    let pad = 3.0;
    let total = widths.iter().sum::<f32>() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(total, 34.0), Sense::hover());
    let id = resp.id;
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(12), theme::raised());
    let mut xs = Vec::with_capacity(widths.len());
    let mut x = pad;
    for w in &widths {
        xs.push(x);
        x += w;
    }
    let ctx = ui.ctx().clone();
    let vis = ctx.animate_bool_with_time(id.with("vis"), selected.is_some(), 0.15);
    let knob = if theme::is_light() { Color32::WHITE } else { theme::raised_hi() };
    if let Some(sel) = selected.filter(|s| *s < widths.len()) {
        let ax = ctx.animate_value_with_time(id.with("x"), xs[sel], 0.22);
        let aw = ctx.animate_value_with_time(id.with("w"), widths[sel], 0.22);
        let r = Rect::from_min_size(rect.min + vec2(ax, pad), vec2(aw, rect.height() - pad * 2.0));
        if theme::is_light() {
            fx::shadow(p, r, 9, -0.6);
        }
        p.rect_filled(r, CornerRadius::same(9), theme::alpha(knob, vis));
    }
    let mut clicked = None;
    for (i, opt) in options.iter().enumerate() {
        let r = Rect::from_min_size(rect.min + vec2(xs[i], pad), vec2(widths[i], rect.height() - pad * 2.0));
        let item = ui.interact(r, id.with(("opt", i)), Sense::click());
        let sel = selected == Some(i);
        let hover = ctx.animate_bool_with_time(item.id, item.hovered() && !sel, 0.12);
        let color = if sel { theme::text() } else { theme::mix(theme::muted(), theme::text(), hover) };
        let f = if sel { theme::bold(13.5) } else { font.clone() };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, *opt, f, color);
        if item.hovered() {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }
        if item.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// Grand interrupteur à deux positions (« Éteint » / « Allumé ») ; la position allumée prend
/// la couleur soleil. Renvoie `Some(true)` pour allumer, `Some(false)` pour éteindre.
pub fn power_switch(ui: &mut Ui, on: bool, enabled: bool) -> Option<bool> {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 56.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(18), theme::raised());
    let half = (rect.width() - 8.0) / 2.0;
    let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), on, 0.18);
    let knob = Rect::from_min_size(rect.min + vec2(4.0 + half * fx::ease_out(t), 4.0), vec2(half, rect.height() - 8.0));
    let fill = theme::mix(if theme::is_light() { Color32::WHITE } else { theme::raised_hi() }, theme::sun(), t);
    p.rect_filled(knob, CornerRadius::same(14), fill);
    let mut out = None;
    for (i, label) in ["Éteint", "Allumé"].into_iter().enumerate() {
        let r = Rect::from_min_size(rect.min + vec2(4.0 + half * i as f32, 4.0), vec2(half, rect.height() - 8.0));
        let item = ui.interact(r, resp.id.with(i), if enabled { Sense::click() } else { Sense::hover() });
        let sel = (i == 1) == on;
        let color = if sel && on { theme::ink_on(theme::sun()) } else if sel { theme::text() } else { theme::muted() };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, label, theme::bold(15.0), if enabled { color } else { theme::faint() });
        if enabled && item.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if item.clicked() && !sel {
            out = Some(i == 1);
        }
    }
    out
}

/// Ligne « libellé à gauche, contrôle à droite ». Le contrôle est placé d'abord : le libellé
/// et son explication passent à la ligne dans la place qui reste, sans jamais le recouvrir.
/// Le contrôle est centré sur la hauteur du libellé (mesurée à l'image précédente).
pub fn row<R>(ui: &mut Ui, label: &str, hint: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    // Identifiant propre à cette ligne (deux lignes au même libellé ne se gênent pas).
    let id = ui.next_auto_id().with("row-height");
    let height = ui.ctx().data(|d| d.get_temp::<f32>(id)).unwrap_or(30.0).max(30.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), height), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.set_min_height(height);
        let inner = add(ui);
        let text = ui.allocate_ui_with_layout(vec2(ui.available_width(), height), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(label).size(14.0));
            if let Some(h) = hint {
                ui.label(RichText::new(h).size(11.5).color(theme::faint()));
            }
        });
        let used = text.response.rect.height();
        if (used - height).abs() > 0.5 {
            // Hauteur changée (premier affichage, texte passé à la ligne) : ce passage est refait
            // aussitôt, sans image décalée à l'écran.
            ui.ctx().data_mut(|d| d.insert_temp(id, used));
            ui.ctx().request_discard("hauteur d'une ligne de réglage");
        }
        inner
    })
    .inner
}

/// Ligne de navigation d'un panneau : libellé, valeur discrète et chevron ; toute la ligne est cliquable.
pub fn nav_row(ui: &mut Ui, label: &str, value: &str) -> Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 52.0), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    if h > 0.0 {
        p.rect_filled(rect.expand2(vec2(8.0, -3.0)), CornerRadius::same(12), theme::alpha(theme::raised(), h));
    }
    p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, theme::line()));
    p.text(rect.left_center(), Align2::LEFT_CENTER, label, FontId::proportional(15.0), theme::text());
    let chev = Rect::from_center_size(pos2(rect.right() - 8.0, rect.center().y), vec2(15.0, 15.0));
    icons::paint(p, chev, Icon::Chevron, theme::faint());
    if !value.is_empty() {
        p.text(pos2(chev.left() - 10.0, rect.center().y), Align2::RIGHT_CENTER, value, FontId::proportional(13.5), theme::muted());
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

/// Titre de page (30 px) ; le contenu à droite (boutons ronds) est ajouté par `right`.
pub fn page_header(ui: &mut Ui, title: &str, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(42.0);
        ui.label(RichText::new(title).font(theme::bold(30.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
}

/// Phrase d'état sous un titre de page.
pub fn lede(ui: &mut Ui, strong: &str, rest: &str) {
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(RichText::new(strong).size(15.0).strong().color(theme::text()));
        if !rest.is_empty() {
            ui.label(RichText::new(rest).size(15.0).color(theme::muted()));
        }
    });
}

/// Intertitre : 14 px, demi-gras, discret.
pub fn section_title(ui: &mut Ui, title: &str, right: Option<&str>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::bold(14.0)).color(theme::muted()));
        if let Some(r) = right {
            ui.label(RichText::new(r).size(12.5).color(theme::faint()));
        }
    });
    ui.add_space(2.0);
}

/// Bouton principal : rempli, texte lisible sur sa couleur.
pub fn primary_button(text: &str, accent: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).font(theme::bold(14.0)).color(theme::ink_on(accent))).fill(accent).corner_radius(CornerRadius::same(12))
}

/// Cadre d'un panneau (carte de contenu) : surface, coins de 20 px.
pub fn panel_frame() -> egui::Frame {
    let f = egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(20)).inner_margin(egui::Margin::same(18));
    if theme::is_light() {
        f.shadow(egui::epaint::Shadow { offset: [0, 2], blur: 12, spread: 0, color: Color32::from_black_alpha(16) })
    } else {
        f.stroke(Stroke::new(1.0, theme::line()))
    }
}

/// Limite la largeur du contenu sans jamais dépasser la place disponible (`set_max_width`
/// seul peut l'élargir au-delà de la fenêtre quand elle est étroite).
pub fn cap_width(ui: &mut Ui, max: f32) {
    ui.set_max_width(ui.available_width().min(max));
}

/// Nombre à la française (virgule décimale).
pub fn fr(x: f64, decimals: usize) -> String {
    homesmart::kinds::fr(x, decimals)
}

/// Puissance lisible : « 45 W », « 1,4 kW ».
pub fn watts(w: f64) -> String {
    if w.abs() >= 1000.0 {
        format!("{} kW", fr(w / 1000.0, 1))
    } else {
        format!("{} W", fr(w, 0))
    }
}


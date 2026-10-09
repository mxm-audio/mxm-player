//! The device rail: one compact vertical strip per device, in the free region to the right of
//! the sequencer's rows.
//!
//! **The owner's brief, 2026-09-04**: the player shows the chain the way Bitwig shows a track's
//! devices — a narrow vertical strip each, a coloured header, the name read bottom-to-top, and a
//! `+` strip at the end — and shows **no effect parameters at all**. An effect's controls are its
//! own editor's business. What a strip offers is exactly the chain's acts: open the editor,
//! switch the effect off (which stops calling it, so it costs no CPU), move it earlier or later,
//! and remove it.
//!
//! **Where it sits is the owner's placement too**: the empty region right of the sequencer, which
//! is on screen whether or not the parameter panel is collapsed — and collapsed is how the player
//! is used beside an effect's editor, so a rail in the collapsible band was a rail that vanished
//! exactly when it was wanted. The sequencer panel measures its rows and hands the rail what they
//! leave; the strips are sized to that band's height and scroll sideways when a long chain
//! outgrows the width.
//!
//! **Everything lines up, by construction.** Every strip is allocated the same height and laid
//! top-aligned, so the headers share one edge; the controls are placed from the foot, so their
//! rows share one baseline across strips; the gaps are constants. The strips are laid out with
//! `with_layout`, not `horizontal`, because a horizontal row starts one button tall and grows as
//! children land in it, which staggers tall children and starves the names of height.
//!
//! **The rail is the chain, and the source is not in it** (the owner, 2026-09-04). The instrument
//! already has its name in the status bar's picker and its editor behind that bar's Show/Hide
//! editor button; a strip repeating both would be a second control for one act, and the first
//! thing to go wrong when they disagree. So the strips are the effects, left to right in signal
//! order, then `+` — the order the sound goes through them, and a chain drawn in any other order
//! would have to be explained.
//!
//! Everything a strip decides is collected as a [`RailAct`] and applied **after** drawing, so the
//! engine is never edited while the strips are still borrowing what they draw from.

use super::{PlayerApp, adapter, plugin_menu};
use crate::discovery::Found;
use crate::engine::editor::EditorTarget;
use egui::epaint::TextShape;
use egui::{Align, Layout, RichText, Sense, vec2};
use std::path::PathBuf;

/// One strip's width: two small buttons side by side, or three narrower ones.
const STRIP_WIDTH: f32 = 64.0;
const STRIP_GAP: f32 = 6.0;
/// The coloured header — the device's identity and its number, read before its name.
const HEADER_HEIGHT: f32 = 16.0;
const CONTROL_HEIGHT: f32 = 20.0;
const CONTROL_GAP: f32 = 2.0;
const CONTROL_FONT: f32 = 11.0;
/// The strip's inner margin below the header and around the controls.
const INNER_MARGIN: f32 = 3.0;
/// The gap between the sequencer's rows and the rail; the separator line sits in its middle.
pub const RAIL_INSET: f32 = 18.0;

/// Something a strip asked for this frame, applied once the frame is drawn.
enum RailAct {
    Add(PathBuf, String),
    Remove(usize),
    Move { from: usize, to: usize },
    Bypass { index: usize, bypassed: bool },
    Editor(usize),
}

impl PlayerApp {
    /// Draws the rail into `free`, the region the sequencer's rows left to their right.
    ///
    /// A child of the panel rather than a panel of its own, so it neither moves the rows nor
    /// changes the panel's height. Too narrow for one strip means no rail rather than a clipped
    /// one; anything wider scrolls sideways once the chain outgrows it.
    pub(super) fn fx_rail(&mut self, ui: &mut egui::Ui, free: egui::Rect) {
        if free.width() < STRIP_WIDTH || free.height() < HEADER_HEIGHT + 4.0 * CONTROL_HEIGHT {
            return;
        }
        let mut acts: Vec<RailAct> = Vec::new();
        let ctx = ui.ctx().clone();

        // The line that separates the rows from the rail, the band's full height.
        ui.painter().vline(
            free.left() - RAIL_INSET / 2.0,
            free.y_range(),
            ui.visuals().widgets.noninteractive.bg_stroke,
        );

        let mut rail = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(free)
                .layout(Layout::left_to_right(Align::Min)),
        );
        egui::ScrollArea::horizontal()
            .id_salt("fx-rail")
            .show(&mut rail, |ui| {
                let height = ui.available_height();
                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.x = STRIP_GAP;
                    for index in 0..self.engine.fx_len() {
                        self.fx_strip(ui, height, index, &mut acts);
                    }
                    self.add_strip(ui, height, &mut acts);
                });
            });

        for act in acts {
            let outcome = match act {
                RailAct::Add(bundle, id) => self.add_fx(bundle, id).map(|_| ()),
                RailAct::Remove(index) => self.remove_fx(index),
                RailAct::Move { from, to } => self.move_fx(from, to),
                RailAct::Bypass { index, bypassed } => self.set_fx_bypassed(index, bypassed),
                RailAct::Editor(index) => {
                    let outcome = self.show_fx_editor(index);
                    // As the top bar's "Show editor" does: a foreign window appearing invalidates
                    // ours without generating input, so the frame has to be asked for.
                    ctx.request_repaint();
                    outcome
                }
            };
            if let Err(reason) = outcome {
                self.status = Some(reason);
            }
        }
    }

    /// One effect's strip: identity, on/off, its editor, its place in the chain, and removal.
    fn fx_strip(&mut self, ui: &mut egui::Ui, height: f32, index: usize, acts: &mut Vec<RailAct>) {
        let tokens = adapter::tokens_for(ui);
        let Some(info) = self.engine.fx_info().into_iter().nth(index) else {
            return;
        };
        let name = self
            .found
            .iter()
            .find(|f| f.id == info.plugin_id)
            .map_or_else(|| info.plugin_id.clone(), |f| f.name.clone());
        let last = index + 1 == self.engine.fx_len();
        let editor_visible = self
            .engine
            .editor_state_for(EditorTarget::Fx(index))
            .visible;
        let has_editor = self.engine.fx_editor_floating_supported(index);
        let bypassed = info.bypassed;

        // Off is drawn as off: the header loses its colour, so a silent chain reads as one at a
        // glance rather than needing every button read.
        let colour = if bypassed {
            tokens.text_disabled
        } else {
            tokens.accent
        };
        let detail = format!(
            "{}\n{} in → {} out · {}\n{}",
            info.plugin_id,
            info.input_channels,
            info.output_channels,
            info.selection,
            info.bundle.display()
        );
        let inner = STRIP_WIDTH - 2.0 * INNER_MARGIN;
        let half = (inner - CONTROL_GAP) / 2.0;
        let third = (inner - 2.0 * CONTROL_GAP) / 3.0;

        strip(
            ui,
            height,
            colour,
            &(index + 1).to_string(),
            &name,
            &detail,
            2,
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = CONTROL_GAP;
                    if ui
                        .add(small_toggle(ui, "On", !bypassed, half))
                        .on_hover_text(
                            "Off bypasses the effect and stops calling it, so it costs no CPU. \
                             It comes back reset — no stale tail from before it was switched off.",
                        )
                        .clicked()
                    {
                        acts.push(RailAct::Bypass {
                            index,
                            bypassed: !bypassed,
                        });
                    }
                    if ui
                        .add_enabled(has_editor, small_toggle(ui, "Edit", editor_visible, half))
                        .on_hover_text(
                            "The effect's own interface, in its own window. One editor at a time.",
                        )
                        .on_disabled_hover_text("This effect has no interface of its own.")
                        .clicked()
                    {
                        acts.push(RailAct::Editor(index));
                    }
                });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = CONTROL_GAP;
                    if ui
                        .add_enabled(index > 0, small_button("<", third))
                        .on_hover_text("Earlier in the chain")
                        .clicked()
                    {
                        acts.push(RailAct::Move {
                            from: index,
                            to: index - 1,
                        });
                    }
                    if ui
                        .add_enabled(!last, small_button(">", third))
                        .on_hover_text("Later in the chain")
                        .clicked()
                    {
                        acts.push(RailAct::Move {
                            from: index,
                            to: index + 1,
                        });
                    }
                    if ui
                        .add(small_button("×", third))
                        .on_hover_text("Remove from the chain")
                        .clicked()
                    {
                        acts.push(RailAct::Remove(index));
                    }
                });
            },
        );
    }

    /// The `+` strip: the effect picker. The scan's effect-capable plugins in columns, then — in a
    /// submenu, disabled, each with its reason — the ones that cannot be effects, by the same rule
    /// the plugin picker follows: a refusal moves, it does not disappear (`plugin_menu`).
    fn add_strip(&mut self, ui: &mut egui::Ui, height: f32, acts: &mut Vec<RailAct>) {
        let tokens = adapter::tokens_for(ui);
        let full = self.engine.fx_len() >= crate::engine::fx::MAX_FX;

        ui.allocate_ui_with_layout(
            vec2(STRIP_WIDTH, height),
            Layout::top_down(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.y = CONTROL_GAP;
                paint_card(ui, height, tokens.surface_2);
                let (header, _) =
                    ui.allocate_exact_size(vec2(STRIP_WIDTH, HEADER_HEIGHT), Sense::hover());
                ui.painter()
                    .rect_filled(header, header_rounding(), tokens.border_strong);
                // Centred in what is left below the header, as a `+` with nothing to name.
                let button_height = 28.0;
                ui.add_space(((height - HEADER_HEIGHT - button_height) / 2.0).max(0.0));
                ui.push_id("add-effect", |ui| {
                    ui.add_enabled_ui(!full, |ui| {
                        let menu = ui.menu_button(RichText::new("+").size(18.0), |ui| {
                            let chosen = plugin_menu::contents(
                                ui,
                                "effect",
                                &self.found,
                                Found::is_effect,
                                Found::effect_refusal_reason,
                                &plugin_menu::Words {
                                    nothing: "Nothing found can be an effect.",
                                    refused: "cannot be an effect",
                                },
                                |_| {},
                            );
                            if let Some((bundle, id)) = chosen {
                                acts.push(RailAct::Add(bundle, id));
                            }
                        });
                        if menu.inner.is_none() {
                            plugin_menu::forget_search(ui.ctx(), "effect");
                        }
                        menu.response
                            .on_hover_text("Add an effect after the last one.")
                            .on_disabled_hover_text(format!(
                                "The chain holds at most {} effects.",
                                crate::engine::fx::MAX_FX
                            ));
                    });
                });
            },
        );
    }
}

/// One device strip: a card with a coloured header carrying the device's mark, the name read
/// bottom-to-top in what is left, and `control_rows` rows of the caller's controls at the foot.
///
/// The name is painted rather than laid out as a label because egui lays text out horizontally;
/// a vertical strip needs the galley rotated a quarter turn, which is a paint operation. It is
/// shortened with an ellipsis to the height left above the controls, so a long name never draws
/// over them; the full identity is the hover text.
#[allow(clippy::too_many_arguments)]
fn strip(
    ui: &mut egui::Ui,
    height: f32,
    colour: egui::Color32,
    mark: &str,
    name: &str,
    detail: &str,
    control_rows: usize,
    controls: impl FnOnce(&mut egui::Ui),
) {
    let tokens = adapter::tokens_for(ui);
    ui.allocate_ui_with_layout(
        vec2(STRIP_WIDTH, height),
        Layout::top_down(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.y = CONTROL_GAP;
            paint_card(ui, height, tokens.surface_2);

            let (header, _) =
                ui.allocate_exact_size(vec2(STRIP_WIDTH, HEADER_HEIGHT), Sense::hover());
            ui.painter().rect_filled(header, header_rounding(), colour);
            ui.painter().text(
                header.center(),
                egui::Align2::CENTER_CENTER,
                mark,
                egui::FontId::proportional(CONTROL_FONT),
                tokens.canvas,
            );

            // The name takes exactly what is left above the controls, so the controls land at
            // the foot of the strip whatever the band's height — and at the same foot in every
            // strip, since every strip is given the same height.
            let controls_height =
                control_rows as f32 * (CONTROL_HEIGHT + CONTROL_GAP) + INNER_MARGIN;
            let name_height = (ui.available_height() - controls_height).max(0.0);
            let (name_rect, response) =
                ui.allocate_exact_size(vec2(STRIP_WIDTH, name_height), Sense::hover());
            response.on_hover_text(detail);
            paint_vertical_name(ui, name_rect, name, tokens.text_primary);

            controls(ui);
        },
    );
}

/// The card behind a strip, painted before its contents so they sit on it.
fn paint_card(ui: &egui::Ui, height: f32, fill: egui::Color32) {
    let rect = egui::Rect::from_min_size(ui.cursor().min, vec2(STRIP_WIDTH, height));
    ui.painter().rect_filled(rect, 3.0, fill);
}

fn header_rounding() -> egui::CornerRadius {
    egui::CornerRadius {
        nw: 3,
        ne: 3,
        sw: 0,
        se: 0,
    }
}

/// A small text button that fits a strip. The text is the accessibility label the conformance
/// sweep sees, so it stays a word.
fn small_button(text: &str, width: f32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).size(CONTROL_FONT))
        .min_size(vec2(width, CONTROL_HEIGHT))
}

/// A small toggle that reads as a control before it is touched — the same resting border and
/// accent-stroked selection the top bar's toggles have (design system §7.2).
fn small_toggle(ui: &egui::Ui, text: &str, selected: bool, width: f32) -> egui::Button<'static> {
    let button = small_button(text, width)
        .selected(selected)
        .frame_when_inactive(true);
    if selected {
        button.stroke(egui::Stroke::new(
            1.5,
            mxm_ui::theme::tokens(ui.ctx()).accent,
        ))
    } else {
        button
    }
}

/// Paints `name` reading bottom-to-top, centred in `rect`, shortened with an ellipsis if it
/// would not fit.
fn paint_vertical_name(ui: &egui::Ui, rect: egui::Rect, name: &str, colour: egui::Color32) {
    let font = egui::FontId::proportional(CONTROL_FONT);
    let painter = ui.painter();
    let room = rect.height() - 2.0 * INNER_MARGIN;
    let mut text = name.to_owned();
    let mut galley = painter.layout_no_wrap(text.clone(), font.clone(), colour);
    // Shorten until the rotated text fits the height, keeping the head of the name: the tail of
    // an id is where two builds of one plugin differ least.
    while galley.size().x > room && text.chars().count() > 1 {
        let keep = text.trim_end_matches('…').chars().count().saturating_sub(1);
        text = text.chars().take(keep).collect::<String>() + "…";
        galley = painter.layout_no_wrap(text.clone(), font.clone(), colour);
    }
    if room <= 0.0 || galley.size().x > room {
        return;
    }
    // A quarter turn anticlockwise about the galley's top-left corner: galley x runs up the
    // screen and galley y runs right. So the anchor sits at the bottom of the text's run and to
    // the left of its height, both centred in the rect.
    let anchor = egui::pos2(
        rect.center().x - galley.size().y / 2.0,
        rect.center().y + galley.size().x / 2.0,
    );
    painter.add(TextShape::new(anchor, galley, colour).with_angle(-std::f32::consts::FRAC_PI_2));
}

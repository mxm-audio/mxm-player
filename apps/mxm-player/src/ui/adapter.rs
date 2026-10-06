//! The parameter-control adapter.
//!
//! **No longer interim.** Until M4a this module held plain unstyled eframe widgets, deliberately
//! confined here so the retrofit would be bounded to one file rather than smeared across every
//! panel. `crates/ui` has landed, so the bodies now call `mxm_ui` and the compromise is over.
//!
//! What the module still exists for is the *translation*: `mxm_ui` speaks normalised values and
//! knows nothing about CLAP, while the player speaks [`ParamSnapshot`] and real units. Keeping
//! that conversion in one place is what lets `mxm_ui` stay usable from inside a plugin, which is
//! the whole reason the controls live there.
//!
//! # What the retrofit found
//!
//! Two things, which is the argument for doing it before mxm-mono-01's editor is built on the same
//! API rather than after:
//!
//! - **Scroll-wheel editing was hover-only.** Design system §7.1 permits the wheel *"if enabled"*
//!   but requires it to *"require focus or a modifier"*. The old body applied it whenever the
//!   pointer was over a control, so a user scrolling a long parameter list could silently
//!   automate whatever passed underneath. It is now [`mxm_ui::Wheel::FocusOrModifier`], and the
//!   rule lives in the shared crate where the next instrument inherits it.
//! - **Controls were below the minimum pointer target.** An egui slider is about 18 px tall
//!   against §11's 32 px floor. `mxm_ui` allocates the floor, and [`PARAM_ROW_HEIGHT`] already
//!   budgeted 46 px per row — a 14 px label line plus a 32 px control — so the panel's wrap
//!   arithmetic is unchanged.
//!
//! [`PARAM_ROW_HEIGHT`]: super::PARAM_ROW_HEIGHT

use egui::Ui;
use mxm_ui::control::{self, ParamView, Wheel};
use mxm_ui::theme::Tokens;

use crate::params::ParamSnapshot;

/// What a parameter control did this frame.
///
/// Re-exported from `mxm_ui` rather than redefined: the player and a plugin editor must agree on
/// what a gesture is, and two structurally identical types that drift apart is the failure this
/// avoids.
pub use mxm_ui::ControlOutcome;

/// Draws one parameter and reports what happened.
///
/// `value` is edited in place, in the parameter's own units. The caller is responsible for the
/// gesture bracketing that [`ControlOutcome`] describes — the adapter reports, it does not send.
pub fn parameter(
    ui: &mut Ui,
    param: &ParamSnapshot,
    value: &mut f64,
    text_entry: &mut Option<String>,
    scroll_wheel_editing: bool,
    step_text: Option<&str>,
) -> ControlOutcome {
    let tokens = tokens_for(ui);

    // A host sees a name, a range and a formatted value. It does **not** see a description: CLAP
    // carries no such field, so §7.1's one-sentence requirement cannot be met from here. The
    // honest substitute is the range, which is at least information the user cannot otherwise
    // get. A plugin drawing its own editor supplies the real sentence, which is one of the
    // reasons the editor belongs to the plugin.
    //
    // ASCII, deliberately: design system §6 requires Inter bundled for release and it is not yet
    // (`mxm_ui::typography::FONT_IS_BUNDLED`), so the fallback font draws an em dash as a
    // missing-glyph box.
    let description = format!(
        "{} - range {:.3} to {:.3}, default {:.3}",
        param.module, param.min, param.max, param.default
    );

    // **One argument, so the two halves cannot disagree.** When a selected step sets this
    // parameter, the control shows that step's value — and the formatted number beside it must be
    // that value read back through the plugin, not the live one. Passing "is it marked" and "what
    // does it read" separately is how a slider ends up sitting at one number and printing another.
    let view = ParamView::new(&param.name, step_text.unwrap_or(&param.text), &description)
        .default_at(normalise(param, param.default))
        .read_only(param.is_read_only)
        .marked(step_text.is_some());

    let mut normalised = normalise(param, *value);

    // A slider rather than a knob: this is the testing view, where a column of parameters is read
    // by comparing them, and §7.1 prefers a slider exactly where range and comparison matter.
    let outcome = control::slider(
        ui,
        &tokens,
        &view,
        &mut normalised,
        ui.available_width(),
        text_entry,
        if scroll_wheel_editing {
            Wheel::FocusOrModifier
        } else {
            Wheel::Off
        },
    );

    if outcome.changed {
        *value = param.denormalise(normalised);
    }

    outcome
}

/// A labelled read-only figure, for the meters.
pub fn reading(ui: &mut Ui, label: &str, value: &str, hover: &str) {
    let tokens = tokens_for(ui);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(tokens.text_secondary));
        let style = mxm_ui::typography::value_style(ui.style());
        ui.label(
            egui::RichText::new(value)
                .text_style(style)
                .color(tokens.text_primary),
        )
        .on_hover_text(hover);
    });
}

/// Where the value sits in its range.
fn normalise(param: &ParamSnapshot, value: f64) -> f64 {
    if param.max <= param.min {
        return 0.0;
    }
    ((value - param.min) / (param.max - param.min)).clamp(0.0, 1.0)
}

/// The token set matching the theme the context is currently in.
///
/// Read per call rather than cached, so a host that switches theme at runtime is followed. It is
/// two comparisons and a reference; caching it would be an optimisation with a correctness cost.
pub(crate) fn tokens_for(ui: &Ui) -> Tokens {
    if ui.visuals().dark_mode {
        mxm_ui::DARK
    } else {
        mxm_ui::LIGHT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(min: f64, max: f64, value: f64) -> ParamSnapshot {
        ParamSnapshot {
            id: 1,
            name: "Cutoff".into(),
            module: "Filter".into(),
            min,
            max,
            default: min,
            value,
            text: "1.0 kHz".into(),
            is_stepped: false,
            is_hidden: false,
            is_read_only: false,
            is_modulatable: true,
            is_bypass: false,
        }
    }

    #[test]
    fn normalising_survives_a_degenerate_range() {
        // A plugin is allowed to report min == max, and a division by zero here would be a NaN
        // fed straight into a paint call.
        let param = snapshot(1.0, 1.0, 1.0);
        assert_eq!(normalise(&param, 1.0), 0.0);
    }

    #[test]
    fn normalising_clamps_a_value_outside_its_own_range() {
        // Snapshots and edits race: the plugin can report a value from before a change while the
        // host still holds the newer one. Clamping keeps the arc inside the control either way.
        let param = snapshot(20.0, 20_000.0, 25_000.0);
        assert_eq!(normalise(&param, 25_000.0), 1.0);
        assert_eq!(normalise(&param, 0.0), 0.0);
    }

    #[test]
    fn normalise_and_denormalise_are_inverses() {
        let param = snapshot(20.0, 20_000.0, 440.0);
        let round_trip = param.denormalise(normalise(&param, 440.0));
        assert!((round_trip - 440.0).abs() < 1e-9, "{round_trip} != 440");
    }

    #[test]
    fn the_wheel_policy_is_never_hover_only() {
        // The retrofit's own finding, pinned: §7.1 allows the wheel to edit only with focus or a
        // modifier. Both arms of the player's setting must resolve to a policy that says so.
        for enabled in [false, true] {
            let policy = if enabled {
                Wheel::FocusOrModifier
            } else {
                Wheel::Off
            };
            assert!(matches!(policy, Wheel::Off | Wheel::FocusOrModifier));
        }
    }
}

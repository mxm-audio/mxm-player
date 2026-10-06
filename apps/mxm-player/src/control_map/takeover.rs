//! Pickup: what stops a knob from yanking a parameter when you first touch it.
//!
//! A physical knob sits at 20%; the parameter is at 80% because a preset put it there. Moving the
//! knob must not jump the value to 20%. So a knob is **armed** until it reaches where the parameter
//! already is, and only then does it take control.
//!
//! # Scoped per control, not per parameter
//!
//! State is keyed by CC, because the player supports several independent MIDI sources and a
//! parameter can be reachable from more than one control at once — a fixed knob and a bank slot
//! can both land on Cutoff. Two knobs on one parameter each have to earn control separately, or
//! whichever moved second would be treated as though it had already caught up.
//!
//! # Re-arming
//!
//! A knob that has taken control keeps it until the parameter moves for some other reason: the
//! on-screen panel, a state load, plugin-driven output, a page change, a layout reload. Then it is
//! armed again, so the next turn picks up from the new value rather than snapping back to where
//! the knob happens to be sitting.

use super::curve::CC_MAX;

/// How close a knob must get to the parameter to take control, in position.
///
/// One controller step. Landing exactly on the value is the common case when the knob has not been
/// touched since it last drove the parameter, and it should not require a wiggle.
fn catch_width() -> f64 {
    1.0 / f64::from(CC_MAX)
}

/// One control's pickup state.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct Control {
    /// Whether this control currently has the parameter.
    engaged: bool,
    /// The last position seen while still armed, for crossing detection.
    last_armed: Option<f64>,
}

/// Pickup state for every CC.
///
/// A flat array rather than a map: there are 128 possible CCs, the whole thing is small, and
/// lookup happens on every incoming control change.
pub struct Takeover {
    controls: [Control; 128],
}

impl Default for Takeover {
    fn default() -> Self {
        Self {
            controls: [Control::default(); 128],
        }
    }
}

impl Takeover {
    /// Whether this movement should take effect, updating the control's state.
    ///
    /// `knob` and `parameter` are both positions in `0.0..=1.0` under the same curve, which is
    /// what makes them comparable at all.
    pub fn accepts(&mut self, cc: u8, knob: f64, parameter: f64) -> bool {
        let control = &mut self.controls[usize::from(cc & 0x7f)];

        if control.engaged {
            return true;
        }

        // Near enough to count as having caught it.
        if (knob - parameter).abs() <= catch_width() {
            control.engaged = true;
            control.last_armed = None;
            return true;
        }

        // Or moved across it since the last time we looked. This is what lets a knob that starts
        // far away take control by being swept through the value, rather than needing to be
        // landed on it exactly.
        if let Some(previous) = control.last_armed
            && (previous < parameter) != (knob < parameter)
        {
            control.engaged = true;
            control.last_armed = None;
            return true;
        }

        control.last_armed = Some(knob);
        false
    }

    /// This control must catch up again before it takes effect.
    pub fn rearm(&mut self, cc: u8) {
        self.controls[usize::from(cc & 0x7f)] = Control::default();
    }

    pub fn rearm_all(&mut self) {
        self.controls = [Control::default(); 128];
    }

    #[cfg(test)]
    fn engaged(&self, cc: u8) -> bool {
        self.controls[usize::from(cc & 0x7f)].engaged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_knob_away_from_the_parameter_does_not_move_it() {
        let mut takeover = Takeover::default();
        assert!(
            !takeover.accepts(74, 0.2, 0.8),
            "a knob at 20% must not yank a parameter sitting at 80%"
        );
    }

    #[test]
    fn a_knob_takes_control_once_it_reaches_the_value() {
        let mut takeover = Takeover::default();
        assert!(!takeover.accepts(74, 0.2, 0.8));
        assert!(takeover.accepts(74, 0.8, 0.8));
    }

    #[test]
    fn a_knob_swept_through_the_value_takes_control_without_landing_on_it() {
        let mut takeover = Takeover::default();
        assert!(!takeover.accepts(74, 0.2, 0.5), "below");
        assert!(
            takeover.accepts(74, 0.7, 0.5),
            "swept past the value in one step"
        );
    }

    #[test]
    fn control_is_kept_once_taken() {
        let mut takeover = Takeover::default();
        assert!(takeover.accepts(74, 0.5, 0.5));
        // The parameter now follows the knob, so it is no longer near the old value.
        assert!(takeover.accepts(74, 0.9, 0.5));
        assert!(takeover.accepts(74, 0.1, 0.9));
    }

    #[test]
    fn two_controls_on_one_parameter_each_earn_it_separately() {
        let mut takeover = Takeover::default();
        assert!(takeover.accepts(74, 0.5, 0.5), "the fixed knob catches it");
        assert!(
            !takeover.accepts(102, 0.1, 0.5),
            "a bank slot on the same parameter must earn it in its own right"
        );
    }

    #[test]
    fn rearming_makes_a_knob_catch_up_again() {
        let mut takeover = Takeover::default();
        assert!(takeover.accepts(74, 0.5, 0.5));
        takeover.rearm(74);
        assert!(
            !takeover.accepts(74, 0.5, 0.9),
            "the parameter moved elsewhere, so the knob must catch up"
        );
    }

    #[test]
    fn rearm_all_leaves_nothing_engaged() {
        let mut takeover = Takeover::default();
        takeover.accepts(74, 0.5, 0.5);
        takeover.accepts(102, 0.3, 0.3);
        takeover.rearm_all();
        assert!(!takeover.engaged(74));
        assert!(!takeover.engaged(102));
    }

    #[test]
    fn a_knob_already_where_the_parameter_is_works_on_the_first_turn() {
        // The common case after a knob has been driving a parameter and nothing else touched it.
        let mut takeover = Takeover::default();
        assert!(takeover.accepts(74, 0.5, 0.5 + catch_width() / 2.0));
    }
}

//! How a controller's 0..=127 becomes a parameter value, and back again.
//!
//! The "and back again" is not symmetry for its own sake: pickup (see [`super::takeover`]) has to
//! compare where the knob is with where the parameter is, and those two only compare if the same
//! curve maps both ways.
//!
//! # Why this module exists, and when it does nothing
//!
//! **CLAP carries no curve hint.** `clap_param_info` has stepped/periodic/hidden/readonly/bypass
//! flags and nothing about skew, so a host mapping a controller linearly across a plain `min..max`
//! gets 128 evenly spaced *values* — which for a frequency or a time is not 128 evenly spaced
//! *musical steps*. Over 20 Hz–20 kHz that puts the first step near 176 Hz and spends more than
//! half the travel above 10 kHz.
//!
//! **But nice-plug plugins do not report plain ranges.** They report `min_value = 0.0` and
//! `max_value = step_count.unwrap_or(1)`
//! (`src/wrapper/clap/wrapper.rs:3760-3764` in the nice-plug fork, mxm-audio/nice-plug; it was
//! `vendor/nice-plug/…:3455-3459` in the monorepo), so a continuous parameter reaches
//! the host as `0.0..=1.0` with the plugin's own skew applied inside. Mapping linearly across that
//! **inherits the plugin's curve**, which is exactly what should happen — and a host-side log
//! curve on top would double-apply it.
//!
//! Both are handled by one rule rather than a special case: [`to_value`] falls back to linear
//! whenever a log range starts at zero, which is precisely the normalised case. So every MXM
//! plugin gets its own curve, and a third-party plugin reporting real hertz gets the layout's.

use super::schema::Curve;

/// The highest value a 7-bit controller sends.
pub const CC_MAX: u8 = 127;

/// Where a controller sits, `0.0..=1.0`.
pub fn position_of(cc_value: u8) -> f64 {
    f64::from(cc_value.min(CC_MAX)) / f64::from(CC_MAX)
}

/// What a parameter must look like for a controller to drive it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Range {
    pub min: f64,
    pub max: f64,
    /// From the `params` extension. A stepped parameter moves in whole steps or not at all.
    pub is_stepped: bool,
}

impl Range {
    pub fn new(min: f64, max: f64, is_stepped: bool) -> Self {
        Self {
            min,
            max,
            is_stepped,
        }
    }

    /// A range a controller cannot meaningfully drive: empty, inverted, or carrying a NaN.
    ///
    /// Written with `matches!` on the ordering rather than `!(max > min)` so the NaN case is
    /// explicit — a NaN range must be refused, not mapped.
    fn is_degenerate(&self) -> bool {
        !matches!(
            self.max.partial_cmp(&self.min),
            Some(std::cmp::Ordering::Greater)
        )
    }

    fn centre(&self) -> f64 {
        (self.min + self.max) / 2.0
    }
}

/// Turns a controller position into a parameter value.
pub fn to_value(curve: Curve, range: Range, position: f64) -> f64 {
    if range.is_degenerate() {
        return range.min;
    }
    let t = position.clamp(0.0, 1.0);

    // A stepped parameter is stepped whatever the layout says: the plugin's own step count is the
    // authority, and landing between two steps would either be rounded by the plugin anyway or
    // rejected.
    if range.is_stepped || curve == Curve::Stepped {
        let steps = (range.max - range.min).round().max(1.0);
        return range.min + (t * steps).round();
    }

    match curve {
        Curve::Linear => range.min + t * (range.max - range.min),

        // Even in ratio, which is what makes equal knob movements equal musical intervals. Only
        // defined for a strictly positive range; anything else falls back rather than producing a
        // NaN that would reach the plugin.
        Curve::Log if range.min > 0.0 => range.min * (range.max / range.min).powf(t),
        Curve::Log => range.min + t * (range.max - range.min),

        // Linear, except that the centre detent is exact. A tune knob that cannot be put back to
        // zero by hand is a knob you stop using.
        Curve::Bipolar => {
            if (t - CENTRE_POSITION).abs() < detent_width() {
                range.centre()
            } else {
                range.min + t * (range.max - range.min)
            }
        }

        Curve::Stepped => unreachable!("handled above"),
    }
}

/// Turns a parameter value back into the controller position that would produce it.
pub fn to_position(curve: Curve, range: Range, value: f64) -> f64 {
    if range.is_degenerate() {
        return 0.0;
    }
    let value = value.clamp(range.min, range.max);

    if range.is_stepped || curve == Curve::Stepped {
        let steps = (range.max - range.min).round().max(1.0);
        return ((value - range.min) / steps).clamp(0.0, 1.0);
    }

    match curve {
        Curve::Log if range.min > 0.0 && value > 0.0 => {
            ((value / range.min).ln() / (range.max / range.min).ln()).clamp(0.0, 1.0)
        }
        _ => ((value - range.min) / (range.max - range.min)).clamp(0.0, 1.0),
    }
}

/// Where the centre of a bipolar control is, in position.
///
/// **64, not 0.5.** A 7-bit controller has 128 values over a range whose true midpoint is 63.5, so
/// there is no exact centre and 63 and 64 sit equidistant from 0.5. Convention puts the centre at
/// 64, and picking it explicitly is what lets the detent catch one value rather than two.
const CENTRE_POSITION: f64 = 64.0 / CC_MAX as f64;

/// How wide the bipolar centre detent is, in position.
///
/// A quarter of a step either side: wide enough to catch 64 exactly, narrow enough that its
/// neighbours keep their own values.
fn detent_width() -> f64 {
    0.25 / f64::from(CC_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mxm-mono-01's cutoff, the parameter that motivated the whole module.
    const CUTOFF: Range = Range {
        min: 20.0,
        max: 20_000.0,
        is_stepped: false,
    };

    #[test]
    fn a_linear_cutoff_would_be_musically_useless() {
        // Not a test of our code — a test of the premise. Half travel on a linear map lands at
        // 10 kHz, which is most of the way through the audible range with 64 steps left over.
        let half = to_value(Curve::Linear, CUTOFF, 0.5);
        assert!(half > 9_000.0, "linear half-travel was {half} Hz");
    }

    #[test]
    fn a_log_cutoff_puts_half_travel_somewhere_a_player_would_want_it() {
        // The geometric mean of 20 and 20000 is 632 Hz — around the middle of a filter sweep,
        // which is where the middle of the knob belongs.
        let half = to_value(Curve::Log, CUTOFF, 0.5);
        assert!(
            (400.0..1_000.0).contains(&half),
            "log half-travel was {half} Hz"
        );
    }

    #[test]
    fn a_log_curve_gives_the_bottom_of_the_range_real_resolution() {
        // The complaint about 7 bits is really a complaint about linear. Under a log curve the
        // first step is a musical hair, not a 176 Hz jump.
        let first = to_value(Curve::Log, CUTOFF, position_of(1));
        assert!(first < 25.0, "first step landed at {first} Hz");

        let linear_first = to_value(Curve::Linear, CUTOFF, position_of(1));
        assert!(linear_first > 150.0, "sanity: linear first step");
    }

    #[test]
    fn the_ends_of_the_range_are_exact_under_every_curve() {
        for curve in [Curve::Linear, Curve::Log, Curve::Bipolar] {
            assert_eq!(to_value(curve, CUTOFF, 0.0), 20.0, "{curve:?} at zero");
            let top = to_value(curve, CUTOFF, 1.0);
            assert!((top - 20_000.0).abs() < 1e-6, "{curve:?} at full: {top}");
        }
    }

    #[test]
    fn a_bipolar_knob_can_be_put_back_to_the_centre_by_hand() {
        let tune = Range::new(-12.0, 12.0, false);
        assert_eq!(to_value(Curve::Bipolar, tune, position_of(64)), 0.0);
    }

    #[test]
    fn the_detent_catches_only_the_centre_value() {
        let tune = Range::new(-12.0, 12.0, false);
        assert_ne!(to_value(Curve::Bipolar, tune, position_of(63)), 0.0);
        assert_ne!(to_value(Curve::Bipolar, tune, position_of(65)), 0.0);
    }

    #[test]
    fn a_stepped_parameter_lands_on_whole_steps() {
        let range = Range::new(0.0, 3.0, true);
        for cc in 0..=CC_MAX {
            let value = to_value(Curve::Linear, range, position_of(cc));
            assert_eq!(value, value.round(), "cc {cc} landed between steps");
            assert!((0.0..=3.0).contains(&value));
        }
    }

    #[test]
    fn every_step_of_a_stepped_parameter_is_reachable() {
        let range = Range::new(0.0, 3.0, true);
        let reached: Vec<f64> = (0..=CC_MAX)
            .map(|cc| to_value(Curve::Stepped, range, position_of(cc)))
            .collect();
        for step in 0..=3 {
            assert!(
                reached.contains(&f64::from(step)),
                "step {step} was unreachable"
            );
        }
    }

    #[test]
    fn a_value_round_trips_to_the_position_that_produced_it() {
        for curve in [Curve::Linear, Curve::Log] {
            for cc in [0u8, 1, 32, 64, 100, 127] {
                let position = position_of(cc);
                let value = to_value(curve, CUTOFF, position);
                let back = to_position(curve, CUTOFF, value);
                assert!(
                    (back - position).abs() < 1e-9,
                    "{curve:?} cc {cc}: {position} -> {value} -> {back}"
                );
            }
        }
    }

    #[test]
    fn a_degenerate_range_never_produces_a_nan() {
        let empty = Range::new(5.0, 5.0, false);
        assert_eq!(to_value(Curve::Log, empty, 0.5), 5.0);
        assert_eq!(to_position(Curve::Log, empty, 5.0), 0.0);
    }

    #[test]
    fn a_log_curve_over_a_range_reaching_zero_falls_back_rather_than_producing_a_nan() {
        // Envelope times legitimately start at zero, and log of zero is not a number the plugin
        // should ever be handed.
        let times = Range::new(0.0, 10.0, false);
        for cc in 0..=CC_MAX {
            let value = to_value(Curve::Log, times, position_of(cc));
            assert!(value.is_finite(), "cc {cc} produced {value}");
        }
    }
}

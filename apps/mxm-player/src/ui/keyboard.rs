//! The computer keyboard as a piano, and the on-screen keyboard along the bottom of the window.
//!
//! ```text
//!     black:    w   e       t   y   u       o   p
//!     white:  a   s   d   f   g   h   j   k   l   ;   '
//!     note:   C   D   E   F   G   A   B   C   D   E   F
//! ```
//!
//! `d`→`f` and `j`→`k` have no key between them, matching E–F and B–C.
//!
//! This matters for testing as much as playing: legato phrasing and glide are what mxm-mono-01's
//! note stack exists for, and a mouse cannot exercise them.

use egui::Key;

/// The lowest octave the keyboard can be shifted to, and the highest.
pub const MIN_OCTAVE: i32 = -2;
pub const MAX_OCTAVE: i32 = 7;

/// How wide one white key is, in points.
///
/// A **fixed** width, deliberately: a wider window shows more of the keyboard rather than
/// stretching the same keys, which is what a longer keyboard means everywhere else.
pub const WHITE_KEY_WIDTH: f32 = 26.0;

/// The fewest white keys worth drawing, however narrow the window gets.
pub const MIN_WHITE_KEYS: usize = 7;

/// How many white keys fit in `width` points.
pub fn white_key_count(width: f32) -> usize {
    ((width / WHITE_KEY_WIDTH).floor() as usize).max(MIN_WHITE_KEYS)
}

/// Semitone offsets from the base C, in the layout order above.
const LAYOUT: &[(Key, i32)] = &[
    (Key::A, 0),
    (Key::W, 1),
    (Key::S, 2),
    (Key::E, 3),
    (Key::D, 4),
    (Key::F, 5),
    (Key::T, 6),
    (Key::G, 7),
    (Key::Y, 8),
    (Key::H, 9),
    (Key::U, 10),
    (Key::J, 11),
    (Key::K, 12),
    (Key::O, 13),
    (Key::L, 14),
    (Key::P, 15),
    (Key::Semicolon, 16),
    (Key::Quote, 17),
];

/// Both octave-shift pairs, both active.
const OCTAVE_DOWN: &[Key] = &[Key::Z, Key::Minus];
const OCTAVE_UP: &[Key] = &[Key::X, Key::Plus, Key::Equals];

/// What a key press or release means.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum KeyAction {
    NoteOn(u8),
    NoteOff(u8),
    OctaveDown,
    OctaveUp,
}

/// Maps a computer key to a note, given the current octave.
///
/// `None` for a key with no note, and — importantly — for anything out of MIDI's range once the
/// octave shift is applied.
pub fn key_to_note(key: Key, octave: i32) -> Option<u8> {
    let offset = LAYOUT.iter().find(|(k, _)| *k == key)?.1;
    // Octave 3 is middle C at MIDI 60, which is what every other tool on this machine calls C3.
    let note = (octave + 2) * 12 + offset;
    u8::try_from(note).ok().filter(|n| *n <= 127)
}

pub fn is_octave_down(key: Key) -> bool {
    OCTAVE_DOWN.contains(&key)
}

pub fn is_octave_up(key: Key) -> bool {
    OCTAVE_UP.contains(&key)
}

/// Translates one egui key event into an action.
///
/// **Auto-repeat is suppressed.** Holding a key down makes the platform emit repeated `pressed`
/// events with only one release at the end; egui surfaces this as `Event::Key::repeat`
/// specifically so integrations can ignore it. Acting on repeats would open a new press — and a
/// new voice ID — for every repeat while exactly one release ever arrives, leaving voices
/// outstanding and the counted sustain state permanently non-zero. So note keys and both
/// octave-shift pairs act **only on an up-to-down transition**.
pub fn translate(key: Key, pressed: bool, repeat: bool, octave: i32) -> Option<KeyAction> {
    if repeat {
        return None;
    }

    if pressed {
        if is_octave_down(key) {
            return Some(KeyAction::OctaveDown);
        }
        if is_octave_up(key) {
            return Some(KeyAction::OctaveUp);
        }
    }

    let note = key_to_note(key, octave)?;
    Some(if pressed {
        KeyAction::NoteOn(note)
    } else {
        KeyAction::NoteOff(note)
    })
}

/// Whether a MIDI note number is a black key.
pub fn is_black(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

/// The white keys of the on-screen keyboard, starting at the given octave.
pub fn white_keys(octave: i32, count: usize) -> Vec<u8> {
    let all: Vec<u8> = (0u8..=127).filter(|n| !is_black(*n)).collect();
    let base = ((octave + 2) * 12).clamp(0, 127) as u8;

    let start = all.iter().position(|n| *n >= base).unwrap_or(0);
    // If the run would overrun the top of MIDI's range, slide down rather than showing a short
    // keyboard: a wider window should always mean more keys, not a gap on the right.
    let start = start.min(all.len().saturating_sub(count));

    all[start..(start + count).min(all.len())].to_vec()
}

/// The name of a note that starts an octave, for labelling the keyboard.
///
/// Only the Cs are named: a label on every key is noise, and the Cs are what a player counts
/// from. `C3` is middle C at MIDI 60, matching the octave numbering the shift keys use.
pub fn octave_label(note: u8) -> Option<String> {
    note.is_multiple_of(12)
        .then(|| format!("C{}", i32::from(note) / 12 - 2))
}

/// How tall a black key is, as a fraction of a white key.
pub const BLACK_KEY_HEIGHT: f32 = 0.62;

/// How wide a black key is, as a fraction of a white key.
pub const BLACK_KEY_WIDTH: f32 = 0.62;

/// A black key's note and where it sits, as a fraction of the white-key width from the left edge
/// of the keyboard.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BlackKey {
    pub note: u8,
    /// Centre position, in white-key widths from the left edge.
    pub centre: f32,
}

/// The black keys belonging to a run of white keys.
///
/// A black key sits *between* two white keys, so it is placed on the boundary rather than in a
/// slot of its own — which is why the keyboard cannot be drawn as one flat row of rectangles.
/// There is no black key after E or B, which is what gives a piano its groups of two and three.
pub fn black_keys(octave: i32, count: usize) -> Vec<BlackKey> {
    let whites = white_keys(octave, count);
    let mut keys = Vec::new();

    for (index, white) in whites.iter().enumerate() {
        // C, D, F, G and A each have a sharp; E and B do not.
        if !matches!(white % 12, 0 | 2 | 5 | 7 | 9) {
            continue;
        }
        // Only if the next white key is actually on the keyboard, so the run does not end with a
        // black key hanging off the right edge.
        if index + 1 >= whites.len() {
            continue;
        }
        let Some(note) = white.checked_add(1).filter(|n| *n <= 127) else {
            continue;
        };
        keys.push(BlackKey {
            note,
            centre: index as f32 + 1.0,
        });
    }

    keys
}

/// Vertical position within a key sets velocity: the bottom of the key is loudest, as on a
/// weighted keyboard where a deeper strike is harder.
pub fn velocity_from_position(fraction_from_top: f32) -> f64 {
    let clamped = fraction_from_top.clamp(0.0, 1.0);
    // Never zero: velocity zero is a note-off by convention, which is not what a click means.
    (0.15 + 0.85 * f64::from(clamped)).clamp(0.01, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_row_is_the_white_keys_in_order() {
        let notes: Vec<u8> = [
            Key::A,
            Key::S,
            Key::D,
            Key::F,
            Key::G,
            Key::H,
            Key::J,
            Key::K,
            Key::L,
            Key::Semicolon,
            Key::Quote,
        ]
        .into_iter()
        .filter_map(|k| key_to_note(k, 3))
        .collect();

        assert_eq!(notes, vec![60, 62, 64, 65, 67, 69, 71, 72, 74, 76, 77]);
        assert!(notes.iter().all(|n| !is_black(*n)));
    }

    #[test]
    fn the_row_above_carries_the_black_keys_in_their_correct_positions() {
        // ...and there is deliberately no key between d and f, or between j and k.
        for key in [Key::W, Key::E, Key::T, Key::Y, Key::U, Key::O, Key::P] {
            let note = key_to_note(key, 3).expect("a black key");
            assert!(is_black(note), "{key:?} should be a black key, got {note}");
        }
        assert_eq!(key_to_note(Key::R, 3), None, "there is no E sharp");
        assert_eq!(key_to_note(Key::I, 3), None, "there is no B sharp");
    }

    #[test]
    fn auto_repeat_never_becomes_a_second_press() {
        // One press, many repeats, one release: exactly one note-on and one note-off.
        assert_eq!(
            translate(Key::A, true, false, 3),
            Some(KeyAction::NoteOn(60))
        );
        for _ in 0..20 {
            assert_eq!(
                translate(Key::A, true, true, 3),
                None,
                "a repeat must not open a new press"
            );
        }
        assert_eq!(
            translate(Key::A, false, false, 3),
            Some(KeyAction::NoteOff(60))
        );
    }

    #[test]
    fn octave_shift_also_ignores_auto_repeat() {
        assert_eq!(
            translate(Key::Z, true, false, 3),
            Some(KeyAction::OctaveDown)
        );
        assert_eq!(translate(Key::Z, true, true, 3), None);
        assert_eq!(translate(Key::X, true, false, 3), Some(KeyAction::OctaveUp));
        assert_eq!(
            translate(Key::Minus, true, false, 3),
            Some(KeyAction::OctaveDown)
        );
        assert_eq!(
            translate(Key::Plus, true, false, 3),
            Some(KeyAction::OctaveUp)
        );
    }

    #[test]
    fn notes_outside_midis_range_are_refused_rather_than_wrapped() {
        assert_eq!(key_to_note(Key::A, MIN_OCTAVE), Some(0));
        assert_eq!(
            key_to_note(Key::Quote, MAX_OCTAVE + 2),
            None,
            "shifting past 127 must produce no note at all"
        );
    }

    #[test]
    fn a_click_never_produces_velocity_zero() {
        // Velocity zero is a note-off by convention; a click at the very top of a key is not one.
        assert!(velocity_from_position(0.0) > 0.0);
        assert!(velocity_from_position(-1.0) > 0.0);
        assert!((velocity_from_position(1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_keyboard_has_black_keys_in_groups_of_two_and_three() {
        let blacks = black_keys(3, 21);
        assert!(
            blacks.iter().all(|k| is_black(k.note)),
            "every black key must actually be a black note"
        );

        // One octave from C is C#, D#, F#, G#, A# — two, then three.
        let first_octave: Vec<u8> = blacks
            .iter()
            .map(|k| k.note)
            .filter(|n| (60..72).contains(n))
            .collect();
        assert_eq!(first_octave, vec![61, 63, 66, 68, 70]);
    }

    #[test]
    fn no_black_key_sits_after_e_or_b() {
        // The gaps are what make a keyboard readable at a glance.
        let blacks = black_keys(3, 21);
        for gap in [65u8, 72] {
            assert!(
                !blacks.iter().any(|k| k.note == gap),
                "there is no black key at {gap}"
            );
        }
    }

    #[test]
    fn black_keys_sit_on_the_boundary_between_two_white_keys() {
        let blacks = black_keys(3, 21);
        let whites = white_keys(3, 21);
        for key in &blacks {
            assert!(
                key.centre > 0.0 && key.centre < whites.len() as f32,
                "a black key must stay inside the keyboard, got {}",
                key.centre
            );
            // Centres land exactly on a white-key boundary, never in the middle of one.
            assert_eq!(key.centre.fract(), 0.0);
        }
    }

    #[test]
    fn a_wider_keyboard_shows_more_keys_rather_than_wider_ones() {
        // The whole point of a fixed key width.
        let narrow = white_key_count(400.0);
        let wide = white_key_count(1200.0);
        assert!(wide > narrow, "{wide} should be more than {narrow}");

        // From the bottom there is room for every key asked for.
        assert_eq!(white_keys(MIN_OCTAVE, wide).len(), wide);
        assert!(white_keys(MIN_OCTAVE, wide).len() > white_keys(MIN_OCTAVE, narrow).len());

        // ...and a very narrow window still shows a playable octave rather than nothing.
        assert_eq!(white_key_count(0.0), MIN_WHITE_KEYS);
    }

    #[test]
    fn a_high_octave_slides_down_rather_than_leaving_a_gap() {
        // Asking for a wide keyboard near the top of the range would overrun 127. Sliding down
        // keeps it full, which is what "wider window, more keys" has to mean everywhere.
        let asked = 40;
        let keys = white_keys(MAX_OCTAVE, asked);
        assert_eq!(keys.len(), asked);
        assert!(keys.iter().all(|n| *n <= 127));
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "strictly ascending");
        assert!(keys.iter().all(|n| !is_black(*n)));
    }

    #[test]
    fn asking_for_more_keys_than_midi_has_gives_the_whole_range() {
        // 75 white keys exist in 0..=127; beyond that there is nothing left to show.
        let keys = white_keys(MIN_OCTAVE, 500);
        assert_eq!(keys.len(), 75);
        assert_eq!(keys.first(), Some(&0));
        assert_eq!(keys.last(), Some(&127));
    }

    #[test]
    fn only_the_cs_are_labelled_and_middle_c_is_c3() {
        assert_eq!(octave_label(60).as_deref(), Some("C3"));
        assert_eq!(octave_label(72).as_deref(), Some("C4"));
        assert_eq!(octave_label(0).as_deref(), Some("C-2"));
        assert_eq!(octave_label(62), None, "D is not labelled");
        assert_eq!(octave_label(61), None, "a black key is not labelled");

        // The label agrees with what the computer keyboard plays at that octave.
        for octave in MIN_OCTAVE..=MAX_OCTAVE {
            if let Some(note) = key_to_note(Key::A, octave) {
                assert_eq!(octave_label(note).as_deref(), Some(&*format!("C{octave}")));
            }
        }
    }

    #[test]
    fn the_on_screen_keyboard_shows_white_keys_only() {
        let keys = white_keys(3, 21);
        assert_eq!(keys.len(), 21);
        assert!(keys.iter().all(|n| !is_black(*n)));
        assert_eq!(keys[0], 60);
    }
}

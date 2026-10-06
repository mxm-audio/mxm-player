//! The conformance sweep: **every act the interface offers has a verb, and every verb works.**
//!
//! `plans/plan-cli-conformance.md`, implemented. Two directions in one binary so they cannot pass
//! separately: completeness walks the accessibility tree across interface states and requires
//! every interactive widget — and every [`GestureKind`] — to map to a `COVERAGE` row or an argued
//! `EXEMPT` row; honesty executes every row's verb over a real socket and asserts its oracle.

use mxm_player_harness::app_harness;

use app_harness::AppHarness;
use egui_kittest::kittest::{NodeT, Queryable, by};
use mxm_player::state::PlayerState;
use mxm_player::ui::GestureKind;
use std::path::{Path, PathBuf};

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

// ------------------------------------------------------------------------------------------------
// The tables
// ------------------------------------------------------------------------------------------------

/// What proves a verb did its work.
enum Oracle {
    /// This dot-path into the dump changes when the command runs.
    DumpField(&'static str),
    /// The reply names a file that exists and is non-trivial.
    Artifact,
    /// The reply contains this, and the act's effect is observed by a later row.
    Reply(&'static str),
    /// The reply is an error containing this — a guard, exercised.
    Error(&'static str),
}

/// One covered act: what in the interface it is, the concrete command that drives it, the proof.
struct Row {
    /// A widget-label prefix ("Step ", "Copy"), or a gesture kind's name ("ClickStep").
    act: &'static str,
    command: &'static str,
    oracle: Oracle,
}

/// The coverage table. **Order matters for prefix matching**: more specific prefixes first.
const COVERAGE: &[Row] = &[
    // --- gestures (kind names) — each maps to the verb reaching the same funnel ---------------
    Row {
        act: "ClickStep",
        command: "select 2",
        oracle: Oracle::DumpField("state.sequencer.selected_step"),
    },
    Row {
        act: "ShiftClickStep",
        command: "select 2-4",
        oracle: Oracle::DumpField("state.sequencer.selected_steps"),
    },
    Row {
        act: "CtrlClickStep",
        command: "select 1,3",
        oracle: Oracle::DumpField("state.sequencer.selected_steps"),
    },
    Row {
        act: "EscapeKey",
        command: "deselect",
        oracle: Oracle::DumpField("state.sequencer.selected_step"),
    },
    Row {
        act: "SpaceKey",
        command: "play",
        oracle: Oracle::DumpField("state.sequencer.transport"),
    },
    Row {
        act: "CopyShortcut",
        command: "copysteps",
        oracle: Oracle::Reply("copied"),
    },
    Row {
        act: "CutShortcut",
        command: "cutsteps",
        oracle: Oracle::Reply("cut"),
    },
    Row {
        act: "PasteShortcut",
        command: "paste",
        oracle: Oracle::Reply("pasted"),
    },
    Row {
        act: "ResetParam",
        command: "reset cutoff",
        oracle: Oracle::Reply("reset"),
    },
    Row {
        act: "PlayNote",
        command: "note C3",
        oracle: Oracle::Reply("note"),
    },
    Row {
        act: "ReleaseNote",
        command: "off C3",
        oracle: Oracle::Reply("off"),
    },
    Row {
        act: "OctaveShift",
        command: "octave 4",
        oracle: Oracle::DumpField("state.keyboard.octave"),
    },
    // --- widgets (label prefixes), filled from what the walk itself surfaced ------------------
    Row {
        act: "Step ",
        command: "toggle 1 C3",
        oracle: Oracle::DumpField("state.sequencer.steps"),
    },
    Row {
        act: "Bar ",
        command: "bar 2",
        oracle: Oracle::DumpField("state.sequencer.selected_bar"),
    },
    Row {
        act: "Copy",
        command: "copybar",
        oracle: Oracle::Reply("copied"),
    },
    Row {
        act: "Paste",
        command: "paste",
        oracle: Oracle::Reply("pasted"),
    },
    Row {
        act: "Clear bar",
        command: "clearbar",
        oracle: Oracle::Reply("cleared"),
    },
    Row {
        act: "Clear pattern",
        command: "clearpattern",
        oracle: Oracle::Reply("cleared"),
    },
    // The whole-pattern Clear button, and Random beside it — which fills the shown bar.
    Row {
        act: "Clear",
        command: "clear",
        oracle: Oracle::Reply("cleared"),
    },
    Row {
        act: "Random",
        command: "random",
        oracle: Oracle::Reply("randomised"),
    },
    // The loop toggles speak their span ("1 Bar", "Bar 1-8", "All bars"); the bare "Bar" and
    // "Pattern" rows are the tools toggle beside Copy/Paste/Clear.
    Row {
        act: "Loop",
        command: "loop pattern",
        oracle: Oracle::DumpField("state.sequencer.loop_scope"),
    },
    Row {
        act: "1 Bar",
        command: "loop bar",
        oracle: Oracle::DumpField("state.sequencer.loop_scope"),
    },
    Row {
        act: "Bar 1-8",
        command: "loop pattern",
        oracle: Oracle::DumpField("state.sequencer.loop_scope"),
    },
    Row {
        act: "All bars",
        command: "loop all",
        oracle: Oracle::DumpField("state.sequencer.loop_scope"),
    },
    Row {
        act: "Bar",
        command: "copybar",
        oracle: Oracle::Reply("copied"),
    },
    Row {
        act: "Pattern",
        command: "copypattern",
        oracle: Oracle::Reply("copied"),
    },
    // The pattern steppers and the shape counters: navigation and shape, as verbs.
    Row {
        act: "\u{2039}",
        command: "bar 1",
        oracle: Oracle::Reply("showing"),
    },
    Row {
        act: "\u{203a}",
        command: "bar 9",
        oracle: Oracle::Reply("showing"),
    },
    // Four, not two: rows above this one grow the sequence — Random fills the *shown* bar and
    // materialises it — so a target the table shares with them proves nothing when it is already
    // there. A length no earlier row can reach always moves.
    Row {
        act: "+",
        command: "bars 4",
        oracle: Oracle::DumpField("state.sequencer.bars"),
    },
    Row {
        act: "\u{2212}",
        command: "bars 1",
        oracle: Oracle::DumpField("state.sequencer.bars"),
    },
    Row {
        act: "-",
        command: "steps 12",
        oracle: Oracle::DumpField("state.sequencer.steps_per_bar"),
    },
    // Transport, performance and recovery controls.
    Row {
        act: "\u{25b6}",
        command: "stop",
        oracle: Oracle::Reply("stopped"),
    },
    Row {
        act: "\u{23f9}",
        command: "play",
        oracle: Oracle::Reply("playing"),
    },
    Row {
        act: "Panic",
        command: "panic",
        oracle: Oracle::Reply("all sound off"),
    },
    Row {
        act: "Sustain",
        command: "sustain on",
        oracle: Oracle::Reply("sustain"),
    },
    Row {
        act: "Bend",
        command: "bend 0.5",
        oracle: Oracle::Reply("bend"),
    },
    // **Export normalisation is a setting with an audible consequence**, so it answers over the
    // socket like every other one: peak-normalising an export is right for making a sample and
    // wrong for comparing two patches, because it cancels a pure level change exactly.
    Row {
        act: "Normalise",
        command: "normalise off",
        oracle: Oracle::Reply("own gain"),
    },
    Row {
        act: "Mod",
        command: "cc 1 64",
        oracle: Oracle::Reply("CC 1"),
    },
    Row {
        act: "Octave",
        command: "octave 2",
        oracle: Oracle::DumpField("state.keyboard.octave"),
    },
    // Sequence files: the name field is `save`'s argument surface; Load gets its own verb.
    Row {
        act: "Save",
        command: "save conformance-flow",
        oracle: Oracle::Artifact,
    },
    Row {
        act: "Name",
        command: "savemid conformance-flow",
        oracle: Oracle::Artifact,
    },
    Row {
        act: "Load",
        command: "loadseq conformance-flow",
        oracle: Oracle::Reply("loaded"),
    },
    // The plugin menu's Rescan, and the picker itself is covered dynamically by plugin names.
    Row {
        act: "Rescan",
        command: "rescan",
        oracle: Oracle::Reply("rescanned"),
    },
    // The collapsible panels.
    Row {
        act: "Settings",
        command: "panel settings open",
        oracle: Oracle::Reply("settings panel"),
    },
    Row {
        act: "Parameters",
        command: "panel parameters open",
        oracle: Oracle::Reply("parameters panel"),
    },
    // The app bar's theme control: the button, and the three choices inside it. The menu is shut
    // in every state the walk enters, so only the button is met there — the choices are listed
    // because they are acts of the interface, and the honesty run drives all four either way.
    Row {
        act: "Theme",
        command: "theme system",
        oracle: Oracle::Reply("theme system"),
    },
    Row {
        act: "Light",
        command: "theme light",
        oracle: Oracle::Reply("theme light"),
    },
    Row {
        act: "Dark",
        command: "theme dark",
        oracle: Oracle::Reply("theme dark"),
    },
    Row {
        act: "System",
        command: "theme system",
        oracle: Oracle::Reply("theme system"),
    },
    // Tempo is a dump field driven by its verb.
    Row {
        act: "Tempo",
        command: "tempo 118",
        oracle: Oracle::DumpField("state.sequencer.tempo"),
    },
    // --- guards: every refusal is part of the surface, exercised as rows ----------------------
    // --- the device rail: the chain's acts, driven on the reference effect fixtures ------------
    // The strips' labels are short and shared between strips, as the step buttons' are: one row
    // per label, and the honesty run drives each verb on a chain it builds here. `+` is also the
    // bars stepper's label; both verbs run, whichever row the prefix match lands on.
    Row {
        act: "+",
        command: "fx add dk.mxm.fixture.effect",
        oracle: Oracle::DumpField("state.fx"),
    },
    Row {
        act: "+",
        command: "fx add dk.mxm.fixture.effect-mono",
        oracle: Oracle::DumpField("state.fx"),
    },
    Row {
        act: ">",
        command: "fx move 1 2",
        oracle: Oracle::DumpField("state.fx"),
    },
    Row {
        act: "<",
        command: "fx move 2 1",
        oracle: Oracle::DumpField("state.fx"),
    },
    Row {
        act: "On",
        command: "fx off 1",
        oracle: Oracle::DumpField("state.fx"),
    },
    Row {
        act: "On",
        command: "fx on 1",
        oracle: Oracle::DumpField("state.fx"),
    },
    // The fixture has no interface, so the verb's refusal is what proves it reached the editor.
    Row {
        act: "Edit",
        command: "fx editor 1",
        oracle: Oracle::Error("no interface"),
    },
    Row {
        act: "×",
        command: "fx remove 2",
        oracle: Oracle::DumpField("state.fx"),
    },
    Row {
        act: "guard: no such effect",
        command: "fx remove 9",
        oracle: Oracle::Error("the chain has"),
    },
    Row {
        act: "guard: not an effect",
        command: "fx add dk.mxm.mxm-mono-01",
        oracle: Oracle::Error("no effect matches"),
    },
    Row {
        act: "guard: unknown verb",
        command: "frobnicate",
        oracle: Oracle::Error("commands:"),
    },
    Row {
        act: "guard: zero step",
        command: "toggle 0 C3",
        oracle: Oracle::Error("counted from 1"),
    },
    Row {
        act: "guard: bad note",
        command: "toggle 1 H9",
        oracle: Oracle::Error("not a note"),
    },
    Row {
        act: "guard: bad cc",
        command: "cc 200 1",
        oracle: Oracle::Error("0..=127"),
    },
    Row {
        act: "guard: bad scope",
        command: "loop sideways",
        oracle: Oracle::Error("loop takes"),
    },
    Row {
        act: "guard: zero bars",
        command: "bars 0",
        oracle: Oracle::Error("not a number of bars"),
    },
];

/// Interface acts with no verb, each with the argument for why. **Never a musical or diagnostic
/// act** — the owner's ruling: reachable acts get verbs, and only what the CLI honestly cannot
/// or should not reach is skipped.
const EXEMPT: &[(&str, &str)] = &[
    (
        "Main",
        "a parameter-panel tab: it filters what is painted and nothing else, so every parameter \n         stays writable from the CLI whatever tab is showing. The other tabs are named by the \n         plugin and cannot appear in a fixed table at all",
    ),
    (
        "Device",
        "audio environment, not music: the dump reports it, tests drive it through PlayerConfig",
    ),
    ("Sample rate", "audio environment, as above"),
    ("Buffer size", "audio environment, as above"),
    ("Refresh ports", "MIDI environment, as above"),
    ("Output", "MIDI environment, as above"),
    ("All ports", "MIDI environment, as above"),
    ("Keyboard thru", "MIDI environment, as above"),
    (
        "Open folder",
        "opens the OS file explorer - a convenience with no state of its own",
    ),
    (
        "Show editor",
        "opens the plugin's own window; a machine reaches the same state over the socket",
    ),
];

// ------------------------------------------------------------------------------------------------
// The completeness direction
// ------------------------------------------------------------------------------------------------

fn interactive_labels(harness: &mut AppHarness) -> Vec<String> {
    // Interactive roles matched by their Debug names: `accesskit::Role` is not re-exported by
    // the harness crates, and a type dependency is not worth a string comparison.
    const INTERACTIVE: &[&str] = &[
        "Button",
        "CheckBox",
        "ComboBox",
        "Slider",
        "SpinButton",
        "TextInput",
        "ListBoxOption",
        "ToggleButton",
        "RadioButton",
    ];
    let mut labels = Vec::new();
    for node in harness
        .harness
        .query_all(by().predicate(|n| INTERACTIVE.contains(&format!("{:?}", n.role()).as_str())))
    {
        if let Some(label) = node.accesskit_node().label() {
            labels.push(label.to_string());
        }
    }
    labels.sort();
    labels.dedup();
    labels
}

fn covered(label: &str, dump: &PlayerState) -> bool {
    if COVERAGE.iter().any(|row| label.starts_with(row.act)) {
        return true;
    }
    if EXEMPT.iter().any(|(prefix, _)| label.starts_with(prefix)) {
        return true;
    }
    // Every parameter's own control is covered by `set`/`reset`/`lock`, matched dynamically:
    // the parameter list is the plugin's, never pinned here.
    if let Some(plugin) = &dump.plugin
        && plugin.params.iter().any(|p| label.starts_with(&p.name))
    {
        return true;
    }
    // The plugin picker's button and rows carry plugin names - covered by `load`.
    if dump
        .found
        .iter()
        .any(|f| label.starts_with(&f.name) || label.starts_with(&f.id))
    {
        return true;
    }
    // MIDI port checkboxes carry port names - environment, same argument as the EXEMPT class.
    if dump
        .midi
        .available_inputs
        .iter()
        .chain(dump.midi.available_outputs.iter())
        .any(|port| label.starts_with(port))
    {
        return true;
    }
    false
}

#[test]
fn every_interactive_widget_has_a_verb_or_an_argued_exemption() {
    let Some(dir) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut harness = AppHarness::new("conformance-walk", vec![dir.clone()]);
    harness.run();

    // The no-plugin state, walked before anything loads: a fresh player must not offer an
    // unreachable act either. The dump is taken in the same state, so dynamic matching sees the
    // same (absent) plugin the walk saw.
    let mut uncovered_fresh: Vec<String> = Vec::new();
    {
        let dump = harness.state();
        for label in interactive_labels(&mut harness) {
            if !covered(&label, &dump) {
                uncovered_fresh.push(format!("[no plugin] {label}"));
            }
        }
    }

    harness
        .app()
        .load(dir.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    harness.run();

    // The states, because one rendered state hides conditional controls.
    let mut uncovered: Vec<String> = Vec::new();
    type Enter = Box<dyn Fn(&mut AppHarness)>;
    let states: Vec<(&str, Enter)> = vec![
        ("loaded", Box::new(|_| {})),
        ("step selected", Box::new(|h| h.app().select_step(0))),
        (
            "multi-selection",
            Box::new(|h| {
                h.app().select_step(0);
                h.app().shift_select_step(3);
            }),
        ),
        ("playing", Box::new(|h| h.app().play_from_start())),
        // The plugin picker popped open, so its menu items are walked too.
        (
            "plugin menu open",
            Box::new(|h| {
                let pos = h
                    .harness
                    .query_all(by().predicate(|n| n.label().is_some_and(|l| l.contains('⏷'))))
                    .next()
                    .map(|n| n.rect().center());
                if let Some(pos) = pos {
                    h.click_at(pos);
                }
            }),
        ),
        // Two effects in the chain, so every strip control exists: on/off, editor, both
        // directions of move (the first cannot go earlier, the last cannot go later) and remove.
        (
            "effects in the chain",
            Box::new(|h| {
                let Some(dir) = fixtures_dir() else {
                    eprintln!("no chain walked: run `cargo xtask fixtures --release` first");
                    return;
                };
                let bundle = dir.join("mxm-fixtures.clap");
                h.app()
                    .add_fx(bundle.clone(), "dk.mxm.fixture.effect".to_owned())
                    .expect("the stereo fixture effect loads");
                h.app()
                    .add_fx(bundle, "dk.mxm.fixture.effect-mono".to_owned())
                    .expect("the mono fixture effect loads");
            }),
        ),
    ];
    for (name, enter) in states {
        enter(&mut harness);
        harness.run();
        let dump = harness.state();
        for label in interactive_labels(&mut harness) {
            if !covered(&label, &dump) {
                uncovered.push(format!("[{name}] {label}"));
            }
        }
        harness.app().stop_sequencer();
        harness.app().deselect_step();
        harness.run();
    }
    uncovered.extend(uncovered_fresh);
    uncovered.sort();
    uncovered.dedup();
    assert!(
        uncovered.is_empty(),
        "widgets with no verb and no argued exemption:\n{}",
        uncovered.join("\n")
    );
}

#[test]
fn every_gesture_kind_has_a_coverage_row() {
    // `GestureKind::ALL` is macro-generated from the same token list as the enum, so this is the
    // whole inventory; a gesture without a row fails here by name.
    let missing: Vec<String> = GestureKind::ALL
        .iter()
        .filter(|kind| {
            let name = format!("{kind:?}");
            !COVERAGE.iter().any(|row| row.act == name)
        })
        .map(|kind| format!("{kind:?}"))
        .collect();
    assert!(
        missing.is_empty(),
        "gestures with no coverage row: {missing:?}"
    );
}

/// The rail is on screen with the parameter panel collapsed — the state the player is in beside
/// an editor, and the state a rail in the collapsible band vanished from. The sweep alone cannot
/// say this: a control that is not drawn is not uncovered, only absent.
#[test]
fn the_rail_stays_on_screen_with_the_parameter_panel_collapsed() {
    let (Some(dir), Some(fixtures)) = (app_harness::bundled_dir(), fixtures_dir()) else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` and `cargo xtask fixtures` first");
        return;
    };
    let mut harness = AppHarness::new("rail-collapsed", vec![dir.clone()]);
    harness.run();
    harness
        .app()
        .load(dir.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    let bundle = fixtures.join("mxm-fixtures.clap");
    harness
        .app()
        .add_fx(bundle.clone(), "dk.mxm.fixture.effect".to_owned())
        .expect("the stereo fixture effect loads");
    harness
        .app()
        .add_fx(bundle, "dk.mxm.fixture.effect-mono".to_owned())
        .expect("the mono fixture effect loads");
    harness.run();
    harness.harness.get_by_label("Parameters").click();
    harness.run();

    let count = |label: &str| harness.harness.query_all(by().label(label)).count();
    // Two of everything, because two effects are in the chain and the source has no strip:
    // its editor is the status bar's own button.
    for (label, expected) in [
        ("On", 2),
        ("<", 2),
        (">", 2),
        ("×", 2),
        ("Edit", 2),
        ("+", 1),
    ] {
        assert!(
            count(label) >= expected,
            "with the parameter panel collapsed, `{label}` should be on screen {expected} times              — the rail belongs to the sequencer band, not the collapsible one"
        );
    }
}

// ------------------------------------------------------------------------------------------------
// The honesty direction: every row's verb, over a real socket
// ------------------------------------------------------------------------------------------------

fn player_with_cli(name: &str) -> Option<(mxm_player::ui::PlayerApp, PathBuf)> {
    let bundled = app_harness::bundled_dir()?;
    let dir = std::env::temp_dir().join(format!("mxm-player-conformance-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    // The fixtures too, when staged: the rail's rows add the reference effect from them.
    let mut roots = vec![bundled.clone()];
    roots.extend(fixtures_dir());
    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(roots);
    let settings_path = config.settings_path.clone();
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.start_cli();
    Some((app, settings_path))
}

/// Where the staged fixture bundle lives, or `None` when `cargo xtask fixtures --release` has
/// not been run — the rail's rows are skipped then, with a line saying so.
fn fixtures_dir() -> Option<PathBuf> {
    let dir = app_harness::workspace_root()
        .join("target")
        .join("fixtures");
    dir.join("mxm-fixtures.clap").exists().then_some(dir)
}

fn command(app: &mut mxm_player::ui::PlayerApp, settings: &Path, line: &str) -> String {
    let settings = settings.to_path_buf();
    let line = line.to_owned();
    let client = std::thread::spawn(move || mxm_player::cli::run_command(&settings, &line));
    let started = std::time::Instant::now();
    loop {
        app.service();
        if client.is_finished() || started.elapsed() > std::time::Duration::from_secs(5) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    client.join().expect("client thread").expect("an answer")
}

fn field<'a>(dump: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    let mut node = dump;
    for part in path.split('.') {
        node = &node[part];
    }
    node
}

#[test]
fn every_coverage_row_works_over_the_socket() {
    let Some((mut app, settings)) = player_with_cli("honesty") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // Material for the verbs to act on: a note (so copy verbs have content), and a selection.
    assert!(command(&mut app, &settings, "toggle 1 C3").contains("ok"));

    let fixtures_staged = fixtures_dir().is_some();
    for row in COVERAGE {
        if row.command.starts_with("fx ") && !fixtures_staged && !row.act.starts_with("guard") {
            eprintln!(
                "skipped `{}`: run `cargo xtask fixtures --release` first",
                row.command
            );
            continue;
        }
        // Rows that need a selection first get one; the table stays declarative.
        if row.command.starts_with("copysteps")
            || row.command.starts_with("cutsteps")
            || row.command.starts_with("paste")
        {
            assert!(command(&mut app, &settings, "select 1").contains("ok"));
        }
        match &row.oracle {
            Oracle::DumpField(path) => {
                let before: serde_json::Value =
                    serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump");
                let answer = command(&mut app, &settings, row.command);
                assert!(answer.contains("ok"), "{}: {answer}", row.command);
                let after: serde_json::Value =
                    serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump");
                assert_ne!(
                    field(&before, path),
                    field(&after, path),
                    "{}: `{path}` did not move",
                    row.command
                );
            }
            Oracle::Artifact => {
                let answer = command(&mut app, &settings, row.command);
                assert!(answer.contains("ok"), "{}: {answer}", row.command);
                let path = answer
                    .split(char::is_whitespace)
                    .next_back()
                    .map(|s| s.trim_end_matches(['"', '}']).replace("\\\\", "\\"))
                    .expect("a path in the reply");
                let meta = std::fs::metadata(path.trim()).expect("the artifact exists");
                assert!(meta.len() > 16, "{}: artifact is trivial", row.command);
            }
            Oracle::Reply(pattern) => {
                let answer = command(&mut app, &settings, row.command);
                assert!(
                    answer.contains("ok") && answer.contains(pattern),
                    "{}: {answer}",
                    row.command
                );
            }
            Oracle::Error(pattern) => {
                let answer = command(&mut app, &settings, row.command);
                assert!(
                    answer.contains("error") && answer.contains(pattern),
                    "{}: expected a `{pattern}` error, got {answer}",
                    row.command
                );
            }
        }
    }
}

/// The vocabulary line and the coverage table agree, as normalized roots.
#[test]
fn the_vocabulary_and_the_coverage_table_agree() {
    let Some((mut app, settings)) = player_with_cli("vocabulary") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let answer = command(&mut app, &settings, "definitely-not-a-verb");
    // `<bar|pattern|all>` argument groups carry pipes of their own; strip them before splitting.
    let mut listing = answer
        .split("commands:")
        .nth(1)
        .expect("the vocabulary answer")
        .to_owned();
    while let (Some(open), Some(close)) = (listing.find('<'), listing.find('>')) {
        if open < close {
            listing.replace_range(open..=close, "ARG");
        } else {
            break;
        }
    }
    let vocab: Vec<String> = listing
        .split('|')
        .filter_map(|part| {
            part.trim()
                .trim_matches(|c: char| c == '"' || c == '}' || c == '\\')
                .split_whitespace()
                .next()
                .map(str::to_owned)
        })
        .filter(|root| !root.is_empty())
        .collect();

    let covered_roots: Vec<&str> = COVERAGE
        .iter()
        .filter_map(|row| row.command.split_whitespace().next())
        .collect();

    // Every vocabulary root must be either covered here or covered by the behaviour suites named
    // in the exception list — the roots the sweep does not re-drive because t9 already does,
    // each with its test named.
    const ELSEWHERE: &[(&str, &str)] = &[
        ("dump", "every t9 test reads it"),
        ("load", "t9::the_cli_loads_a_plugin_by_name"),
        ("set", "t9::every_bar_act / t6 lock suites"),
        ("reset", "covered as ResetParam row"),
        ("toggle", "covered as the `Step ` row"),
        ("tie", "t9::every_bar_act_has_a_verb"),
        (
            "lock",
            "t9::the_cli_drives_the_transport_and_the_locks_stay_absolute",
        ),
        ("unlock", "same t9 test"),
        ("tempo", "t9::the_cli_authors_a_sequence"),
        ("clear", "t9 clear coverage"),
        ("random", "reachable; asserted below"),
        ("stop", "t9 transport test"),
        ("note", "covered as PlayNote row"),
        ("off", "covered as ReleaseNote row"),
        ("bar", "covered as the `Bar 1` row"),
        ("copybar", "covered as the `Copy` row"),
        ("copypattern", "t9::every_bar_act_has_a_verb"),
        ("copysteps", "covered as CopyShortcut row"),
        ("cutsteps", "covered as CutShortcut row"),
        ("clearbar", "covered as the `Clear bar` row"),
        ("clearpattern", "covered as the `Clear pattern` row"),
        ("loop", "covered as the `Loop` row"),
        ("cc", "t9::the_cc_verb_reaches_the_control_map"),
        ("export", "t9::the_cli_exports_a_sound"),
        ("save", "t9 sequence tests"),
        ("savemid", "t9::savemid_writes_a_midi_file_that_reads_back"),
        ("dumpstate", "asserted below"),
        ("loadstate", "asserted below"),
        (
            "select",
            "covered as ClickStep/ShiftClickStep/CtrlClickStep rows",
        ),
        ("deselect", "covered as EscapeKey row"),
        ("play", "covered as SpaceKey row"),
        ("paste", "covered as PasteShortcut row"),
        ("octave", "covered as OctaveShift row"),
        ("panel", "asserted below"),
        ("bars", "covered as the + / − stepper rows"),
        ("steps", "covered as the - stepper row"),
        ("panic", "covered as the Panic row"),
        ("sustain", "covered as the Sustain row"),
        ("normalise", "covered as the Normalise row"),
        ("bend", "covered as the Bend row"),
        ("loadseq", "covered as the Load row"),
        ("savemid", "covered as the Name row"),
    ];
    let missing: Vec<&String> = vocab
        .iter()
        .filter(|root| {
            !covered_roots.contains(&root.as_str())
                && !ELSEWHERE.iter().any(|(r, _)| r == &root.as_str())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "vocabulary roots with no coverage anywhere: {missing:?}"
    );

    // The handful marked "asserted below", exercised so the exception list cannot lie:
    assert!(command(&mut app, &settings, "random").contains("ok"));
    assert!(command(&mut app, &settings, "panel settings closed").contains("ok"));
    assert!(command(&mut app, &settings, "panel settings open").contains("ok"));
    assert!(command(&mut app, &settings, "dumpstate").contains("ok"));
    assert!(command(&mut app, &settings, "loadstate").contains("ok"));
}

// ------------------------------------------------------------------------------------------------
// The guards: every refusal answers, promptly and usefully
// ------------------------------------------------------------------------------------------------

#[test]
fn every_guard_answers_with_a_usable_error() {
    let Some((mut app, settings)) = player_with_cli("guards") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    for (cmd, expect) in [
        ("frobnicate", "commands:"),
        ("toggle 0 C3", "counted from 1"),
        ("toggle 1 H9", "not a note"),
        ("select 0", "counted from 1"),
        ("load nothing-here", "no loadable plugin"),
        ("cc 200 1", "0..=127"),
        ("loop sideways", "loop takes"),
        ("bars 0", "not a number of bars"),
        ("fx remove 9", "the chain is empty"),
        ("fx add nothing-here", "no effect matches"),
        ("fx sideways", "fx takes"),
    ] {
        let answer = command(&mut app, &settings, cmd);
        assert!(
            answer.contains("error") && answer.contains(expect),
            "`{cmd}`: expected `{expect}`, got {answer}"
        );
    }
    // An empty clipboard refuses a paste rather than pretending.
    let answer = command(&mut app, &settings, "paste");
    assert!(
        answer.contains("error") || answer.contains("nothing copied"),
        "{answer}"
    );
}

// ------------------------------------------------------------------------------------------------
// The flows: the CLI's actual jobs, end to end
// ------------------------------------------------------------------------------------------------

/// The dump's canonical subset: what two identical sessions must agree on, byte for byte.
/// Timing, paths, logs, the `found` list and MIDI topology are environment, excluded here by
/// construction rather than per assertion.
fn canonical(dump: &serde_json::Value) -> serde_json::Value {
    let sequencer = &dump["state"]["sequencer"];
    serde_json::json!({
        "bars": sequencer["bars"],
        "steps_per_bar": sequencer["steps_per_bar"],
        "tempo": sequencer["tempo"],
        "steps": sequencer["steps"],
        "tied": sequencer["tied"],
        "locks": sequencer["locks"],
        "sequence_patch": dump["sequence_patch"],
    })
}

/// One composing session, from an empty player: shape, notes across bars, a lock, an audition.
fn compose(app: &mut mxm_player::ui::PlayerApp, settings: &Path) {
    for line in [
        "tempo 116",
        "steps 12",
        "toggle 1:1 C3",
        "toggle 1:5 E3",
        "toggle 2:1 G2",
        "tie 2:2",
        "toggle 3:1 C3",
        "set cutoff 0.4",
        "lock 2:1 cutoff 0.7",
        "loop bar",
        "loop all",
    ] {
        let answer = command(app, settings, line);
        assert!(answer.contains("ok"), "{line}: {answer}");
    }
}

#[test]
fn an_identical_script_gives_an_identical_canonical_dump() {
    let Some((mut first, settings_a)) = player_with_cli("flow-compose-a") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let (mut second, settings_b) = player_with_cli("flow-compose-b").expect("second sandbox");

    compose(&mut first, &settings_a);
    compose(&mut second, &settings_b);

    let a: serde_json::Value =
        serde_json::from_str(&command(&mut first, &settings_a, "dump")).expect("dump a");
    let b: serde_json::Value =
        serde_json::from_str(&command(&mut second, &settings_b, "dump")).expect("dump b");
    assert_eq!(
        canonical(&a),
        canonical(&b),
        "two identical scripts in fresh sandboxes must agree on every canonical byte"
    );
}

#[test]
fn a_base_vs_patch_divergence_is_visible_in_the_dump() {
    // The reason the CLI exists: this class of bug was invisible to every automated eye. Induce
    // the divergence deliberately - an editor drag parks the base at the step's value - and the
    // dump must carry it, in `parked_bases` and in `raw_readback` differing from the patch.
    let Some((mut app, settings)) = player_with_cli("flow-diagnose") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump");
    let cutoff_id = dump["state"]["plugin"]["params"]
        .as_array()
        .and_then(|params| params.iter().find(|p| p["name"] == "Cutoff"))
        .and_then(|p| p["id"].as_u64())
        .expect("cutoff id") as u32;

    assert!(command(&mut app, &settings, "set cutoff 0.6").contains("ok"));
    assert!(command(&mut app, &settings, "select 3").contains("ok"));
    // The editor-drag shape, driven directly: gesture open, the plugin's own value moves - the
    // real event, not only the notification, because the divergence lives in the plugin.
    app.plugin_gesture_began(cutoff_id);
    app.engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: cutoff_id,
            value: 0.2,
        });
    app.plugin_moved(&[(cutoff_id, 0.2)]);
    app.plugin_gesture_ended(cutoff_id);
    // Let the audio thread apply it before the readback.
    for _ in 0..20 {
        app.service();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump");
    let parked = dump["parked_bases"]
        .as_array()
        .expect("parked_bases in the dump");
    assert!(
        parked
            .iter()
            .any(|id| id.as_u64() == Some(cutoff_id.into())),
        "the parked base is machine-visible: {parked:?}"
    );
    let raw = dump["raw_readback"][cutoff_id.to_string()]
        .as_f64()
        .expect("raw readback");
    let patch = dump["sequence_patch"][cutoff_id.to_string()]
        .as_f64()
        .expect("sequence patch");
    assert!(
        (raw - patch).abs() > 0.1,
        "the divergence is visible: raw {raw} vs patch {patch}"
    );
}

#[test]
fn the_exports_do_not_surprise() {
    // Flow four, added after a live composing session: the .mid must reproduce the dump exactly,
    // and the audio must sound the authored roots - so a surprise always names its real owner.
    let Some((mut app, settings)) = player_with_cli("flow-exports") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // Two bars, distinct roots, a tie - enough to hear structure.
    for line in [
        "tempo 120",
        "toggle 1:1 C3",
        "tie 1:2",
        "toggle 1:9 C3",
        "toggle 2:1 G3",
        "tie 2:2",
        "toggle 2:9 G3",
    ] {
        assert!(command(&mut app, &settings, line).contains("ok"), "{line}");
    }

    // --- MIDI: read back through the same reader the player trusts, compare to the dump -------
    let answer = command(&mut app, &settings, "savemid conformance-exports");
    assert!(answer.contains("ok"), "{answer}");
    let mid_path = answer
        .split(char::is_whitespace)
        .next_back()
        .map(|s| s.trim_end_matches(['"', '}']).replace("\\\\", "\\"))
        .expect("path");
    let bytes = std::fs::read(mid_path.trim()).expect("the .mid");
    let loaded = mxm_player::sequencer::smf::read(&bytes).expect("the file reads back");
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump");
    let steps = dump["state"]["sequencer"]["steps"]
        .as_array()
        .expect("steps");
    for (index, notes) in steps.iter().enumerate() {
        let authored: Vec<String> = notes
            .as_array()
            .expect("a step")
            .iter()
            .map(|n| n.as_str().expect("a note name").to_owned())
            .collect();
        let round_tripped: Vec<String> = loaded
            .pattern
            .step(index)
            .notes()
            .into_iter()
            .map(mxm_player::sequencer::pattern::note_name)
            .collect();
        assert_eq!(
            authored,
            round_tripped,
            "step {}: the .mid must say what the dump says",
            index + 1
        );
    }

    // --- audio: each bar's fundamental matches its authored root -------------------------------
    let answer = command(&mut app, &settings, "export conformance-exports");
    assert!(answer.contains("ok"), "{answer}");
    let wav_path = answer
        .split(char::is_whitespace)
        .next_back()
        .map(|s| s.trim_end_matches(['"', '}']).replace("\\\\", "\\"))
        .expect("path");
    let decoded = mxm_audio_file_decode::decode_file(
        wav_path.trim(),
        &mxm_audio_file_decode::Limits::new(
            usize::MAX,
            mxm_audio_file_decode::AtLimit::Refuse,
            mxm_audio_file_decode::Keep::AllUpTo(8),
        ),
    )
    .expect("a readable WAV");
    let channels = decoded.channels;
    let sample_rate = decoded.sample_rate;
    let samples = decoded.interleaved;
    let mono: Vec<f32> = samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    let sr = sample_rate as f64;
    let frames_per_bar = (sr * 16.0 * 60.0 / (120.0 * 4.0)) as usize;

    // C and G, as pitch classes; the octave is the instrument's business.
    for (bar, want) in [(0usize, 0u8), (1, 7)] {
        let start = bar * frames_per_bar + frames_per_bar / 16; // inside the first note
        let window: Vec<f64> = mono[start + (sr * 0.05) as usize..]
            .iter()
            .take((sr * 0.15) as usize)
            .map(|s| f64::from(*s))
            .collect();
        let mean = window.iter().sum::<f64>() / window.len() as f64;
        let rms = (window.iter().map(|s| (s - mean) * (s - mean)).sum::<f64>()
            / window.len() as f64)
            .sqrt();
        assert!(rms > 1e-4, "bar {}: the note is audible", bar + 1);
        // Plain autocorrelation over the bass band.
        let (lo, hi) = ((sr / 400.0) as usize, (sr / 25.0) as usize);
        let mut best = (0.0f64, lo);
        for lag in lo..hi.min(window.len() / 2) {
            let mut acc = 0.0;
            for i in 0..window.len() - lag {
                acc += (window[i] - mean) * (window[i + lag] - mean);
            }
            if acc > best.0 {
                best = (acc, lag);
            }
        }
        let freq = sr / best.1 as f64;
        let midi = 69.0 + 12.0 * (freq / 440.0).log2();
        let class = (midi.round() as i64).rem_euclid(12) as u8;
        assert_eq!(
            class,
            want,
            "bar {}: fundamental {freq:.1} Hz is pitch class {class}, authored {want}",
            bar + 1
        );
    }
}

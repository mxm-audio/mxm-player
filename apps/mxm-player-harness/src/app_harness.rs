//! Layer 2: the real `PlayerApp`, driven the way a person drives it.
//!
//! `egui_kittest` hosts the app with no window, a fake backend, and a **sandboxed settings
//! directory** — the last of which is not a nicety. With the production configuration, an app-level
//! test would rewrite the settings of whoever is using the player on the same machine.
//!
//! Three assertion styles are available here, and they see different things:
//!
//! | Style | Reads | Blind to |
//! |---|---|---|
//! | Structural | the AccessKit tree (`get_by_label`) | anything drawn with bare `painter` calls |
//! | State | [`PlayerState`] | what was actually drawn |
//! | Paint output | [`AppHarness::painted_rects`] | text layout, perceived colour |
//!
//! The keyboard is painted directly rather than as widgets, so **only** paint output can see it.

use egui_kittest::Harness;
use mxm_player::config::PlayerConfig;
use mxm_player::engine::audio::FakeBackend;
use mxm_player::state::PlayerState;
use mxm_player::ui::PlayerApp;
use std::path::{Path, PathBuf};

/// A filled rectangle the app painted, in screen coordinates.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PaintedRect {
    pub rect: egui::Rect,
    pub fill: egui::Color32,
}

impl PaintedRect {
    pub fn width(&self) -> f32 {
        self.rect.width()
    }

    pub fn height(&self) -> f32 {
        self.rect.height()
    }
}

/// How many frames one [`AppHarness::run`] draws.
pub const FRAMES_PER_RUN: usize = 4;

/// The app under test, plus the scratch directory it is confined to.
pub struct AppHarness {
    pub harness: Harness<'static, PlayerApp>,
    /// Kept alive so the directory outlives the test.
    dir: PathBuf,
}

impl AppHarness {
    /// Builds the app in a sandbox, searching only `search_paths`.
    pub fn new(name: &str, search_paths: Vec<PathBuf>) -> Self {
        let dir = std::env::temp_dir().join(format!("mxm-player-ui-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");

        let config = PlayerConfig::sandboxed(&dir, Box::new(FakeBackend::new()))
            .with_search_paths(search_paths);

        let harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 760.0))
            .build_eframe(move |_cc| PlayerApp::with_config(config));

        Self { harness, dir }
    }

    /// The app itself, for driving it the way a session does.
    pub fn app(&mut self) -> &mut PlayerApp {
        self.harness.state_mut()
    }

    /// The sandbox, for asserting what was and was not written.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Draws a fixed number of frames.
    ///
    /// A fixed count rather than `Harness::run`'s run-until-settled: scroll areas animate, so
    /// "settled" never arrives and `run` panics on its step cap. A fixed count is also the more
    /// deterministic choice, which is what this whole layer is for.
    pub fn run(&mut self) {
        self.harness.run_steps(FRAMES_PER_RUN);
    }

    /// Everything the window is showing, as data.
    pub fn state(&mut self) -> PlayerState {
        self.harness.state_mut().state()
    }

    /// Clicks at a screen position, rather than at a widget found by label.
    ///
    /// **The only way to ask about empty space.** `get_by_label(..).click()` needs something in the
    /// accessibility tree to aim at, and the question "what happens when I click where nothing is"
    /// has no such target by definition.
    ///
    /// Press and release are separate events with a frame between them because that is what egui
    /// resolves a click from; sending them into one frame registers movement and a button state,
    /// not a click.
    pub fn click_at(&mut self, pos: egui::Pos2) {
        for pressed in [true, false] {
            self.harness
                .input_mut()
                .events
                .push(egui::Event::PointerMoved(pos));
            self.harness
                .input_mut()
                .events
                .push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            self.harness.step();
        }
        self.run();
    }

    /// Resizes the window and redraws, which is how "does a wider window reveal more" is asked.
    pub fn resize(&mut self, width: f32, height: f32) {
        self.harness.set_size(egui::vec2(width, height));
        self.run();
    }

    /// Every filled rectangle the last frame painted.
    ///
    /// This is the oracle for painter-only widgets. The keyboard allocates **one** interaction
    /// region and paints each key directly, so there is no AccessKit node per key — a structural
    /// assertion cannot see whether any keys were drawn at all, let alone how many.
    pub fn painted_rects(&self) -> Vec<PaintedRect> {
        let mut rects = Vec::new();
        for clipped in &self.harness.output().shapes {
            collect_rects(&clipped.shape, &mut rects);
        }
        rects
    }

    /// Painted rectangles whose fill matches `colour`, which is how keys are identified.
    pub fn painted_rects_filled(&self, colour: egui::Color32) -> Vec<PaintedRect> {
        self.painted_rects()
            .into_iter()
            .filter(|r| r.fill == colour)
            .collect()
    }

    /// Every filled circle the last frame painted.
    ///
    /// The same oracle as [`Self::painted_rects`], for the same reason: the dot that marks a step or
    /// a parameter as sequenced is painted directly, so no AccessKit node exists to find it by. A
    /// mark nothing can see is a mark that can silently stop being drawn.
    pub fn painted_circles(&self) -> Vec<PaintedCircle> {
        let mut circles = Vec::new();
        for clipped in &self.harness.output().shapes {
            collect_circles(&clipped.shape, &mut circles);
        }
        circles
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PaintedCircle {
    pub center: egui::Pos2,
    pub radius: f32,
    pub fill: egui::Color32,
}

/// Walks the shape tree, since egui nests shapes inside `Shape::Vec`.
fn collect_circles(shape: &egui::Shape, out: &mut Vec<PaintedCircle>) {
    match shape {
        egui::Shape::Circle(circle) => {
            if circle.fill != egui::Color32::TRANSPARENT {
                out.push(PaintedCircle {
                    center: circle.center,
                    radius: circle.radius,
                    fill: circle.fill,
                });
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_circles(shape, out);
            }
        }
        _ => {}
    }
}

/// Walks the shape tree, since egui nests shapes inside `Shape::Vec`.
fn collect_rects(shape: &egui::Shape, out: &mut Vec<PaintedRect>) {
    match shape {
        egui::Shape::Rect(rect) => {
            if rect.fill != egui::Color32::TRANSPARENT {
                out.push(PaintedRect {
                    rect: rect.rect,
                    fill: rect.fill,
                });
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_rects(shape, out);
            }
        }
        _ => {}
    }
}

/// The workspace root containing staged fixtures and any built plugin bundles.
pub use crate::workspace_root;

/// The directory holding the built mxm-mono-01 bundle, or `None` if it has not been built.
/// Existing collection tests use this as both directory lookup and their mono-01 skip gate.
pub fn bundled_dir() -> Option<PathBuf> {
    let dir = workspace_root().join("target").join("bundled");
    dir.join("mxm-mono-01.clap").exists().then_some(dir)
}

/// The bundle directory, or `None` when `plugin`'s own bundle has not been built there — the skip
/// gate for a plugin's host tests, which must not wait on any other plugin's bundle (in its own
/// repository the others are never built).
pub fn bundled_dir_with(plugin: &str) -> Option<PathBuf> {
    let dir = workspace_root().join("target").join("bundled");
    dir.join(format!("{plugin}.clap")).exists().then_some(dir)
}

/// The shared bundle directory for a test that checks its own product artifact.
pub fn any_bundled_dir() -> Option<PathBuf> {
    let dir = workspace_root().join("target").join("bundled");
    dir.is_dir().then_some(dir)
}

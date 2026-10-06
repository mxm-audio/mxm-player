//! MXM Player — a CLAP host for testing and playing the MXM Synth Collection.
//!
//! A host, not a standalone wrapper: it loads bundles at runtime, so it works for every future
//! MXM plugin without rebuilding, and tests the `.clap` we actually ship rather than a
//! standalone wrapper around it.
//!
//! The crate is a library as well as a binary so the verification suite can drive the engine,
//! the envelope and the event paths directly, without a window or an audio device.

pub mod cli;
pub mod clock;
pub mod config;
pub mod control_map;
pub mod discovery;
pub mod engine;
pub mod entry;
pub mod envelope;
pub mod events;
pub mod host;
pub mod midi;
pub mod notifier;
pub mod offline;
pub mod ownership;
pub mod params;
pub mod sequencer;
pub mod session;
pub mod settings;
pub mod state;
pub mod ui;

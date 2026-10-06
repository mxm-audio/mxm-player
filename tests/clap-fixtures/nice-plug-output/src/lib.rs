//! Test-only nice-plug CLAP plugin that fills the host's output-event list.
//!
//! This is separate from the clack-based fixture factory because the behavior under test is
//! nice-plug's own `ProcessContext::try_send_event()` implementation. It is loaded only by path from
//! the build tree and is never staged with shipped plugins.
//!
//! **Since nice-plug 0.4.2 the wrapper has no output queue.** `try_send_event()` pushes each event
//! straight into the host's `out_events`; when the host's list refuses one, it returns
//! `SendEventError::HostBufferFull` and hands the event back. So each process call sends a distinct
//! note-on into the empty list, then ordinary note-ons until the host refuses one, then that
//! note's note-off. The full list refuses the note-off too, because a CLAP output list is
//! append-only and nothing can displace an accepted event. The fixture keeps it and sends it first
//! in its next process call. *(Under nice-plug 0.3.0 it sent 1000 note-ons and the note-off
//! through `ProcessContext::send_event()` into the wrapper's bounded queue, whose MXM guard
//! admitted the note-off in the same call by displacing an ordinary event. 0.4 removed that
//! queue, and the guard with it.)*

use nice_plug::context::process::SendEventError;
use nice_plug::midi::{Channel, Key, VoiceID};
use nice_plug::prelude::*;
use std::sync::Arc;

/// Stops the flood in a host whose list never refuses, so such a host cannot keep the audio
/// thread here. The player's list refuses long before.
const MAX_ORDINARY_EVENTS_PER_BLOCK: i32 = 1 << 16;
const ORDINARY_NOTE: u8 = 60;
/// The distinct note: sent into the empty list, terminated after the list is full. Ordinary
/// note-ons use the voice IDs after it.
const ADMITTED_NOTE_VOICE_ID: i32 = 0;
const ADMITTED_NOTE: u8 = 61;

#[derive(Params)]
struct FixtureParams {
    #[id = "enabled"]
    enabled: BoolParam,
}

impl Default for FixtureParams {
    fn default() -> Self {
        Self {
            enabled: BoolParam::new("Enabled", true),
        }
    }
}

struct OutputFlood {
    params: Arc<FixtureParams>,
    /// The note-off the host's full list refused, as `try_send_event()` handed it back. It is sent
    /// first in the next process call, when the host's list is empty again.
    refused_termination: Option<NoteEvent<()>>,
}

impl Default for OutputFlood {
    fn default() -> Self {
        Self {
            params: Arc::new(FixtureParams::default()),
            refused_termination: None,
        }
    }
}

impl OutputFlood {
    /// Sends a termination, keeping it for the next process call when the host's list is full.
    /// Returns whether the host took it.
    ///
    /// Only `HostBufferFull` is kept. Any other refusal drops the termination, so the host
    /// regression sees the note stick rather than a retry that hides a wrong error.
    fn send_termination(
        &mut self,
        context: &mut impl ProcessContext<Self>,
        termination: NoteEvent<()>,
    ) -> bool {
        match context.try_send_event(termination) {
            Ok(()) => true,
            Err((termination, SendEventError::HostBufferFull)) => {
                self.refused_termination = Some(termination);
                false
            }
            Err(_) => false,
        }
    }
}

impl Plugin for OutputFlood {
    const NAME: &'static str = "MXM Fixture: nice-plug output flood";
    const VENDOR: &'static str = "mxm";
    const URL: &'static str = "https://mxm.dk";
    const EMAIL: &'static str = "plugins@mxm.dk";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::Basic;

    type Editor = ();
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        for channel in buffer.as_slice() {
            channel.fill(0.0);
        }

        if !self.params.enabled.value() {
            return ProcessStatus::Normal;
        }

        // The termination refused last time goes first, at the start of this call. A host that
        // refuses even that has no room for anything else.
        if let Some(mut termination) = self.refused_termination.take() {
            termination.subtract_timing(termination.timing());
            if !self.send_termination(context, termination) {
                return ProcessStatus::Normal;
            }
        }

        // Into a list with room, so the host takes it before saturation. Its result is the
        // host regression's to check.
        let _ = context.try_send_event(NoteEvent::NoteOn {
            timing: 0,
            voice_id: VoiceID::ID(ADMITTED_NOTE_VOICE_ID),
            channel: Channel::Number(0),
            key: Key::Number(ADMITTED_NOTE),
            velocity: 1.0,
        });

        // Ordinary output until the host's list refuses one. The refused one is dropped: nothing
        // depends on ordinary output arriving.
        for voice_id in ADMITTED_NOTE_VOICE_ID + 1..=MAX_ORDINARY_EVENTS_PER_BLOCK {
            let sent = context.try_send_event(NoteEvent::NoteOn {
                timing: 0,
                voice_id: VoiceID::ID(voice_id),
                channel: Channel::Number(0),
                key: Key::Number(ORDINARY_NOTE),
                velocity: 1.0,
            });
            if sent.is_err() {
                break;
            }
        }

        // After saturation. The full list refuses it, and it comes back for the next call.
        self.send_termination(
            context,
            NoteEvent::NoteOff {
                timing: buffer.samples().saturating_sub(1) as u32,
                voice_id: VoiceID::ID(ADMITTED_NOTE_VOICE_ID),
                channel: Channel::Number(0),
                key: Key::Number(ADMITTED_NOTE),
                velocity: 0.0,
            },
        );

        ProcessStatus::Normal
    }
}

impl ClapPlugin for OutputFlood {
    const CLAP_ID: &'static str = "dk.mxm.fixture.nice-plug-output-flood";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Test-only plugin that fills the host's output-event list through nice-plug");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Instrument, ClapFeature::Stereo];
}

nice_export_clap!(OutputFlood);

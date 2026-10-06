//! Test-only nice-plug CLAP plugin that floods the wrapper's output-event queue.
//!
//! This is separate from the clack-based fixture factory because the behavior under test is
//! nice-plug's own `ProcessContext::send_event()` implementation. It is loaded only by path from
//! the build tree and is never staged with shipped plugins.

use nice_plug::prelude::*;
use std::sync::Arc;

const OUTPUT_EVENTS_PER_BLOCK: u32 = 1_000;
const ADMITTED_NOTE_VOICE_ID: u32 = 510;
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
}

impl Default for OutputFlood {
    fn default() -> Self {
        Self {
            params: Arc::new(FixtureParams::default()),
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

        if self.params.enabled.value() {
            for voice_id in 0..OUTPUT_EVENTS_PER_BLOCK {
                context.send_event(NoteEvent::NoteOn {
                    timing: voice_id % buffer.samples().max(1) as u32,
                    voice_id: Some(voice_id as i32),
                    channel: 0,
                    note: if voice_id == ADMITTED_NOTE_VOICE_ID {
                        ADMITTED_NOTE
                    } else {
                        60
                    },
                    velocity: 1.0,
                });
            }
            context.send_event(NoteEvent::NoteOff {
                timing: buffer.samples().saturating_sub(1) as u32,
                voice_id: Some(ADMITTED_NOTE_VOICE_ID as i32),
                channel: 0,
                note: ADMITTED_NOTE,
                velocity: 0.0,
            });
        }

        ProcessStatus::Normal
    }
}

impl ClapPlugin for OutputFlood {
    const CLAP_ID: &'static str = "dk.mxm.fixture.nice-plug-output-flood";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Test-only plugin that exceeds nice-plug's bounded output-event queue");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Instrument, ClapFeature::Stereo];
}

nice_export_clap!(OutputFlood);

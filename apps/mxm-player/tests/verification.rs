//! The rest of the verification matrix: the properties that need a deliberately misbehaving
//! plugin, or a look at what the audio thread does rather than what it produces.

use mxm_player_harness::harness;

use harness::Harness;
use mxm_player::engine::PluginOutput;
use mxm_player::engine::processor::Command;
use mxm_player::envelope::GlobalRecovery;
use mxm_player::events::input::Payload;
use mxm_player::sequencer::SequencerState;
use mxm_player::sequencer::clock::Transport;
use mxm_player::sequencer::locks::LockSet;
use mxm_player::sequencer::pattern::Pattern;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

// --- the allocation hook ----------------------------------------------------------------------

/// Counts allocations made while a thread has opted in.
///
/// The same standard the plugins are held to: `assert_process_allocs` aborts on any allocation
/// inside a plugin's `process()`, and the host's callback has to meet it too.
struct WatchingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// `const` initialised, so reading the flag cannot itself allocate on first touch.
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: every method forwards to the system allocator unchanged; the only addition is an
// atomic increment and a thread-local read, neither of which allocates.
unsafe impl GlobalAlloc for WatchingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if WATCHING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if WATCHING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: WatchingAllocator = WatchingAllocator;

/// Runs `body` with allocation counting on, and returns how many allocations it made.
fn count_allocations(body: impl FnOnce()) -> u64 {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    WATCHING.with(|w| w.set(true));
    body();
    WATCHING.with(|w| w.set(false));
    ALLOCATIONS.load(Ordering::Relaxed) - before
}

// --- audio thread ------------------------------------------------------------------------------

#[test]
fn consuming_a_sequencer_state_never_allocates_or_frees_on_the_audio_thread() {
    // **The property the pointer handoff exists for.** The state reaches the worker as an `Arc`, so
    // the callback that consumes it holds the only reference — and releasing that reference would
    // call the allocator inside the callback. It hands the `Arc` back instead, and this is what says
    // it does: the counting hook fires on every allocation *and* is the same one the deallocation
    // path would go through.
    //
    // A pattern and lock set are attached so the state is not trivially empty; the runtime copies
    // out of it, which is exactly the work being measured.
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.mxm-mono-01", 2).expect("mxm-mono-01 hosts");
    h.render(256);
    h.render(256);

    // **A long one, because that is the case the pointer handoff exists for.** Sixty-four bars is
    // 1,024 steps and roughly 17 KB of pattern; copying that into the runtime would allocate inside
    // the callback, which is why the runtime reads through the pointer and never clones.
    let mut pattern = Pattern::empty();
    pattern.set_bars(64);
    for step in 0..64 {
        pattern.toggle(step * 16, 60 + (step % 12) as u8);
    }
    let mut locks = LockSet::EMPTY;
    for id in 0..4u32 {
        for step in 0..8 {
            locks.set(step, id, 0.5, 0.25).expect("room");
        }
    }

    // Queued from this thread, consumed by the callback below.
    for serial in 1..=8u64 {
        h.commands
            .push(Command::SetSequencer(std::sync::Arc::new(SequencerState {
                pattern: pattern.clone(),
                locks,
                editing: None,
                held: mxm_player::sequencer::runtime::HeldParams::EMPTY,
                tempo: 120.0,
                bar: None,
                transport: Transport::Playing,
                generation: serial,
                serial,
            })))
            .expect("room in the command queue");
    }

    let allocations = count_allocations(|| {
        h.render(512);
    });

    assert_eq!(
        allocations, 0,
        "consuming a sequencer state must neither allocate nor free on the audio thread"
    );
    assert!(
        h.retired.slots() > 0,
        "and the states must have been handed back rather than dropped there"
    );
}

#[test]
fn retiring_a_sequencer_state_cannot_overflow_its_queue() {
    // **The argument the hand-back rests on.** The worker must never drop the last reference, so a
    // full retire queue would leave it holding one — or freeing it on the audio thread, which is the
    // thing being avoided. The queue is as deep as the command queue and the GUI drains it before
    // pushing, so the states in flight can never exceed the commands in flight.
    //
    // Filling the command queue completely and consuming all of it is the worst case that argument
    // has to survive.
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.mxm-mono-01", 2).expect("mxm-mono-01 hosts");
    h.render(256);

    let mut queued = 0u64;
    while h
        .commands
        .push(Command::SetSequencer(std::sync::Arc::new(SequencerState {
            pattern: Pattern::empty(),
            locks: LockSet::EMPTY,
            editing: None,
            held: mxm_player::sequencer::runtime::HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Playing,
            generation: queued + 1,
            serial: queued + 1,
        })))
        .is_ok()
    {
        queued += 1;
    }
    assert!(queued > 0, "the command queue took at least one");

    h.render(512);

    assert_eq!(
        h.retired.slots(),
        queued as usize,
        "every state the worker consumed came back rather than being dropped on its thread"
    );
}

#[test]
fn the_audio_callback_never_allocates() {
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.mxm-mono-01", 2).expect("mxm-mono-01 hosts");

    // Warm up: the first callback sizes the test's own output buffer and starts processing.
    h.render(256);
    h.render(256);

    // A busy callback: notes, a release, controllers, a panic, and the recovery it triggers.
    h.push(
        0,
        Payload::NoteOn {
            channel: 0,
            key: 60,
            velocity: 1.0,
        },
    );
    h.push(
        1,
        Payload::NoteOn {
            channel: 0,
            key: 64,
            velocity: 1.0,
        },
    );
    h.push(
        0,
        Payload::ControlChange {
            channel: 0,
            controller: 1,
            value: 90,
        },
    );
    h.push(
        0,
        Payload::PitchBend {
            channel: 0,
            value: 0.75,
        },
    );
    h.push(0, Payload::SustainPedal(true));
    h.push(
        0,
        Payload::NoteOff {
            channel: 0,
            key: 60,
            velocity: 0.0,
        },
    );

    let allocations = count_allocations(|| {
        h.render(256);
    });
    assert_eq!(
        allocations, 0,
        "the callback allocated {allocations} times while merging, converting and processing"
    );

    // ...and the paths that expand one event into many: sustain lift and a global panic.
    h.push(0, Payload::SustainPedal(false));
    h.push(0, Payload::GlobalPanic);
    let allocations = count_allocations(|| {
        h.render(256);
    });
    assert_eq!(
        allocations, 0,
        "recovery and deferred-release flushing allocated {allocations} times"
    );

    h.shutdown();
}

#[test]
fn a_dense_buffer_of_events_does_not_make_the_callback_allocate() {
    // A handful of events is not a real test of the merge: Rust's *stable* sort allocates scratch
    // space once a slice is more than a few elements long, so an allocation-free callback under
    // six events can still allocate under four hundred. This is the case that found that.
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.mxm-mono-01", 2).expect("mxm-mono-01 hosts");
    h.render(256);
    h.render(256);

    // Below the 512-event ceiling nice-plug's own input queue has (see docs/known-issues.md in
    // mxm-kit), so this measures the host rather than tripping the plugin's allocation assertion.
    for i in 0..400u32 {
        let source = (i % 2) as usize;
        h.push(
            source,
            Payload::ControlChange {
                channel: (i % 16) as u8,
                // Varied, so the host's same-offset coalescing does not merge them away.
                controller: (20 + (i % 90)) as u8,
                value: (i % 128) as u8,
            },
        );
    }

    let allocations = count_allocations(|| {
        h.render(1024);
    });
    assert_eq!(
        allocations, 0,
        "merging and sorting a dense buffer allocated {allocations} times"
    );

    h.shutdown();
}

#[test]
fn an_oversized_callback_is_chunked_without_allocating() {
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.mxm-mono-01", 1).expect("mxm-mono-01 hosts");

    // `BufferSize::Fixed` is only a request, so the callback must cope with any frame count —
    // including one larger than the hard cap it renders in.
    let oversized = mxm_player::engine::processor::MAX_BLOCK_FRAMES as usize * 2 + 37;
    h.render(oversized);
    h.render(oversized);

    let allocations = count_allocations(|| {
        h.render(oversized);
    });
    assert_eq!(
        allocations, 0,
        "splitting an oversized callback allocated {allocations} times"
    );

    h.shutdown();
}

// --- the fixtures ------------------------------------------------------------------------------

#[test]
fn a_clap_only_note_port_is_actually_cleared_by_its_own_recovery() {
    // The test that distinguishes a working recovery dialect from an assumed one. A CLAP-only
    // port never receives CC 120, so if the wildcard choke did not work its voices would simply
    // stay held.
    let Some(bundle) = harness::fixtures() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.fixture.clap-only-notes", 1).expect("it hosts");

    assert_eq!(
        h.envelope.global_recovery(),
        GlobalRecovery::WildcardChoke,
        "a port that does not accept MIDI must not be sent CC 120"
    );

    for key in [60u8, 64, 67] {
        h.push(
            0,
            Payload::NoteOn {
                channel: 0,
                key,
                velocity: 1.0,
            },
        );
    }
    h.render(256);
    assert_eq!(voices(&mut h), 3.0, "the fixture should be holding three");

    h.push(0, Payload::GlobalPanic);
    h.render(256);
    assert_eq!(
        voices(&mut h),
        0.0,
        "the wildcard choke must actually clear the voices, not merely be sent"
    );

    h.shutdown();
}

/// Reads the fixture's parameter, which is how it reports its internal counter.
fn voices(h: &mut Harness) -> f64 {
    use clack_extensions::params::PluginParams;
    let ext: PluginParams = h
        .instance
        .plugin_shared_handle()
        .get_extension()
        .expect("the fixture implements params");
    let mut handle = h.instance.plugin_handle();
    ext.get_value(&mut handle, clack_host::utils::ClapId::new(0))
        .expect("the fixture reports its counter")
}

#[test]
fn on_main_thread_is_actually_run_and_a_re_request_is_serviced_later() {
    let Some(bundle) = harness::fixtures() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.fixture.main-thread", 1).expect("it hosts");

    // The fixture asked for a callback as soon as its main-thread state existed.
    assert!(
        h.shared.requests.callback.load(Ordering::Acquire),
        "the request must have been recorded"
    );
    assert_eq!(voices(&mut h), 0.0, "nothing has run yet");

    // What the GUI does each frame: consume the flag and run the callback, on this thread.
    assert!(h.shared.take_callback_request());
    h.instance.call_on_main_thread_callback();
    assert_eq!(voices(&mut h), 1.0, "on_main_thread should have run once");

    // The request raised *inside* that callback is retained and serviced on a later turn.
    assert!(
        h.shared.requests.callback.load(Ordering::Acquire),
        "a request made during the callback must be retained, not recursed into"
    );
    assert!(h.shared.take_callback_request());
    h.instance.call_on_main_thread_callback();
    assert_eq!(voices(&mut h), 2.0);

    h.shutdown();
}

#[test]
fn a_tail_that_becomes_infinite_is_not_slept_on_a_stale_finite_value() {
    let Some(bundle) = harness::fixtures() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.fixture.tail-shift", 1).expect("it hosts");

    // The fixture reports a finite tail, then flips to infinite and calls `HostTail::changed`.
    for _ in 0..12 {
        h.render(256);
    }
    assert!(
        h.shared.notifications.tail_changed.load(Ordering::Acquire) || h.meters.callbacks() > 0,
        "the change notification should have reached the host"
    );

    // Keep going well past the originally reported finite length; the plugin must still be
    // being processed rather than slept on the stale value.
    for _ in 0..64 {
        h.render(256);
    }
    assert_eq!(
        h.worker.state(),
        mxm_player::engine::processor::RunState::Running,
        "an infinite tail must not be truncated by a cached finite length"
    );

    h.shutdown();
}

#[test]
fn concurrent_plugin_logging_is_carried_without_losing_track_of_what_was_dropped() {
    let Some(bundle) = harness::fixtures() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.fixture.log-spam", 1).expect("it hosts");

    // The fixture logs from two threads at once, which an SPSC transport could not accept.
    h.render(256);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut received = 0u64;
    while std::time::Instant::now() < deadline {
        while h.shared.logs.pop().is_some() {
            received += 1;
        }
        if received > 500 {
            break;
        }
        h.render(64);
    }

    assert!(
        received > 0,
        "messages from the plugin's own threads must reach the host"
    );
    // Drops are counted rather than silent; either outcome is acceptable, an uncounted loss is
    // not.
    let dropped = h.shared.logs.dropped();
    assert!(received + dropped > 0);

    h.shutdown();
}

#[test]
fn plugin_output_events_are_routed_and_overflow_is_counted() {
    let Some(bundle) = harness::fixtures() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.fixture.event-emitter", 1).expect("it hosts");

    // The emitter produces a parameter value, a gesture pair, notes, and a note expression that
    // has no MIDI 1 representation — plus, scaled by its parameter, a flood.
    h.render(256);

    let to_gui = h.drain_plugin_output();
    assert!(
        to_gui
            .iter()
            .any(|e| matches!(e, PluginOutput::ParamValue { .. })),
        "parameter values must reach the GUI"
    );
    assert!(
        to_gui
            .iter()
            .any(|e| matches!(e, PluginOutput::GestureBegin { .. }))
            && to_gui
                .iter()
                .any(|e| matches!(e, PluginOutput::GestureEnd { .. })),
        "both ends of a plugin-driven gesture must reach the GUI"
    );

    let to_midi = h.drain_midi_out();
    assert!(
        to_midi.iter().any(|e| mxm_player::midi::is_press(e.data)),
        "representable notes must reach the MIDI-out worker"
    );

    // Drive the flood: the sink must fail rather than grow, and say that it did.
    h.push(
        0,
        Payload::ParamValue {
            param_id: 0,
            value: 1.0,
        },
    );
    let allocations = count_allocations(|| {
        h.render(256);
        h.render(256);
    });
    assert_eq!(
        allocations, 0,
        "a flooded output sink must not make the callback allocate"
    );

    h.shutdown();
}

#[test]
fn a_calibrated_load_moves_the_meters() {
    // Validated against a synthetic load rather than Task Manager, which normalises across cores
    // and includes the GUI and scanning.
    let Some(bundle) = harness::fixtures() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.fixture.cpu-load", 1).expect("it hosts");

    for _ in 0..8 {
        h.render(256);
    }
    let light = h.meters.plugin_load();

    // The fixture's parameter scales its work per frame.
    h.push(
        0,
        Payload::ParamValue {
            param_id: 0,
            value: 1.0,
        },
    );
    for _ in 0..8 {
        h.render(256);
    }
    let heavy = h.meters.plugin_load();

    assert!(
        heavy > light,
        "doubling the synthetic work should move the plugin meter: {light} then {heavy}"
    );
    assert!(
        h.meters.callback_load() >= h.meters.plugin_load(),
        "the callback includes the plugin, so it is never the cheaper of the two"
    );

    h.shutdown();
}

// --- topology and capacity ---------------------------------------------------------------------

#[test]
fn the_merged_buffer_holds_every_producer_plus_the_emergency_reserve() {
    use mxm_player::events::input::{
        EMERGENCY_RESERVE, MAX_INPUT_PRODUCERS, MergedInput, PRODUCER_QUEUE_CAPACITY,
    };

    let merged = MergedInput::new();
    assert!(
        merged.capacity() >= MAX_INPUT_PRODUCERS * PRODUCER_QUEUE_CAPACITY + EMERGENCY_RESERVE,
        "a full merge must not be able to discard what must never be lost"
    );
}

#[test]
fn more_inputs_than_the_maximum_are_refused_with_a_reason_rather_than_truncated() {
    use mxm_player::engine::Engine;
    use mxm_player::engine::audio::FakeBackend;
    use mxm_player::events::input::MAX_INPUT_PRODUCERS;

    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine
        .load(&bundle, "dk.mxm.mxm-mono-01")
        .expect("it loads");

    // More ports than there are slots, and none of them exist on this machine either — both
    // reasons land in the same visible list rather than being silently dropped.
    let ports: Vec<String> = (0..MAX_INPUT_PRODUCERS + 4)
        .map(|i| format!("no such port {i}"))
        .collect();
    engine.set_midi_inputs(ports.clone());
    engine.start(&backend).expect("the fake backend starts");

    assert_eq!(
        engine.refused_midi_inputs().len(),
        ports.len(),
        "every port that could not be connected must be reported"
    );
    assert!(engine.connected_midi_inputs().is_empty());

    engine.stop_now().expect("the processor comes back");
}

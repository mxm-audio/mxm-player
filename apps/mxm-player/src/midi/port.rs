//! The real MIDI output port, and the thread that drives the worker.
//!
//! `midir::send()` is I/O, so it never happens on the audio thread. The worker thread is also
//! where the panic recovery lives, because it is allowed to block.

use super::out::{MidiOutWorker, MidiSink, OutPanic, OutgoingEvent};
use midir::{MidiOutput, MidiOutputConnection, MidiOutputPort};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long the worker sleeps between passes when there is nothing to send.
const IDLE_POLL: Duration = Duration::from_millis(1);

/// Lists the MIDI output ports currently available.
pub fn list_ports() -> Result<Vec<String>, String> {
    let output = MidiOutput::new("mxm-player-scan").map_err(|e| e.to_string())?;
    Ok(output
        .ports()
        .iter()
        .filter_map(|port| output.port_name(port).ok())
        .collect())
}

/// A `midir` connection, behind the sink trait so tests can substitute a failing one.
pub struct MidirSink {
    connection: MidiOutputConnection,
    name: String,
}

impl MidirSink {
    pub fn open(port_name: &str) -> Result<Self, String> {
        let output = MidiOutput::new("mxm-player").map_err(|e| e.to_string())?;
        let port = find_port(&output, port_name)?;
        let connection = output
            .connect(&port, "mxm-player-out")
            .map_err(|e| e.to_string())?;

        Ok(Self {
            connection,
            name: port_name.to_owned(),
        })
    }
}

fn find_port(output: &MidiOutput, name: &str) -> Result<MidiOutputPort, String> {
    output
        .ports()
        .into_iter()
        .find(|port| output.port_name(port).is_ok_and(|n| n == name))
        .ok_or_else(|| format!("no MIDI output port named `{name}` is connected"))
}

impl MidiSink for MidirSink {
    fn send(&mut self, data: &[u8]) -> Result<(), String> {
        self.connection.send(data).map_err(|e| e.to_string())
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

/// A running MIDI-out worker thread.
pub struct RunningWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    pub faulted: Arc<AtomicBool>,
    pub port_name: String,
}

impl RunningWorker {
    pub fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::Relaxed)
    }
}

impl Drop for RunningWorker {
    fn drop(&mut self) {
        // Changing the MIDI output port releases everything outstanding on the old port before
        // closing it, which is what `release_all` in the loop below does on the way out.
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts a worker thread for `sink`.
pub fn spawn<S: MidiSink + Send + 'static>(
    sink: S,
    queue: rtrb::Consumer<OutgoingEvent>,
    panic: Arc<OutPanic>,
) -> RunningWorker {
    let port_name = sink.name();
    let faulted = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let mut worker = MidiOutWorker::new(sink, queue, panic, Arc::clone(&faulted));
    let stop_for_thread = Arc::clone(&stop);

    let handle = std::thread::spawn(move || {
        while !stop_for_thread.load(Ordering::Acquire) {
            let outcome = worker.pump();
            if outcome.sent == 0 && outcome.retrying == 0 {
                std::thread::sleep(IDLE_POLL);
            }
        }
        // Everything the hardware still believes is sounding is released before the port closes.
        worker.release_all();
    });

    RunningWorker {
        stop,
        handle: Some(handle),
        faulted,
        port_name,
    }
}

//! The player answers a local socket, so a machine can ask it what a person can see.
//!
//! # Why this exists
//!
//! Every debugging session before it went the same way: a person reports what the interface shows,
//! and the tests cannot see it — headless readbacks are *corrected by design* (a locked parameter
//! reads as the patch), the GUI harness has no audio, and the one honest oracle left was
//! screenshots and a settings file on a debounce. The base-vs-patch bug class was invisible to
//! every automated eye. This is the eye: the **live** player, asked directly, answering with the
//! same state the window draws plus the uncorrected readbacks the window never shows.
//!
//! # It ships, and it is how an assistant plays the collection
//!
//! **This is a product surface, not scaffolding.** It is built into the shipped player and stays
//! there. The point is that a person can hand MXM Player to an AI assistant and have it load an
//! instrument, write a pattern, move parameters, run the transport, render audio and read back
//! what the window shows — the whole instrument, in the same vocabulary a person uses. Debugging
//! is where it came from; being the machine interface to the product is what it is for.
//!
//! Everything the contract asks for follows from that second reading rather than the first. A
//! verb for every user-visible act (`tests/t10_cli_conformance.rs`) is what makes the instrument
//! wholly reachable, not merely well tested. `dump` answering with what the window draws is what
//! lets an assistant see what it is doing. `lock` going through the funnel a human edit goes
//! through is what stops one authoring a state the interface could not.
//!
//! Do not strip it from a release build or put it behind a development flag.
//!
//! # Shape
//!
//! One TCP listener on `127.0.0.1`, ephemeral port, written to `cli.json` beside the settings so a
//! client can find it. One line in, one JSON line out, connection closed. Commands run **on the GUI
//! thread** — a request is queued here and answered in [`crate::ui::PlayerApp::service`], because
//! everything it touches belongs to that thread. Worst-case latency is one idle frame (~100 ms),
//! which is nothing for a tool.
//!
//! Local only, deliberately: the bind address is the loopback and nothing else. Shipping it opens
//! nothing to the network — an assistant driving the player runs on the same machine as the player.
//! It is a local machine interface, not a remote-control protocol.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc;

/// One command from a client, with the channel its answer travels back on.
pub struct Request {
    pub line: String,
    /// The listener thread blocks on this until the GUI thread answers.
    pub reply: mpsc::Sender<String>,
}

/// Where the port is published, beside the settings file.
pub fn port_file(settings_path: &Path) -> std::path::PathBuf {
    settings_path.with_file_name("cli.json")
}

/// Starts the listener and returns the receiving end for the GUI thread.
///
/// Failure is reported, not fatal: a player that cannot open a socket is still a player, and the
/// CLI is a tool bolted to its side.
pub fn start(settings_path: &Path) -> Result<mpsc::Receiver<Request>, String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("could not bind the CLI: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("no local addr: {e}"))?
        .port();

    let file = port_file(settings_path);
    std::fs::write(
        &file,
        format!("{{\"port\": {port}, \"pid\": {}}}\n", std::process::id()),
    )
    .map_err(|e| format!("could not write {}: {e}", file.display()))?;

    let (tx, rx) = mpsc::channel::<Request>();
    std::thread::Builder::new()
        .name("mxm-cli".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // One request per connection; a client that sends garbage costs one closed socket.
                let _ = serve_one(stream, &tx);
            }
        })
        .map_err(|e| format!("could not spawn the CLI thread: {e}"))?;

    Ok(rx)
}

fn serve_one(stream: TcpStream, tx: &mpsc::Sender<Request>) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let (reply_tx, reply_rx) = mpsc::channel();
    if tx
        .send(Request {
            line: line.trim().to_owned(),
            reply: reply_tx,
        })
        .is_err()
    {
        return Ok(()); // the app is gone; nothing to answer with
    }

    // Blocks until the GUI thread services the request. A generous ceiling, because a wedged
    // player is exactly when somebody is asking questions — but not for ever.
    let answer = reply_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| "{\"error\": \"the player did not answer within 10s\"}".to_owned());

    let mut stream = stream;
    stream.write_all(answer.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

/// Connects to a running player found via its port file and runs one command.
///
/// This is the whole client: the `mxm-cli` binary is a thin wrapper around it, and a test can call
/// it directly.
pub fn run_command(settings_path: &Path, command: &str) -> Result<String, String> {
    let file = port_file(settings_path);
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("no running player found ({}): {e}", file.display()))?;
    let port = text
        .split("\"port\":")
        .nth(1)
        .and_then(|s| s.trim_start().split(&[',', '}'][..]).next())
        .and_then(|s| s.trim().parse::<u16>().ok())
        .ok_or_else(|| format!("{} is not a port file", file.display()))?;

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("player not listening on {port}: {e}"))?;
    stream
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).map_err(|e| e.to_string())?;
    Ok(answer.trim_end().to_owned())
}

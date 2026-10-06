"""Independent conformance check for the player's Standard MIDI File writer.

Our own reader agreeing with our own writer proves they share assumptions, not that either is
valid SMF. This parses a file the player wrote using **mxm.midifile**, a separate implementation
that shares no code with `sequencer::smf`, and asserts what it finds.

It also writes a `.mid` with that library for the player's reader to load, which is the other half
of the release gate — a foreign file, produced by something that is not us.

    pip install mxm.midifile
    python verify.py <written.mid> <foreign.mid>

Exits non-zero on any mismatch, so it can gate a release.
"""

import sys
from mxm.midifile import MidiInFile, MidiOutFile, MidiEvents

# What the player writes. Kept here rather than imported, so a change on the Rust side has to be
# reflected deliberately rather than silently agreed with.
EXPECTED_FORMAT = 0
EXPECTED_TRACKS = 1
EXPECTED_DIVISION = 96
TICKS_PER_STEP = 24
STEPS = 16
CHANNEL = 0
VELOCITY = 100


class Collect(MidiEvents):
    """Records everything the file says, so it can be checked rather than trusted."""

    def __init__(self):
        super().__init__()
        self.header_info = None
        self.tempo_us = None
        self.signature = None
        self.notes_on = []
        self.notes_off = []
        self.other = []

    def header(self, format=0, n_tracks=1, division=96):
        self.header_info = (format, n_tracks, division)

    def tempo(self, value):
        self.tempo_us = value

    def time_signature(self, nn, dd, cc, bb):
        self.signature = (nn, 2 ** dd)

    def note_on(self, channel=0, note=64, velocity=64, use_running_status=False):
        self.notes_on.append((self.abs_time(), channel, note, velocity))

    def note_off(self, channel=0, note=64, velocity=64, use_running_status=False):
        self.notes_off.append((self.abs_time(), channel, note, velocity))

    def continuous_controller(self, channel, controller, value, use_running_status=False):
        self.other.append("controller")

    def patch_change(self, channel, patch, use_running_status=False):
        self.other.append("patch change")


def fail(message):
    print(f"  FAIL  {message}")
    return 1


def check_written(path):
    """Parses a file the player wrote, and checks it against the specification."""
    print(f"Checking {path} with mxm.midifile")
    events = Collect()
    MidiInFile(events, path).read()
    problems = 0

    if events.header_info != (EXPECTED_FORMAT, EXPECTED_TRACKS, EXPECTED_DIVISION):
        problems += fail(
            f"header is {events.header_info}, expected "
            f"{(EXPECTED_FORMAT, EXPECTED_TRACKS, EXPECTED_DIVISION)}"
        )
    else:
        print(f"  ok    header: format {EXPECTED_FORMAT}, "
              f"{EXPECTED_TRACKS} track, division {EXPECTED_DIVISION}")

    if events.tempo_us is None:
        problems += fail("no tempo event")
    else:
        bpm = 60_000_000 / events.tempo_us
        print(f"  ok    tempo: {events.tempo_us} us/quarter = {bpm:.3f} BPM")

    if events.signature not in (None, (4, 4)):
        problems += fail(f"time signature is {events.signature}, expected 4/4")
    else:
        print("  ok    time signature: 4/4")

    if not events.notes_on:
        problems += fail("no notes at all")
    for time, channel, note, velocity in events.notes_on:
        if channel != CHANNEL:
            problems += fail(f"note on channel {channel}, expected {CHANNEL}")
            break
        if time % TICKS_PER_STEP != 0:
            problems += fail(f"note at tick {time} is not on the sixteenth grid")
            break
        if time >= TICKS_PER_STEP * STEPS:
            problems += fail(f"note at tick {time} is past the first bar")
            break
        if velocity != VELOCITY:
            problems += fail(f"velocity {velocity}, expected {VELOCITY}")
            break
    else:
        print(f"  ok    {len(events.notes_on)} notes, all on the grid, all channel {CHANNEL}")

    # Every note must be released, or a DAW would hang the note.
    open_notes = len(events.notes_on) - len(events.notes_off)
    if open_notes != 0:
        problems += fail(f"{open_notes} note(s) never released")
    else:
        print(f"  ok    every note released ({len(events.notes_off)} note-offs)")

    return problems


def write_foreign(path):
    """Writes a file with mxm.midifile, for the player's reader to load.

    Deliberately *not* shaped like ours: a different division, and a velocity we do not use, so the
    reader has to rescale the grid and report the velocity it cannot keep.
    """
    out = MidiOutFile(path)
    out.header(format=0, nTracks=1, division=480)
    out.start_of_track()
    out.update_time(0)
    out.tempo(int(60_000_000 / 110))
    out.update_time(0)
    out.time_signature(4, 2, 24, 8)

    # A C minor triad on step 1, then a note on step 9. 480/4 = 120 ticks per sixteenth.
    step = 120
    for note in (48, 51, 55):
        out.update_time(0)
        out.note_on(channel=0, note=note, velocity=72)
    out.update_time(step // 2)
    for note in (48, 51, 55):
        out.note_off(channel=0, note=note, velocity=0)
        out.update_time(0)

    out.update_time(step * 8 - step // 2)
    out.note_on(channel=0, note=60, velocity=72)
    out.update_time(step // 2)
    out.note_off(channel=0, note=60, velocity=0)

    out.update_time(0)
    out.end_of_track()
    out.eof()
    print(f"Wrote {path} with mxm.midifile (division 480, 110 BPM, velocity 72)")


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)

    problems = check_written(sys.argv[1])
    write_foreign(sys.argv[2])
    if problems:
        print(f"\n{problems} problem(s)")
        sys.exit(1)
    print("\nThe player's MIDI output is valid according to an independent implementation.")

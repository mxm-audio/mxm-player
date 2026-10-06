# MIDI conformance

Our reader agreeing with our writer proves they share assumptions, not that either produces valid
SMF. These check both directions against **[mxm.midifile](https://pypi.org/project/mxm.midifile/)**,
an independent Python implementation.

## Checking what we write

```bash
python -m venv env && env/bin/pip install mxm.midifile
python verify.py <a-file-the-player-saved.mid> foreign.mid
```

It asserts the header, tempo, time signature, grid alignment, channel and note pairing, and exits
non-zero on any mismatch — so it can gate a release. Run it whenever `sequencer::smf`'s writer
changes.

## Checking what we read

`foreign.mid` is written by the same script, using that library rather than ours, and is loaded by
`a_midi_file_from_an_independent_implementation_loads` in `t6_sequencer.rs`.

It is **deliberately awkward**: division 480 rather than our 96, so the reader has to rescale onto
the sixteenth grid, and velocity 72, which the model cannot keep and must therefore report. A file
shaped exactly like ours would have tested nothing.

Regenerate it by running `verify.py` again; it is committed so the Rust suite needs no Python.

## Not covered here: the WAV `acid` chunk

Exported `.wav` files carry an `acid` chunk with tempo and beat count. **Its interoperability is
unverified** — there is no DAW on the development machine, and no independent implementation of that
chunk to check against.

The design does not depend on it: the file name carries the tempo, and
`an_exported_wav_stays_valid_with_the_acid_chunk_present` proves a reader that has never heard of
`acid` still sees an ordinary, correct WAV.

To close it, export a file and drop it into a DAW that reads ACID metadata. If it tempo-matches
without being told the tempo, the chunk is right. If not, fix it or remove it — do not leave it in
and claim it works.

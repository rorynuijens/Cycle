#!/usr/bin/env python3
"""Make the 3D world's sounds from the recordings in data/sounds/sources.json.

Each bed is cut from its recording, folded into a seamless loop (its last
seconds cross-faded into its first), levelled to one loudness and written to
data/sounds/<bed>.ogg as 48 kHz stereo Vorbis. Each call — a goat's bleat, a
gull's cry — is cut from its recording as a few takes, faded in and out,
levelled and written to data/sounds/calls/<call>_<n>.ogg as 48 kHz mono
Vorbis. Both are what src/audio/beds.rs expects; data/sounds/CREDITS.md is
written from the same list.

The recordings themselves are not kept in the repository: download each one
from the page named in sources.json into one folder, then run this inside the
GNOME SDK, which has ffmpeg:

    flatpak run --filesystem=host --command=python3 org.gnome.Sdk//50 -I \\
        scripts/make_ambience.py <folder with the downloads>

"""

import array
import json
import math
import os
import subprocess
import sys

RATE = 48_000
CHANNELS = 2
# How long the end of a loop takes to cross into its start.
CROSS_S = 4.0
# Every loop is levelled to this loudness (RMS, dBFS): quiet beside the ride,
# loud enough to hear over a trainer.
LEVEL_DBFS = -24.0
# And never peaks above this.
PEAK = 0.89
# A call is levelled louder than a bed: it is heard over one, now and then.
CALL_DBFS = -20.0
# How long a call takes to fade in, and — unless its source says — out.
CALL_IN_S = 0.03
CALL_OUT_S = 0.2

HERE = os.path.dirname(os.path.abspath(__file__))
SOUNDS = os.path.join(HERE, "..", "data", "sounds")


def decode(path, start_s, length_s, highpass_hz, lowpass_hz=None, channels=CHANNELS):
    """`length_s` seconds of `path` from `start_s`, as interleaved floats.

    A low-pass takes the sound far off: the sea heard from inland.
    """
    filters = [f"highpass=f={highpass_hz}"] if highpass_hz else []
    if lowpass_hz:
        filters += [f"lowpass=f={lowpass_hz}"] * 2
    filters.append(f"aresample={RATE}")
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-ss", str(start_s), "-t", str(length_s), "-i", path,
         "-af", ",".join(filters), "-ac", str(channels), "-f", "f32le", "-"],
        check=True, capture_output=True).stdout
    pcm = array.array("f")
    pcm.frombytes(raw)
    return pcm


def fold(pcm, loop_frames, cross_frames):
    """A loop of `loop_frames` whose end runs straight into its start.

    `pcm` holds the loop plus `cross_frames` more. The loop is the recording
    from `cross_frames` on; over its last `cross_frames` it fades out while
    the recording's first `cross_frames` — which lead into the loop's start —
    fade in, at equal power.
    """
    out = array.array("f", pcm[cross_frames * CHANNELS:(cross_frames + loop_frames) * CHANNELS])
    tail = loop_frames - cross_frames
    for i in range(cross_frames):
        t = (i + 0.5) / cross_frames
        fade_in = math.sin(t * math.pi / 2)
        fade_out = math.cos(t * math.pi / 2)
        for c in range(CHANNELS):
            k = (tail + i) * CHANNELS + c
            out[k] = out[k] * fade_out + pcm[i * CHANNELS + c] * fade_in
    return out


def level(pcm, dbfs=LEVEL_DBFS):
    """`pcm` brought to `dbfs`, or lower if it would peak above PEAK."""
    rms = math.sqrt(sum(s * s for s in pcm) / len(pcm))
    peak = max(abs(s) for s in pcm)
    gain = min(10 ** (dbfs / 20) / rms, PEAK / peak)
    print(f"    rms {20 * math.log10(rms):.1f} dBFS, peak {peak:.2f}, gain {20 * math.log10(gain):+.1f} dB"
          + ("  (held by its peak)" if gain == PEAK / peak else ""))
    return array.array("f", (s * gain for s in pcm))


def fade(pcm, in_s, out_s):
    """`pcm` (mono) faded in over `in_s` and out over `out_s`, so it starts
    and stops without a click."""
    out = array.array("f", pcm)
    n = len(out)
    for i in range(min(n, int(in_s * RATE))):
        out[i] *= i / (in_s * RATE)
    k = min(n, int(out_s * RATE))
    for i in range(k):
        out[n - 1 - i] *= i / k
    return out


def encode(pcm, path, channels=CHANNELS):
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-f", "f32le", "-ar", str(RATE), "-ac", str(channels),
         "-i", "-", "-c:a", "libvorbis", "-q:a", "3", "-map_metadata", "-1", path],
        input=pcm.tobytes(), check=True)


def credits(sources):
    """CREDITS.md, from the same list the sounds are made from."""
    lines = [
        "# Sounds",
        "",
        "The sounds of the 3D world. Every one is cut from a recording released",
        "into the public domain — CC0, a public-domain dedication, or the work of a",
        "U.S. federal agency — so they can be bundled and changed freely; the",
        "recordists are thanked here and in the app's About dialog all the same.",
        "",
        "`sources.json` says which stretch of which recording each sound is; they are",
        "made from it by `scripts/make_ambience.py`, which also writes this file.",
        "",
        "| Sound | Recording | By | Licence |",
        "|---|---|---|---|",
    ]
    for bed, src in sources["beds"].items():
        lines.append(f"| `{bed}.ogg` | [{src['title']}]({src['page']}) | {src['author']} | {src['licence']} |")
    for call, src in sources["calls"].items():
        lines.append(f"| `calls/{call}_*.ogg` ({len(src['takes'])}) | [{src['title']}]({src['page']}) "
                     f"| {src['author']} | {src['licence']} |")
    return "\n".join(lines) + "\n"


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    downloads = sys.argv[1]
    with open(os.path.join(SOUNDS, "sources.json")) as f:
        sources = json.load(f)
    cross = int(CROSS_S * RATE)
    for bed, src in sources["beds"].items():
        print(bed, "←", src["file"])
        loop = int(src["length_s"] * RATE)
        pcm = decode(os.path.join(downloads, src["file"]), src.get("start_s", 0),
                     src["length_s"] + CROSS_S, src.get("highpass_hz", 60), src.get("lowpass_hz"))
        if len(pcm) < (loop + cross) * CHANNELS:
            sys.exit(f"  {src['file']} is shorter than {src['length_s'] + CROSS_S} s from {src.get('start_s', 0)} s")
        out = level(fold(pcm, loop, cross), LEVEL_DBFS + src.get("trim_db", 0))
        encode(out, os.path.join(SOUNDS, bed + ".ogg"))
    calls_dir = os.path.join(SOUNDS, "calls")
    os.makedirs(calls_dir, exist_ok=True)
    for old in os.listdir(calls_dir):
        os.remove(os.path.join(calls_dir, old))
    for call, src in sources["calls"].items():
        for n, (file, start_s, length_s) in enumerate(src["takes"], 1):
            print(f"{call}_{n} ← {file} at {start_s} s")
            pcm = decode(os.path.join(downloads, file), start_s, length_s, src.get("highpass_hz", 120),
                         channels=1)
            out = level(fade(pcm, src.get("fade_s", CALL_IN_S), src.get("fade_s", CALL_OUT_S)), CALL_DBFS)
            encode(out, os.path.join(calls_dir, f"{call}_{n}.ogg"), channels=1)
    with open(os.path.join(SOUNDS, "CREDITS.md"), "w") as f:
        f.write(credits(sources))


if __name__ == "__main__":
    main()

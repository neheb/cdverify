# cdverify

A simplified, terminal version of PerfectTUNES' AccurateRip checker. It scans a
music library, groups tracks into albums and verifies each album against the
[AccurateRip](http://www.accuraterip.com) database and the CUETools database
(CTDB), which can also repair rips with errors.

Results are grouped into the same four lists as PerfectTUNES:

| List | Meaning |
|------|---------|
| Tracks Corrupted | A file failed to decode or its FLAC MD5 doesn't match the audio |
| Ripping Errors | The disc is in AccurateRip but one or more tracks don't match |
| Accurately Ripped | Every track matches with at least `--min-confidence` |
| Cannot Check | Lossy/unsupported/non-CD audio, incomplete album, or not in the database |

## Install

Each [release](../../releases) has static binaries (no dependencies):

| Archive | For |
|---------|-----|
| `linux-amd64` | 64-bit x86 PCs |
| `linux-armv6` | Raspberry Pi 1 / Zero (also runs on later 32-bit ARM) |
| `linux-aarch64` | 64-bit ARM (Raspberry Pi 3/4/5, ARM servers) |
| `windows-x64` | 64-bit Windows 10/11 (`.zip`) |

```
tar xzf cdverify-*-linux-amd64.tar.gz
./cdverify-*-linux-amd64/cdverify ~/Music
```

Or build from source with `cargo build --release`. Repairs use the `flac`
command when it's installed, for the best compression; otherwise the built-in
encoder.

Releases are built by GitHub Actions when a `v*` tag is pushed
(`git tag v0.1.0 && git push origin v0.1.0`).

## Usage

```
cargo run --release -- ~/Music
```

Keys: `←/→`, `Tab` or `1`–`4` switch list · `↑/↓` select · `PgUp/PgDn` page through
albums · `c` copy the album's MusicBrainz URL · `r` repair · `e` export report ·
`q` quit. `Esc` only clears the status message.

Copying uses the Windows clipboard on Windows and `wl-copy` on Wayland,
otherwise the terminal's clipboard via OSC 52.
Omarchy's Super+C doesn't reach the app: in a terminal it becomes
Ctrl+Shift+C, which the terminal handles itself.

Mouse: click a list name or an album to select it. The wheel moves through albums over the album list and scrolls the tracks over the details pane. With mouse capture on, use Shift+drag to select text in most terminals.

Options:

- `--min-confidence N`: require confidence N (use 2 if you submit your own rips).
- `--no-tag`: ignore dBpoweramp `ACCURATERIPRESULT` tags and always decode.
- `--offset-range N`: pressing-offset search range in samples (default 2939, 0 disables it).
- `--skip-non-cd`: hide lossy and non-44.1 kHz/16-bit albums.
- `--threads N`: number of worker threads (default: number of CPUs).
- `--report PATH`: report path (JSON plus a `.txt` summary).
- `--headless`: no TUI. Prints one line per album and writes the report at the end.
- `--no-ctdb`: don't contact the CUETools database (no layout lookups or repair-data checks).
- `--invert`: use CTDB as the primary source. Albums whose audio matches a CTDB entry exactly are accurate; only the rest are checked against AccurateRip, and those that match neither land in Ripping Errors (with CTDB's repair verdict). Albums CTDB doesn't know (or can't be reached for) get AccurateRip's verdict.
- `--rescan`: re-verify every folder and ignore cached results (the cache is still updated).
- `--cache PATH`: results cache location (default `~/.cache/cdverify/cache.json`, or `%LOCALAPPDATA%\cdverify\cache.json` on Windows). Each run caches results for both normal and `--invert` mode (unless `--no-ctdb` is given), so switching modes doesn't re-verify. The AccurateRip and CTDB checks are shared between the two, costing at most one extra decode per album.

## How it works

- **Albums.** Each folder of audio files is split into albums by its ALBUM and DISCNUMBER tags. Tracks are ordered by TRACKNUMBER, or by file name when that tag is missing. An album must be complete: tracks numbered 1..n, matching TRACKTOTAL when present.
- **CD images.** A single file holding a whole disc is split into tracks by its CUE sheet: a FLAC `CUESHEET` block, a `CUESHEET` tag (foobar2000, CUETools), or a `.cue` file next to it that names it or shares its name. The sheet also gives any pregap before track 1 (hidden track one audio), so the disc ID needs no guessing for it. Repair writes a corrected copy of the whole image, keeping its metadata; copy an external `.cue` alongside it yourself.
- **Disc ID.** The TOC is rebuilt from the track lengths, with track 1 at LBA 0. That gives the AccurateRip disc IDs and the `dBAR-…bin` URL.
- **Checksums.** Each track's v1 and v2 checksums are computed and compared with every pressing in the database response.
- **MusicBrainz.** The same TOC gives the MusicBrainz disc ID, shown as a `musicbrainz.org/cdtoc/…` link listing the releases with that TOC. A `MUSICBRAINZ_DISCID` tag that disagrees with it is flagged, which usually means a wrong or incomplete album grouping. A CD can be in AccurateRip but have no disc ID submitted to MusicBrainz.
- **Discs whose TOC the files don't reveal.** On Enhanced CDs (data after the audio) and mixed-mode game CDs (data track 1), AccurateRip's ID depends on the data track, which the audio files don't contain. The same goes for discs whose track 1 starts after a pregap that rippers leave out. If the audio-only ID is unknown or doesn't fully match, the tool asks the [CUETools database](http://db.cuetools.net) for full TOCs of discs with the same audio tracks. It retries with each layout found and keeps the best fit. Only the TOC is sent. On a data-first disc, AccurateRip numbers its entries by disc track, so the last audio track has no entry and can't be verified. Such an album still counts as accurate if every other track matches.
- **Repair.** Albums with ripping errors are checked against CTDB automatically during the scan. The tool notes whether CTDB has the disc, then compares the rip with CTDB's parity data. The verdict then says whether the rip *matches CTDB exactly* (it's fine; AccurateRip just lacks that version of the disc), is *repairable*, or isn't. Press `r` on a repairable album to write a repaired copy, or pass `--repair DIR` to write them for every repairable album during the scan. The check works like this:
  1. Recomputes CTDB's Reed-Solomon parity for your rip (a port of CUETools' code, bit-compatible with CTDB).
  2. Finds the offset, downloads the repair data and works out the corrected samples.
  3. Accepts the fix only if the corrected audio's CRC32 matches CTDB's.
  4. Writes corrected copies of just the damaged tracks to `DIR` (default `~/accuraterip-repaired`), mirroring the library's folders. FLAC tags and pictures are kept.

  Your library is never modified. It encodes with `flac` if installed, otherwise with the built-in encoder. Each written file is decoded again and compared before it's kept. Repair can fix damaged samples (up to about a second of damage in total), but not a *sample slip*, where the drive lost sync and added or dropped samples. Those show up as accurate tracks on either side matching at different offsets; the tool points out the track to re-rip.
- **Pressing offsets.** If some tracks don't match, the tool computes the frame-450 checksum at every offset within ±range. Offsets where it matches the database get the full checksums recomputed at that offset. This finds rips of a different pressing of the same CD.
- **Formats.** Decoding uses [Symphonia](https://github.com/pdeljanov/Symphonia): FLAC, WAV, AIFF, and ALAC in `.m4a`.

## Results cache

Like PerfectTUNES, results are cached per folder so later runs only check what
changed. A folder is reused when all of its audio files have the same names,
sizes and modification times. Only final results are cached:

- Accurately Ripped albums.
- Cannot Check albums for permanent reasons: lossy, not CD quality, or incomplete.

Ripping errors, "not in database", corrupted tracks and failed lookups are checked again on every run. The database gains submissions over time, and a corruption seen over a network share may be a temporary read error. A cached Accurate result is also re-checked when it no longer meets `--min-confidence`, or when it came from a tag and `--no-tag` is given.

## Limitations

- **Data-track discs without a CTDB entry.** The data track's length can't be inferred from audio files, so such an album shows up as "not in database" or as ripping errors.
- **Pregaps.** A pregap before track 1 (sometimes with hidden audio) shifts the whole TOC. It's recovered through CTDB when CTDB knows the disc. If the pregap audio was ripped into track 1, the tracks no longer line up and the album can't be verified.
- **WavPack, APE, TAK, WMA.** Reported as "Cannot Check" because Symphonia can't decode them.

## License

Copyright (C) 2026 Rosen Penev

GPL-3.0-or-later; see [LICENSE](LICENSE).

The CTDB parity and repair code (`src/parity.rs`, `src/repair.rs`) is a port of
[CUETools](https://github.com/gchudov/cuetools.net) (Copyright (C) 2008-2025
Gregory S. Chudov), whose Reed-Solomon implementation is after Masayuki
Miyazaki. CUETools is licensed GPL-2.0-or-later, which allows this port to be
distributed under GPL-3.0-or-later. It has to stay compatible with CTDB's data,
so it follows CUETools closely.

## Acknowledgements

- [AccurateRip](http://www.accuraterip.com) (Illustrate) for the rip database.
- [CTDB / CUETools](http://cue.tools) for the TOC database, parity data and the
  repair algorithm.
- [MusicBrainz](https://musicbrainz.org) and libdiscid for disc IDs and test
  vectors.
- [ARver](https://github.com/arcctgx/ARver) for a clear reference of the
  AccurateRip checksums.

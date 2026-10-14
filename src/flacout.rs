// SPDX-License-Identifier: GPL-3.0-or-later

//! Writing repaired tracks: FLAC (keeping the original's metadata) or WAV.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::decode;

const STREAMINFO: u8 = 0;
#[cfg(test)]
const PADDING: u8 = 1;
const SEEKTABLE: u8 = 3;

/// Writes `pcm` (interleaved 16-bit stereo, 44.1 kHz) to `out` in the format
/// of `original`, verifies it decodes back to `pcm`, and returns the encoder.
pub fn write_track(original: &Path, out: &Path, pcm: &[i16]) -> Result<&'static str> {
    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let tmp = out.with_extension(format!("{ext}.part"));
    let encoder = match ext.as_str() {
        "flac" => {
            let (encoded, encoder) = encode_flac(pcm)?;
            let original_bytes = std::fs::read(original)?;
            std::fs::write(&tmp, transplant_metadata(&original_bytes, &encoded)?)?;
            encoder
        }
        "wav" => {
            std::fs::write(&tmp, wav_bytes(pcm))?;
            "wav"
        }
        _ => bail!("can only write repaired FLAC or WAV files"),
    };
    if let Err(e) = check_decodes_to(&tmp, pcm) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, out)?;
    Ok(encoder)
}

fn check_decodes_to(path: &Path, pcm: &[i16]) -> Result<()> {
    let mut i = 0usize;
    let mut mismatch = false;
    let d = decode::decode(path, true, |frames| {
        for &f in frames {
            for s in [f as u16 as i16, (f >> 16) as u16 as i16] {
                mismatch |= pcm.get(i) != Some(&s);
                i += 1;
            }
        }
    })?;
    if mismatch || i != pcm.len() || d.verify_ok == Some(false) {
        bail!("written file doesn't decode to the repaired audio");
    }
    Ok(())
}

/// Encodes with the reference `flac` binary when available, else flacenc.
fn encode_flac(pcm: &[i16]) -> Result<(Vec<u8>, &'static str)> {
    match encode_with_flac_binary(pcm) {
        Ok(bytes) => Ok((bytes, "flac")),
        Err(_) => Ok((encode_with_flacenc(pcm)?, "flacenc")),
    }
}

fn encode_with_flac_binary(pcm: &[i16]) -> Result<Vec<u8>> {
    // Encode to a file, not stdout: flac can only fill in STREAMINFO's
    // sample count and MD5 (and write a seek table) when it can seek.
    let tmp = std::env::temp_dir().join(format!(
        "cdverify-{}-{:?}.flac",
        std::process::id(),
        std::thread::current().id()
    ));
    let mut child = Command::new("flac")
        .args([
            "--silent",
            "--best",
            "--no-padding",
            "--force",
            "--force-raw-format",
            "--endian=little",
            "--sign=signed",
            "--channels=2",
            "--bps=16",
            "--sample-rate=44100",
            "-o",
        ])
        .arg(&tmp)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    let writer = std::thread::spawn(move || stdin.write_all(&bytes));
    let status = child.wait()?;
    writer.join().map_err(|_| anyhow!("writer panicked"))??;
    let result = match status.success() {
        true => std::fs::read(&tmp).map_err(Into::into),
        false => Err(anyhow!("flac exited with {status}")),
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

fn encode_with_flacenc(pcm: &[i16]) -> Result<Vec<u8>> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let samples: Vec<i32> = pcm.iter().map(|&s| s as i32).collect();
    let source = flacenc::source::MemSource::from_samples(&samples, 2, 16, 44100);
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| anyhow!("flacenc config: {e:?}"))?;
    // flacenc 0.5.1 writes a final frame shorter than the block size in a way
    // other decoders reject, so pick a block size that divides the track.
    // Tracks are whole CD sectors (588 frames); use up to 7 sectors (4116).
    let frames = pcm.len() / 2;
    let block_size = (1..=7)
        .rev()
        .map(|k| k * 588)
        .find(|b| frames.is_multiple_of(*b))
        .ok_or_else(|| anyhow!("track is not whole CD sectors"))?;
    let stream = flacenc::encode_with_fixed_block_size(&config, source, block_size)
        .map_err(|e| anyhow!("flacenc: {e:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| anyhow!("flacenc write: {e:?}"))?;
    Ok(sink.as_slice().to_vec())
}

struct Block<'a> {
    kind: u8,
    data: &'a [u8],
}

/// Splits a FLAC file into its metadata blocks and the audio frames.
fn parse_flac(bytes: &[u8]) -> Result<(Vec<Block<'_>>, &[u8])> {
    if bytes.get(..4) != Some(b"fLaC") {
        bail!("not a FLAC file");
    }
    let mut blocks = Vec::new();
    let mut p = 4;
    loop {
        let header = bytes.get(p..p + 4).context("truncated FLAC metadata")?;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        let data = bytes
            .get(p + 4..p + 4 + len)
            .context("truncated FLAC metadata block")?;
        blocks.push(Block { kind, data });
        p += 4 + len;
        if last {
            break;
        }
    }
    Ok((blocks, &bytes[p..]))
}

/// The new encoding's STREAMINFO, seek table and frames, with every other
/// metadata block (tags, pictures, padding, …) copied from the original.
fn transplant_metadata(original: &[u8], encoded: &[u8]) -> Result<Vec<u8>> {
    let (old_blocks, _) = parse_flac(original)?;
    let (new_blocks, frames) = parse_flac(encoded)?;
    let mut blocks: Vec<&Block<'_>> = Vec::new();
    blocks.extend(new_blocks.iter().filter(|b| b.kind == STREAMINFO));
    blocks.extend(new_blocks.iter().filter(|b| b.kind == SEEKTABLE));
    blocks.extend(
        old_blocks
            .iter()
            .filter(|b| b.kind != STREAMINFO && b.kind != SEEKTABLE),
    );
    if blocks.first().map(|b| b.kind) != Some(STREAMINFO) {
        bail!("encoder produced no STREAMINFO");
    }
    let mut out = b"fLaC".to_vec();
    for (i, b) in blocks.iter().enumerate() {
        let last = if i + 1 == blocks.len() { 0x80 } else { 0 };
        let len = (b.data.len() as u32).to_be_bytes();
        out.extend([last | b.kind, len[1], len[2], len[3]]);
        out.extend(b.data);
    }
    out.extend(frames);
    Ok(out)
}

fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend(b"RIFF");
    out.extend((36 + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(2u16.to_le_bytes());
    out.extend(44100u32.to_le_bytes());
    out.extend((44100u32 * 4).to_le_bytes());
    out.extend(4u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    out.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flac(blocks: &[(u8, &[u8])], frames: &[u8]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        for (i, (kind, data)) in blocks.iter().enumerate() {
            let last = if i + 1 == blocks.len() { 0x80 } else { 0 };
            let len = (data.len() as u32).to_be_bytes();
            out.extend([last | kind, len[1], len[2], len[3]]);
            out.extend(*data);
        }
        out.extend(frames);
        out
    }

    #[test]
    fn keeps_original_metadata() {
        let original = flac(
            &[
                (STREAMINFO, b"old-info"),
                (SEEKTABLE, b"old-seek"),
                (4, b"tags"),
                (6, b"pic"),
                (PADDING, b"\0\0"),
            ],
            b"old-frames",
        );
        let encoded = flac(
            &[(STREAMINFO, b"new-info"), (SEEKTABLE, b"new-seek")],
            b"new-frames",
        );
        let out = transplant_metadata(&original, &encoded).unwrap();
        let (blocks, frames) = parse_flac(&out).unwrap();
        let got: Vec<(u8, &[u8])> = blocks.iter().map(|b| (b.kind, b.data)).collect();
        assert_eq!(
            got,
            [
                (STREAMINFO, &b"new-info"[..]),
                (SEEKTABLE, b"new-seek"),
                (4, b"tags"),
                (6, b"pic"),
                (PADDING, b"\0\0")
            ]
        );
        assert_eq!(frames, b"new-frames");
    }

    #[test]
    fn encoders_round_trip() {
        let dir = std::env::temp_dir().join(format!("artui-flac-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 75 sectors and a bit, with a last block shorter than flacenc's default.
        let pcm: Vec<i16> = (0..588 * 75 * 2)
            .map(|i| ((i * 37) % 2000) as i16 - 1000)
            .collect();
        for (name, bytes) in [
            ("flacenc.flac", encode_with_flacenc(&pcm).unwrap()),
            (
                "binary.flac",
                encode_with_flac_binary(&pcm).unwrap_or_default(),
            ),
        ] {
            if bytes.is_empty() {
                continue; // no flac binary installed
            }
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let checked = check_decodes_to(&path, &pcm);
            assert!(checked.is_ok(), "{name}: {checked:?}");
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::remove_dir(&dir).unwrap();
    }
}

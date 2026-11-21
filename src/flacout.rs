// SPDX-License-Identifier: GPL-3.0-or-later

//! Writing repaired tracks: FLAC (keeping the original's metadata) or WAV.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::decode;

const STREAMINFO: u8 = 0;
#[cfg(test)]
const PADDING: u8 = 1;
const SEEKTABLE: u8 = 3;

/// A repaired track written and verified next to its destination, not yet
/// in place. Dropping it removes the work file.
pub struct Staged {
    tmp: TempFile,
    out: PathBuf,
}

impl Staged {
    /// Moves the file to its destination, and returns that path.
    pub fn commit(self) -> Result<PathBuf> {
        self.tmp.persist(&self.out)?;
        Ok(self.out)
    }
}

/// Writes `pcm` (interleaved 16-bit stereo, 44.1 kHz) for `out` in the format
/// of `original` and verifies it decodes back to `pcm`. Returns the staged
/// file, to be committed to `out`, and the encoder.
pub fn stage_track(original: &Path, out: &Path, pcm: &[i16]) -> Result<(Staged, &'static str)> {
    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "flac" && ext != "wav" {
        bail!("can only write repaired FLAC or WAV files");
    }
    // Work files sit next to the output, never in a shared temp folder.
    let tmp = TempFile::new(out, &format!("{ext}.part"))?;
    let encoder = match ext.as_str() {
        "flac" => {
            let (encoded, encoder) = encode_flac(pcm, out)?;
            let header = read_flac_header(original)?;
            let mut w = BufWriter::new(tmp.file()?);
            write_with_metadata(&header, &encoded, &mut w)?;
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            encoder
        }
        _ => {
            let mut w = BufWriter::new(tmp.file()?);
            write_wav(pcm, &mut w)?;
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            "wav"
        }
    };
    check_decodes_to(&tmp.path, pcm)?;
    let out = out.to_path_buf();
    Ok((Staged { tmp, out }, encoder))
}

/// A work file created fresh (never through an existing file or link) and
/// removed unless persisted.
struct TempFile {
    path: PathBuf,
    file: Option<File>,
}

impl TempFile {
    /// `<out>.<pid>-<n>.<suffix>`, created exclusively.
    fn new(out: &Path, suffix: &str) -> Result<TempFile> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = out.file_name().context("bad output file name")?;
        for _ in 0..100 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let mut file_name = name.to_os_string();
            file_name.push(format!(".{}-{n}.{suffix}", std::process::id()));
            let path = out.with_file_name(file_name);
            match File::options().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(TempFile {
                        path,
                        file: Some(file),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("couldn't create a work file next to {}", out.display())
    }

    /// The open file (only once).
    fn file(&self) -> Result<File> {
        match &self.file {
            Some(f) => Ok(f.try_clone()?),
            None => bail!("work file already closed"),
        }
    }

    fn persist(mut self, to: &Path) -> Result<()> {
        self.file = None;
        std::fs::rename(&self.path, to)?;
        self.path = PathBuf::new();
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        self.file = None;
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
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

/// Encodes with the reference `flac` binary when available, else flac-codec.
/// `out` is where the result goes; flac's work file is put next to it.
fn encode_flac(pcm: &[i16], out: &Path) -> Result<(Vec<u8>, &'static str)> {
    match encode_with_flac_binary(pcm, out) {
        Ok(bytes) => Ok((bytes, "flac")),
        Err(_) => Ok((encode_with_flac_codec(pcm)?, "flac-codec")),
    }
}

fn encode_with_flac_binary(pcm: &[i16], out: &Path) -> Result<Vec<u8>> {
    // Encode to a file, not stdout: flac can only fill in STREAMINFO's
    // sample count and MD5 (and write a seek table) when it can seek. The
    // file is created exclusively first; --force lets flac reuse it.
    let mut tmp = TempFile::new(out, "enc.flac")?;
    tmp.file = None;
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
        .arg(&tmp.path)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    // Fed in chunks, not as one byte copy of the whole (maybe CD-sized) image.
    let written = std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> std::io::Result<()> {
            let mut stdin = BufWriter::new(stdin);
            for chunk in pcm.chunks(1 << 16) {
                let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                stdin.write_all(&bytes)?;
            }
            stdin.flush()
        });
        let status = child.wait();
        (status, writer.join())
    });
    let status = written.0?;
    written.1.map_err(|_| anyhow!("writer panicked"))??;
    if !status.success() {
        bail!("flac exited with {status}");
    }
    Ok(std::fs::read(&tmp.path)?)
}

fn encode_with_flac_codec(pcm: &[i16]) -> Result<Vec<u8>> {
    use flac_codec::encode::{FlacSampleWriter, Options};

    let mut out = std::io::Cursor::new(Vec::new());
    let mut writer = FlacSampleWriter::new_cdda(
        &mut out,
        Options::best().no_padding(),
        Some(pcm.len() as u64),
    )?;
    // Converted a chunk at a time instead of copied whole.
    let mut block = Vec::with_capacity(1 << 16);
    for chunk in pcm.chunks(1 << 16) {
        block.clear();
        block.extend(chunk.iter().map(|&s| i32::from(s)));
        writer.write(&block)?;
    }
    writer.finalize()?;
    Ok(out.into_inner())
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

/// The `fLaC` marker and metadata blocks of a FLAC file, without reading
/// its audio.
fn read_flac_header(path: &Path) -> Result<Vec<u8>> {
    let mut f = std::io::BufReader::new(File::open(path)?);
    let mut out = vec![0u8; 4];
    f.read_exact(&mut out)?;
    loop {
        let mut header = [0u8; 4];
        f.read_exact(&mut header)
            .context("truncated FLAC metadata")?;
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        out.extend(header);
        // Grows with what's actually there, rather than trusting `len` up
        // front: a truncated file mustn't cost a 16 MiB allocation.
        let got = (&mut f).take(len as u64).read_to_end(&mut out)?;
        if got != len {
            bail!("truncated FLAC metadata block");
        }
        if header[0] & 0x80 != 0 {
            return Ok(out);
        }
    }
}

/// Writes the new encoding's STREAMINFO, seek table and frames, with every
/// other metadata block (tags, pictures, padding, …) copied from the
/// original's header.
fn write_with_metadata(original: &[u8], encoded: &[u8], out: &mut impl Write) -> Result<()> {
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
    out.write_all(b"fLaC")?;
    for (i, b) in blocks.iter().enumerate() {
        let last = if i + 1 == blocks.len() { 0x80 } else { 0 };
        let len = (b.data.len() as u32).to_be_bytes();
        out.write_all(&[last | b.kind, len[1], len[2], len[3]])?;
        out.write_all(b.data)?;
    }
    out.write_all(frames)?;
    Ok(())
}

fn write_wav(pcm: &[i16], out: &mut impl Write) -> Result<()> {
    let data_len = u32::try_from(pcm.len() * 2)
        .ok()
        .filter(|l| *l <= u32::MAX - 36)
        .context("too long for a WAV file")?;
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data_len).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?; // PCM
    out.write_all(&2u16.to_le_bytes())?;
    out.write_all(&44100u32.to_le_bytes())?;
    out.write_all(&(44100u32 * 4).to_le_bytes())?;
    out.write_all(&4u16.to_le_bytes())?;
    out.write_all(&16u16.to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&data_len.to_le_bytes())?;
    for chunk in pcm.chunks(1 << 16) {
        let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
        out.write_all(&bytes)?;
    }
    Ok(())
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
        let mut out = Vec::new();
        write_with_metadata(&original, &encoded, &mut out).unwrap();
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
        // 75 sectors: the last block is shorter than the encoders' 4096.
        let pcm: Vec<i16> = (0..588 * 75 * 2)
            .map(|i| ((i * 37) % 2000) as i16 - 1000)
            .collect();
        for (name, bytes) in [
            ("flac-codec.flac", encode_with_flac_codec(&pcm).unwrap()),
            (
                "binary.flac",
                encode_with_flac_binary(&pcm, &dir.join("binary.flac")).unwrap_or_default(),
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

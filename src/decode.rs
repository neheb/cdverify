// SPDX-License-Identifier: GPL-3.0-or-later

//! Decoding audio files to packed 16-bit stereo frames with Symphonia.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_FLAC, CODEC_ID_VORBIS};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use crate::scan::TrackInfo;

pub struct Opened {
    pub format: Box<dyn FormatReader>,
    pub decoder: Box<dyn AudioDecoder>,
    pub track_id: u32,
}

/// [`open`]'s error for a lossy stream, such as AAC in an `.m4a` file
/// (which may also hold lossless ALAC).
#[derive(Debug)]
pub struct LossyCodec;

impl std::fmt::Display for LossyCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lossy codec")
    }
}

impl std::error::Error for LossyCodec {}

/// Probes a file and creates a decoder for its default audio track.
pub fn open(path: &Path, verify: bool) -> Result<Opened> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // A large buffer keeps reads efficient on network shares.
    let mss = MediaSourceStream::new(
        Box::new(file),
        MediaSourceStreamOptions {
            buffer_len: 1 << 20,
        },
    );
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio track"))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("no audio codec parameters"))?;
    // Symphonia numbers every lossy codec from Vorbis up to FLAC, the first
    // lossless one.
    if (CODEC_ID_VORBIS..CODEC_ID_FLAC).contains(&params.codec) {
        return Err(LossyCodec.into());
    }
    let opts = AudioDecoderOptions::default().verify(verify);
    let decoder = symphonia::default::get_codecs().make_audio_decoder(params, &opts)?;
    let track_id = track.id;
    Ok(Opened {
        format,
        decoder,
        track_id,
    })
}

/// [`decode_album`]'s note for a track whose FLAC MD5 doesn't match its audio.
pub const MD5_MISMATCH: &str = "MD5 mismatch";

pub struct Decoded {
    pub frames: u64,
    /// `Some(false)` when the stream's embedded checksum (FLAC MD5) mismatched.
    pub verify_ok: Option<bool>,
}

/// Decodes a 16-bit stereo file, handing packed frames (`L | R << 16`) to `sink`.
/// Any decode error is fatal: for verification a damaged stream is corrupt.
pub fn decode(path: &Path, verify: bool, mut sink: impl FnMut(&[u32])) -> Result<Decoded> {
    let Opened {
        mut format,
        mut decoder,
        track_id,
    } = open(path, verify)?;
    let mut pcm: Vec<i16> = Vec::new();
    let mut packed: Vec<u32> = Vec::new();
    let mut frames = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = decoder.decode(&packet)?;
        if buf.spec().channels().count() != 2 {
            bail!("not stereo");
        }
        buf.copy_to_vec_interleaved(&mut pcm);
        packed.clear();
        packed.extend(
            pcm.as_chunks::<2>()
                .0
                .iter()
                .map(|lr| (lr[0] as u16 as u32) | ((lr[1] as u16 as u32) << 16)),
        );
        frames += packed.len() as u64;
        sink(&packed);
    }
    let verify_ok = decoder.finalize().verify_ok;
    Ok(Decoded { frames, verify_ok })
}

/// Decodes an album's tracks in order, each file once (a CD image holds
/// several tracks), handing `sink` each run of frames with its position in
/// the album: 0 is the start of track 1, so an image's pregap is skipped.
/// Returns, per track, why it's damaged, if it is.
pub fn decode_album(
    tracks: &[TrackInfo],
    verify: bool,
    mut sink: impl FnMut(u64, &[u32]),
) -> Vec<Option<String>> {
    let mut notes = vec![None; tracks.len()];
    let mut album_pos = 0u64;
    let mut k = 0;
    while k < tracks.len() {
        let path = &tracks[k].path;
        let end = tracks[k..]
            .iter()
            .position(|t| t.path != *path || t.image_start.is_none())
            .map_or(tracks.len(), |n| (k + n).max(k + 1));
        let skip = tracks[k].image_start.unwrap_or(0);
        let lens: Vec<u64> = tracks[k..end]
            .iter()
            .map(|t| t.frames.unwrap_or(0))
            .collect();
        let len: u64 = lens.iter().sum();
        let mut file_pos = 0u64;
        let outcome = decode(path, verify, |frames| {
            let n = frames.len() as u64;
            let a = skip.saturating_sub(file_pos).min(n) as usize;
            let b = (skip + len).saturating_sub(file_pos).min(n) as usize;
            if a < b {
                sink(album_pos + file_pos + a as u64 - skip, &frames[a..b]);
            }
            file_pos += n;
        });
        // Tracks from the one holding frame `at` of the file on.
        let from = |at: u64| {
            let mut track_end = skip;
            lens.iter()
                .position(|l| {
                    track_end += l;
                    track_end > at
                })
                .unwrap_or(lens.len() - 1)
        };
        let expected = skip + len;
        let (first, note) = match outcome {
            Err(e) => (from(file_pos), Some(format!("decode error: {e}"))),
            Ok(d) if d.verify_ok == Some(false) => (0, Some(MD5_MISMATCH.into())),
            Ok(d) if d.frames != expected => {
                let note = match end - k {
                    1 => format!("decoded {} frames, expected {}", d.frames, len),
                    _ => format!("image decoded to {} frames, expected {expected}", d.frames),
                };
                (from(d.frames.min(expected)), Some(note))
            }
            Ok(_) => (0, None),
        };
        if let Some(note) = note {
            for n in &mut notes[k + first..end] {
                *n = Some(note.clone());
            }
        }
        album_pos += len;
        k = end;
    }
    notes
}

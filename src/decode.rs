// SPDX-License-Identifier: GPL-3.0-or-later

//! Decoding audio files to packed 16-bit stereo frames with Symphonia.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

pub struct Opened {
    pub format: Box<dyn FormatReader>,
    pub decoder: Box<dyn AudioDecoder>,
    pub track_id: u32,
}

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
    let opts = AudioDecoderOptions::default().verify(verify);
    let decoder = symphonia::default::get_codecs().make_audio_decoder(params, &opts)?;
    let track_id = track.id;
    Ok(Opened {
        format,
        decoder,
        track_id,
    })
}

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

// SPDX-License-Identifier: GPL-3.0-or-later

//! AccurateRip disc identification from track lengths.
//!
//! Without a CUE sheet the TOC is reconstructed by assuming track 1 starts at
//! LBA 0 and every following track starts where the previous one ended.

use serde::Serialize;

/// Stereo samples ("frames") per CD sector.
pub const SECTOR_FRAMES: u64 = 588;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscId {
    pub ntracks: usize,
    pub id1: u32,
    pub id2: u32,
    pub cddb: u32,
}

impl DiscId {
    /// Builds the disc ID from the length of each track in sectors.
    pub fn from_track_sectors(sectors: &[u32]) -> DiscId {
        let offsets = audio_offsets(sectors);
        let leadout = sectors.iter().sum();
        DiscId::from_toc(&offsets, None, leadout)
    }

    /// Builds the disc ID of an Enhanced CD (CD-Extra): the audio tracks are
    /// followed by a data track at `data_start`, and `leadout` is the end of
    /// the data track.
    pub fn with_data_track(sectors: &[u32], data_start: u32, leadout: u32) -> DiscId {
        DiscId::from_toc(&audio_offsets(sectors), Some(data_start), leadout)
    }

    /// Builds the disc ID of a mixed-mode CD whose first track is data (e.g.
    /// game CDs): the audio tracks start at `audio_start`.
    pub fn with_leading_data_track(sectors: &[u32], audio_start: u32, leadout: u32) -> DiscId {
        let offsets: Vec<u32> = audio_offsets(sectors)
            .iter()
            .map(|o| o + audio_start)
            .collect();
        DiscId::from_toc(&offsets, Some(0), leadout)
    }

    /// Builds the disc ID of an audio CD whose track 1 starts at `start`
    /// (a pregap before track 1 that the rip doesn't contain).
    pub fn with_pregap(sectors: &[u32], start: u32) -> DiscId {
        let offsets: Vec<u32> = audio_offsets(sectors).iter().map(|o| o + start).collect();
        DiscId::from_toc(&offsets, None, start + sectors.iter().sum::<u32>())
    }

    /// AccurateRip's IDs: the two sums cover audio tracks and the lead-out,
    /// the CDDB ID covers every track including a data track.
    fn from_toc(offsets: &[u32], data_start: Option<u32>, leadout: u32) -> DiscId {
        let n = offsets.len();
        let mut id1 = 0u32;
        let mut id2 = 0u32;
        for (i, &off) in offsets.iter().enumerate() {
            id1 = id1.wrapping_add(off);
            id2 = id2.wrapping_add(off.max(1).wrapping_mul(i as u32 + 1));
        }
        id1 = id1.wrapping_add(leadout);
        id2 = id2.wrapping_add(leadout.max(1).wrapping_mul(n as u32 + 1));

        let all: Vec<u32> = offsets.iter().copied().chain(data_start).collect();
        let cddb_sum: u32 = all.iter().map(|&off| digit_sum((off + 150) / 75)).sum();
        let first = all.iter().copied().min().unwrap_or(0);
        let seconds = (leadout + 150) / 75 - (first + 150) / 75;
        let cddb = ((cddb_sum % 0xff) << 24) | (seconds << 8) | all.len() as u32;

        DiscId {
            ntracks: n,
            id1,
            id2,
            cddb,
        }
    }

    pub fn file_name(&self) -> String {
        format!(
            "dBAR-{:03}-{:08x}-{:08x}-{:08x}.bin",
            self.ntracks, self.id1, self.id2, self.cddb
        )
    }

    pub fn url(&self) -> String {
        format!(
            "http://www.accuraterip.com/accuraterip/{:x}/{:x}/{:x}/{}",
            self.id1 & 0xf,
            (self.id1 >> 4) & 0xf,
            (self.id1 >> 8) & 0xf,
            self.file_name()
        )
    }
}

/// MusicBrainz disc ID for the same reconstructed TOC: SHA-1 of the hex TOC
/// (first and last track, lead-out, 99 track offsets, all +150 sectors),
/// base64 with `.`, `_`, `-` in place of `+`, `/`, `=`.
///
/// `start` is where track 1 begins (0 unless there's an unripped pregap).
pub fn musicbrainz_id(sectors: &[u32], start: u32) -> String {
    use sha1::{Digest, Sha1};

    let mut offsets = [0u32; 100];
    let mut pos = 150 + start;
    for (i, &len) in sectors.iter().take(99).enumerate() {
        offsets[i + 1] = pos;
        pos += len;
    }
    offsets[0] = pos;
    let mut toc = format!("{:02X}{:02X}", 1, sectors.len());
    for off in offsets {
        toc.push_str(&format!("{off:08X}"));
    }
    mb_base64(&Sha1::digest(toc.as_bytes()))
}

fn mb_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('-');
            }
        }
    }
    out
}

/// Start of each track, with track 1 at LBA 0.
pub fn audio_offsets(sectors: &[u32]) -> Vec<u32> {
    sectors
        .iter()
        .scan(0u32, |pos, &len| {
            let start = *pos;
            *pos += len;
            Some(start)
        })
        .collect()
}

fn digit_sum(mut n: u32) -> u32 {
    let mut s = 0;
    while n > 0 {
        s += n % 10;
        n /= 10;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_track_ids() {
        // One track of 1000 sectors: offsets [0], leadout 1000.
        let id = DiscId::from_track_sectors(&[1000]);
        assert_eq!(id.id1, 1000);
        assert_eq!(id.id2, 1 + 1000 * 2);
        // cddb: digit_sum(150/75 = 2) = 2; seconds = 1150/75 - 2 = 13.
        assert_eq!(id.cddb, (2 << 24) | (13 << 8) | 1);
    }

    /// libdiscid's test/test_put.c: 22 tracks, lead-out 303602.
    fn libdiscid_sectors() -> Vec<u32> {
        let offsets = [
            150, 9700, 25887, 39297, 53795, 63735, 77517, 94877, 107270, 123552, 135522, 148422,
            161197, 174790, 192022, 205545, 218010, 228700, 239590, 255470, 266932, 288750, 303602,
        ];
        offsets.windows(2).map(|w| w[1] - w[0]).collect()
    }

    #[test]
    fn musicbrainz_matches_libdiscid() {
        assert_eq!(
            musicbrainz_id(&libdiscid_sectors(), 0),
            "xUp1F2NkfP8s8jaeFn_Av3jNEI4-"
        );
    }

    #[test]
    fn cddb_matches_libdiscid() {
        assert_eq!(
            DiscId::from_track_sectors(&libdiscid_sectors()).cddb,
            0x370fce16
        );
    }

    #[test]
    fn enhanced_cd_id() {
        // Dies Irae - Immolated: 9 audio tracks plus a data track, which
        // AccurateRip lists as dBAR-009-000bb659-00586ea9-7c0b2f0a.bin.
        let offsets = [
            0, 16600, 29617, 52414, 62615, 79100, 91114, 101535, 119784, 144061,
        ];
        let sectors: Vec<u32> = offsets.windows(2).map(|w| w[1] - w[0]).collect();
        // The data track follows the audio session's lead-out/lead-in gap of 11400 sectors.
        let id = DiscId::with_data_track(&sectors, 144061 + 11400, 214798);
        assert_eq!(id.file_name(), "dBAR-009-000bb659-00586ea9-7c0b2f0a.bin");
    }

    #[test]
    fn leading_data_track_id() {
        // Half-Life game CD: data track 1, then 27 audio tracks from sector
        // 134817. AccurateRip lists it as dBAR-027-005e6b02-060481bb-bc0f631c.bin.
        let offsets = [
            0, 9737, 19619, 24127, 31335, 38831, 40571, 41231, 48222, 56059, 62264, 71879, 74597,
            80066, 89099, 90315, 99609, 107138, 115809, 122179, 128520, 134566, 142793, 148597,
            156055, 158866, 160168, 160650,
        ];
        let sectors: Vec<u32> = offsets.windows(2).map(|w| w[1] - w[0]).collect();
        let id = DiscId::with_leading_data_track(&sectors, 134817, 134817 + 160650);
        assert_eq!(id.file_name(), "dBAR-027-005e6b02-060481bb-bc0f631c.bin");
    }

    #[test]
    fn pregap_ids() {
        // Cowboy Bebop: Vitaminless, track 1 at sector 37. AccurateRip lists it
        // as dBAR-008-00099162-003e0aa7-7106d808.bin; MusicBrainz knows the ID.
        let offsets = [0, 28280, 42580, 57333, 69760, 79038, 101515, 116798, 131405];
        let sectors: Vec<u32> = offsets.windows(2).map(|w| w[1] - w[0]).collect();
        let id = DiscId::with_pregap(&sectors, 37);
        assert_eq!(id.file_name(), "dBAR-008-00099162-003e0aa7-7106d808.bin");
        assert_eq!(musicbrainz_id(&sectors, 37), "9Ogj41zvgLsTmazs4l2NqkkkG8Q-");
    }

    #[test]
    fn url_layout() {
        let id = DiscId {
            ntracks: 12,
            id1: 0x0012_3abc,
            id2: 0x00ab_cdef,
            cddb: 0xa10b_c40c,
        };
        assert_eq!(
            id.url(),
            "http://www.accuraterip.com/accuraterip/c/b/a/dBAR-012-00123abc-00abcdef-a10bc40c.bin"
        );
    }
}

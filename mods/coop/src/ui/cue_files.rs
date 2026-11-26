// MIT License
//
// Copyright © 2026 Yume-13
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! The sound folder, `ndml/coop/sounds/` in the game folder: one replacement
//! sound per cue, named by [`Cue::file_name`].
//!
//! A file that loads plays in place of the cue's built-in notes. Only
//! `coop-ready.wav` is shipped, written out when absent. A file already there
//! is never overwritten. Every file is read once at the mod's start, so a cue
//! plays without disk access.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::Path;

use super::cues::Cue;

/// The folder under the mod's state folder (`ndml/coop/`).
pub const FOLDER: &str = "sounds";

/// A file larger than this is skipped (about 12 s of 16-bit stereo at
/// 44.1 kHz). Each file stays in memory until the menu hands it to the game.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The shipped partner-ready sound (`mods/coop/sounds/coop-ready.wav`).
pub const SHIPPED_READY: &[u8] = include_bytes!("../../sounds/coop-ready.wav");

/// The note written beside the files (never overwritten).
pub const README: &str = "\
Co-op mod sounds. A file here named after a cue plays in place of the mod's built-in cue:
  coop-ready.wav    the partner is ready and waiting for you (the shipped sound: CC0)
  coop-tick-3.wav   a count's 3 (built in: the game's own chime)
  coop-tick-2.wav   a count's 2
  coop-tick-1.wav   a count's 1
  coop-go.wav       a count's zero
Format: WAV, uncompressed PCM, 8 or 16 bit, mono or stereo, 8000 to 48000 Hz, at most 2 MB.
The files are read when the game starts. A file that is missing or does not load is skipped and
the built-in cue plays (the reason is in ndml/logs/ndml.log). The cue plays at the game's effects
volume, on your game only.
The mod never overwrites a file here: to get the shipped sound back, delete coop-ready.wav and
start the game again.
";

/// The note's file name.
pub const README_NAME: &str = "README.txt";

/// A PCM WAV file's layout, as the game's `audio_create_buffer_sound` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wav {
    pub rate: u32,
    pub channels: u16,
    /// 8 (unsigned) or 16 (signed).
    pub bits: u16,
    /// The samples' place in the file.
    pub data_offset: usize,
    pub data_len: usize,
}

impl Wav {
    /// Read the header; `Err` says why the file is not taken.
    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return Err("not a WAV file (no RIFF WAVE header)".into());
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let (mut fmt, mut data) = (None, None);
        let mut i = 12;
        while i + 8 <= b.len() {
            let size = usize::try_from(u32_at(i + 4)).unwrap_or(usize::MAX);
            let body = i + 8;
            let end = body.saturating_add(size).min(b.len());
            match &b[i..i + 4] {
                b"fmt " if end - body >= 16 => {
                    let mut tag = u16_at(body);
                    if tag == 0xFFFE && end - body >= 26 {
                        // WAVE_FORMAT_EXTENSIBLE: the subformat's first two bytes.
                        tag = u16_at(body + 24);
                    }
                    fmt = Some((tag, u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
                }
                b"data" => data = Some((body, end - body)),
                _ => {}
            }
            i = body.saturating_add(size).saturating_add(size & 1);
        }
        let Some((tag, channels, rate, bits)) = fmt else {
            return Err("no fmt chunk".into());
        };
        let Some((data_offset, len)) = data else {
            return Err("no data chunk".into());
        };
        if tag != 1 {
            return Err(format!(
                "format {tag}, not uncompressed PCM (1): convert it to 16-bit PCM WAV"
            ));
        }
        if !matches!(bits, 8 | 16) {
            return Err(format!("{bits}-bit samples (8 or 16 are read)"));
        }
        if !matches!(channels, 1 | 2) {
            return Err(format!("{channels} channels (mono or stereo are read)"));
        }
        if !(8000..=48000).contains(&rate) {
            return Err(format!("{rate} Hz (8000 to 48000 are read)"));
        }
        let frame = usize::from(channels * bits / 8);
        let data_len = len - len % frame;
        if data_len == 0 {
            return Err("no samples".into());
        }
        Ok(Self {
            rate,
            channels,
            bits,
            data_offset,
            data_len,
        })
    }

    /// Length in seconds.
    #[must_use]
    pub fn seconds(&self) -> f64 {
        let frame = usize::from(self.channels * self.bits / 8);
        #[allow(clippy::cast_precision_loss)] // at most MAX_BYTES
        let frames = (self.data_len / frame) as f64;
        frames / f64::from(self.rate)
    }
}

/// A cue's file, read and checked, waiting for the game to take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueFile {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    pub wav: Wav,
}

/// The folder's files by cue file name, and the lines to log about them.
#[derive(Debug, Default)]
pub struct Loaded {
    pub files: BTreeMap<&'static str, CueFile>,
    pub lines: Vec<String>,
}

/// Every name the folder is read for (each cue's [`Cue::file_name`]).
#[must_use]
pub fn names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Cue::ALL.iter().map(|c| c.file_name()).collect();
    v.dedup();
    v
}

/// The game sound made from `cue`'s file, else `None` (its built-in notes).
#[must_use]
pub fn lookup<V: Clone>(made: &BTreeMap<&'static str, V>, cue: Cue) -> Option<V> {
    made.get(cue.file_name()).cloned()
}

/// Make the folder, the shipped sound and the note where absent, then read
/// every cue's file. Never fails: a problem is a line in [`Loaded::lines`] and
/// the cue keeps its built-in notes.
#[must_use]
pub fn load(dir: &Path) -> Loaded {
    let mut out = Loaded::default();
    if let Err(e) = std::fs::create_dir_all(dir) {
        out.lines.push(format!(
            "{}: not made ({e}); the built-in cues play",
            crate::game::shown_path(dir)
        ));
        return out;
    }
    for (name, bytes) in [
        ("coop-ready.wav", SHIPPED_READY),
        (README_NAME, README.as_bytes()),
    ] {
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        match std::fs::write(&path, bytes) {
            Ok(()) => out.lines.push(format!(
                "{name} written ({} bytes, the shipped one)",
                bytes.len()
            )),
            Err(e) => out.lines.push(format!("{name} not written ({e})")),
        }
    }
    for name in names() {
        let file = format!("{name}.wav");
        let path = dir.join(&file);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let read = if meta.len() > MAX_BYTES {
            Err(format!("{} bytes, over {MAX_BYTES}", meta.len()))
        } else {
            std::fs::read(&path).map_err(|e| e.to_string())
        };
        match read.and_then(|bytes| Wav::parse(&bytes).map(|wav| (bytes, wav))) {
            Ok((bytes, wav)) => {
                out.lines.push(format!(
                    "{file} read: {:.2} s, {} Hz, {} ch, {} bit",
                    wav.seconds(),
                    wav.rate,
                    wav.channels,
                    wav.bits
                ));
                out.files.insert(name, CueFile { name, bytes, wav });
            }
            Err(why) => out
                .lines
                .push(format!("{file} skipped ({why}); the built-in cue plays")),
        }
    }
    out
}

/// A file into a game sound in the game's three steps: `create` a buffer,
/// `fill` it, make the `sound` over it. The buffer is `delete`d on every
/// failure after it was made and kept with the sound it backs.
pub fn buffer_sound_steps<B, S>(
    create: impl FnOnce() -> Option<B>,
    fill: impl FnOnce(&B) -> bool,
    sound: impl FnOnce(&B) -> Option<S>,
    delete: impl FnOnce(B),
) -> Result<S, &'static str> {
    let buf = create().ok_or("buffer_create gave no buffer")?;
    if !fill(&buf) {
        delete(buf);
        return Err("buffer_base64_decode_ext failed; its buffer deleted");
    }
    if let Some(s) = sound(&buf) {
        return Ok(s);
    }
    delete(buf);
    Err("audio_create_buffer_sound gave no sound; its buffer deleted")
}

/// Base64, as the game's `buffer_base64_decode` takes the file's bytes.
#[must_use]
pub fn base64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..4 {
            if k <= c.len() {
                s.push(char::from(A[((n >> (18 - 6 * k)) & 63) as usize]));
            } else {
                s.push('=');
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
        b.extend(b"LIST\x04\0\0\0abcd");
        b.extend(b"fmt \x10\0\0\0");
        let align = channels * bits / 8;
        for x in [tag, channels] {
            b.extend(x.to_le_bytes());
        }
        b.extend(rate.to_le_bytes());
        b.extend((rate * u32::from(align)).to_le_bytes());
        for x in [align, bits] {
            b.extend(x.to_le_bytes());
        }
        b.extend(b"data");
        b.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        b.extend(data);
        b
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/coop-cue-files")
            .join(name);
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_shipped_sound_is_a_wav_the_game_takes() {
        let w = Wav::parse(SHIPPED_READY).expect("the shipped coop-ready.wav");
        assert_eq!((w.rate, w.channels, w.bits), (44100, 2, 16));
        assert!((1.0..2.0).contains(&w.seconds()), "{}", w.seconds());
        assert!(SHIPPED_READY.len() as u64 <= MAX_BYTES);
    }

    #[test]
    fn a_wav_is_read_past_other_chunks_and_bad_ones_say_why() {
        let w = Wav::parse(&wav(1, 1, 22050, 16, &[0, 1, 2, 3, 4])).unwrap();
        assert_eq!((w.rate, w.channels, w.bits, w.data_len), (22050, 1, 16, 4));
        assert_eq!(
            &wav(1, 1, 22050, 16, &[9; 4])[w.data_offset..][..4],
            &[9; 4]
        );
        for (b, why) in [
            (b"ID3\x04 not a wav".to_vec(), "not a WAV"),
            (wav(3, 2, 44100, 32, &[0; 8]), "not uncompressed PCM"),
            (wav(1, 2, 44100, 24, &[0; 12]), "24-bit"),
            (wav(1, 6, 44100, 16, &[0; 12]), "6 channels"),
            (wav(1, 2, 96000, 16, &[0; 8]), "96000 Hz"),
            (wav(1, 2, 44100, 16, &[]), "no samples"),
            (b"RIFF\0\0\0\0WAVE".to_vec(), "no fmt"),
        ] {
            let e = Wav::parse(&b).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
    }

    #[test]
    fn the_folder_gets_the_shipped_sound_and_note_once_and_keeps_a_swap() {
        let d = scratch("ship");
        let l = load(&d);
        assert_eq!(
            std::fs::read(d.join("coop-ready.wav")).unwrap(),
            SHIPPED_READY
        );
        assert_eq!(
            std::fs::read_to_string(d.join(README_NAME)).unwrap(),
            README
        );
        assert_eq!(l.files.keys().copied().collect::<Vec<_>>(), ["coop-ready"]);
        assert!(
            l.lines
                .iter()
                .any(|x| x.starts_with("coop-ready.wav written"))
        );
        // The player's replacement: never overwritten, and it is what plays.
        let mine = wav(1, 1, 8000, 8, &[128; 800]);
        std::fs::write(d.join("coop-ready.wav"), &mine).unwrap();
        std::fs::write(d.join(README_NAME), "mine").unwrap();
        let l = load(&d);
        assert_eq!(l.files["coop-ready"].bytes, mine);
        assert_eq!(
            std::fs::read_to_string(d.join(README_NAME)).unwrap(),
            "mine"
        );
        assert!(
            !l.lines.iter().any(|x| x.contains("written")),
            "{:?}",
            l.lines
        );
    }

    #[test]
    fn every_cue_name_is_read_and_a_bad_file_falls_back() {
        let d = scratch("swaps");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("coop-tick-2.wav"), wav(1, 2, 48000, 16, &[0; 400])).unwrap();
        std::fs::write(d.join("coop-go.wav"), b"OggS not a wav").unwrap();
        std::fs::write(
            d.join("coop-tick-3.wav"),
            vec![0; usize::try_from(MAX_BYTES).unwrap() + 1],
        )
        .unwrap();
        std::fs::write(d.join("coop-ready.wav"), b"").unwrap();
        let l = load(&d);
        assert_eq!(l.files.keys().copied().collect::<Vec<_>>(), ["coop-tick-2"]);
        for (file, why) in [
            ("coop-go.wav", "not a WAV"),
            ("coop-tick-3.wav", "over"),
            ("coop-ready.wav", "not a WAV"),
        ] {
            assert!(
                l.lines.iter().any(|x| x.starts_with(file)
                    && x.contains(why)
                    && x.ends_with("the built-in cue plays")),
                "{file}: {:?}",
                l.lines
            );
        }
        assert_eq!(
            names(),
            [
                "coop-ready",
                "coop-tick-3",
                "coop-tick-2",
                "coop-tick-1",
                "coop-go"
            ]
        );
    }

    #[test]
    fn a_folder_that_cannot_be_made_leaves_the_built_in_cues() {
        let d = scratch("blocked");
        std::fs::create_dir_all(d.parent().unwrap()).unwrap();
        std::fs::write(&d, b"a file where the folder goes").unwrap();
        let l = load(&d);
        assert!(l.files.is_empty());
        assert!(l.lines[0].contains("not made"), "{:?}", l.lines);
        std::fs::remove_file(&d).unwrap();
    }

    #[test]
    fn each_cue_finds_its_own_file_and_only_it() {
        let made: BTreeMap<&'static str, u32> = [("coop-tick-1", 1), ("coop-ready", 2)].into();
        assert_eq!(lookup(&made, Cue::PartnerReady), Some(2));
        assert_eq!(lookup(&made, Cue::Tick(1)), Some(1));
        // Other numbers sound as 1, their file too.
        assert_eq!(lookup(&made, Cue::Tick(7)), Some(1));
        for c in [Cue::Tick(3), Cue::Tick(2), Cue::Go] {
            assert_eq!(lookup(&made, c), None, "{}: its notes", c.name());
        }
        assert_eq!(
            lookup(&BTreeMap::<&'static str, u32>::new(), Cue::PartnerReady),
            None
        );
    }

    /// A failure after the buffer was made deletes it once. A sound keeps it.
    #[test]
    fn a_refused_sound_frees_its_buffer() {
        use std::cell::RefCell;
        let deleted = RefCell::new(Vec::new());
        let del = |b: u32| deleted.borrow_mut().push(b);
        assert_eq!(
            buffer_sound_steps(|| Some(7), |_| true, |b| Some(*b * 10), del),
            Ok(70)
        );
        assert!(deleted.borrow().is_empty(), "kept with its sound");
        assert!(buffer_sound_steps(|| Some(8), |_| false, |_| Some(1), del).is_err());
        assert!(buffer_sound_steps(|| Some(9), |_| true, |_| None::<u32>, del).is_err());
        assert!(buffer_sound_steps(|| None, |_: &u32| true, |_| Some(1), del).is_err());
        assert_eq!(*deleted.borrow(), [8, 9], "each made buffer deleted once");
    }

    #[test]
    fn base64_matches_the_standard() {
        for (b, s) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foobar", "Zm9vYmFy"),
            (&[0xff, 0xfe, 0x00][..], "//4A"),
        ] {
            assert_eq!(base64(b), s);
        }
    }
}

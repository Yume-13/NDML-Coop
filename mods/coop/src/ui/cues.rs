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

//! Sound cues, pure: which of the game's sounds each cue plays, at what pitch
//! and when. `ui::glue::play_cue` plays a cue as this game's own sound, at the
//! player's effects volume. A file in the sound folder named after a cue
//! (`ui::cue_files`) plays in place of its notes.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

/// The partner-ready sound does not play again this soon after it played. A
/// held-back sound is not owed. The notice, the alert and the bar follow every
/// press.
pub const READY_SOUND_HOLD_OFF: Duration = Duration::from_secs(5);

/// When the partner-ready sound last played, on the wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadyHoldOff {
    played: Option<Instant>,
}

impl ReadyHoldOff {
    /// The sound asked for now. `Ok`: it plays and the hold-off starts again.
    /// `Err`: it is held back, with how long ago it last played.
    pub fn ask(&mut self, now: Instant) -> Result<(), Duration> {
        if let Some(t) = self.played {
            let ago = now.saturating_duration_since(t);
            if ago < READY_SOUND_HOLD_OFF {
                return Err(ago);
            }
        }
        self.played = Some(now);
        Ok(())
    }
}

/// A cue: an event the player should hear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// The partner is ready and waits for this player.
    PartnerReady,
    /// A count's 3, 2 or 1: one tick, higher at each number. Other numbers
    /// sound as 1.
    Tick(u8),
    /// A count reached zero. A count that is called off has no go.
    Go,
}

/// One note of a cue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Note {
    /// The game's sound asset, by name.
    pub sound: &'static str,
    /// `audio_sound_pitch` (1: as recorded).
    pub pitch: f64,
    /// After the cue's start.
    pub at: Duration,
}

/// The game's menu select sound.
pub const SELECT: &str = "cursorSelect";

/// The game's small bounty chime (0.55 s): the count's tick.
pub const TICK: &str = "bountySmall";

/// A note `ms` after the cue's start.
const fn note(sound: &'static str, pitch: f64, ms: u64) -> Note {
    Note {
        sound,
        pitch,
        at: Duration::from_millis(ms),
    }
}

/// [`Cue::Tick`] 3, 2, 1: rising steps of one sound.
const TICK_3: &[Note] = &[note(TICK, 0.84, 0)];
const TICK_2: &[Note] = &[note(TICK, 1.0, 0)];
const TICK_1: &[Note] = &[note(TICK, 1.19, 0)];

/// [`Cue::PartnerReady`] when `coop-ready.wav` does not load: the tick's chime
/// as a quick rising major arpeggio.
const PARTNER_READY: &[Note] = &[
    note(TICK, 1.0, 0),
    note(TICK, 1.26, 80),
    note(TICK, 1.5, 160),
];

/// [`Cue::Go`]: the tick's chime in a quick two-note finish, an octave above
/// tick 3.
const GO: &[Note] = &[note(TICK, 1.26, 0), note(TICK, 1.68, 90)];

impl Cue {
    /// Every cue. The start count's ticks are the resume count's.
    pub const ALL: &'static [Self] = &[
        Self::PartnerReady,
        Self::Tick(3),
        Self::Tick(2),
        Self::Tick(1),
        Self::Go,
    ];

    /// Where a player hears it, in words.
    #[must_use]
    pub fn heard_on(self) -> &'static str {
        match self {
            Self::PartnerReady => {
                "the pause screen, the upgrade screen, the new game screen, the menus, the end screen"
            }
            Self::Tick(_) => {
                "the resume count and the run's start count on both games, the respawn alert on the downed player's game"
            }
            Self::Go => {
                "the end of the resume count and of the run's start count on both games, the return after the respawn alert on the downed player's game"
            }
        }
    }

    /// The cue's notes, in time order.
    #[must_use]
    pub fn notes(self) -> &'static [Note] {
        match self {
            Self::PartnerReady => PARTNER_READY,
            Self::Tick(3) => TICK_3,
            Self::Tick(2) => TICK_2,
            Self::Tick(_) => TICK_1,
            Self::Go => GO,
        }
    }

    /// The cue's file in the sound folder (`<name>.wav`, `ui::cue_files`).
    #[must_use]
    pub fn file_name(self) -> &'static str {
        match self {
            Self::PartnerReady => "coop-ready",
            Self::Tick(3) => "coop-tick-3",
            Self::Tick(2) => "coop-tick-2",
            Self::Tick(_) => "coop-tick-1",
            Self::Go => "coop-go",
        }
    }

    /// The cue's name in log lines.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::PartnerReady => "partner ready",
            Self::Tick(3) => "count tick 3",
            Self::Tick(2) => "count tick 2",
            Self::Tick(_) => "count tick 1",
            Self::Go => "count go",
        }
    }
}

/// Notes still to play, by when (the glue's queue, pure).
#[derive(Debug, Default)]
pub struct Queue {
    due: Vec<(std::time::Instant, Note)>,
}

impl Queue {
    /// `cue` starts at `now`: every note is queued.
    pub fn start(&mut self, cue: Cue, now: std::time::Instant) {
        self.due
            .extend(cue.notes().iter().map(|n| (now + n.at, *n)));
        self.due.sort_by_key(|d| d.0);
    }

    /// The notes due at `now`, taken out of the queue.
    pub fn take_due(&mut self, now: std::time::Instant) -> Vec<Note> {
        let split = self.due.partition_point(|d| d.0 <= now);
        self.due.drain(..split).map(|d| d.1).collect()
    }

    /// Nothing queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.due.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_readies_rings_once_and_readies_apart_ring_each() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut h = ReadyHoldOff::default();
        // Five readies inside the hold-off: one sound.
        let rang: Vec<bool> = [0, 400, 900, 2100, 4900]
            .iter()
            .map(|&ms| h.ask(at(ms)).is_ok())
            .collect();
        assert_eq!(rang, [true, false, false, false, false]);
        assert_eq!(h.ask(at(4999)), Err(Duration::from_millis(4999)));
        // Counted from the sound that played, not from the held ones.
        assert!(h.ask(at(5000)).is_ok());
        // Two readies further apart than the hold-off: two sounds.
        let mut h = ReadyHoldOff::default();
        assert!(h.ask(at(0)).is_ok() && h.ask(at(7100)).is_ok());
        assert!(
            READY_SOUND_HOLD_OFF > Duration::from_millis(1350),
            "longer than the shipped sound"
        );
    }

    /// Every cue's notes start at once and come in time order.
    #[test]
    fn every_cue_starts_at_once_in_time_order() {
        for c in Cue::ALL {
            let n = c.notes();
            assert_eq!(n[0].at, Duration::ZERO, "{}", c.name());
            assert!(n.windows(2).all(|w| w[0].at <= w[1].at), "{}", c.name());
            assert!(n.iter().all(|n| n.pitch > 0.0 && !n.sound.is_empty()));
        }
    }

    #[test]
    fn every_cue_is_listed_once_with_notes_and_places() {
        for (i, c) in Cue::ALL.iter().enumerate() {
            assert!(!c.notes().is_empty() && !c.heard_on().is_empty());
            assert!(!Cue::ALL[..i].contains(c), "{} twice", c.name());
        }
    }

    /// The variants of `Cue`, read from this file's own source.
    fn variants(src: &str) -> Vec<String> {
        let body = src
            .split("pub enum Cue {\n")
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .expect("the enum Cue in cues.rs");
        body.lines()
            .filter_map(|l| l.strip_prefix("    "))
            .filter(|l| !l.starts_with([' ', '/', '#']))
            .map(|l| {
                l.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
            .filter(|v| !v.is_empty())
            .collect()
    }

    /// `Cue::ALL` holds every variant of the enum and every number a count says.
    #[test]
    fn every_variant_and_every_count_number_is_in_all() {
        use crate::rules::countdown::{ALERT_FROM, START_COUNT, secs_up};
        use crate::rules::machine::RESUME_COUNT;
        let src = include_str!("cues.rs").replace('\r', "");
        let shapes = "pub enum Cue {\n    /// A.\n    A,\n    #[doc(hidden)]\n    B(u8),\n    C {\n        x: u8,\n    },\n}\n";
        assert_eq!(variants(shapes), ["A", "B", "C"], "every variant shape");
        let vs = variants(&src);
        // The reader finds the known variants.
        assert!(
            ["PartnerReady", "Tick", "Go"]
                .iter()
                .all(|k| vs.iter().any(|v| v == k)),
            "the enum as read: {vs:?}"
        );
        let all = src
            .split("pub const ALL: &'static [Self] = &[")
            .nth(1)
            .and_then(|r| r.split("];").next())
            .expect("Cue::ALL in cues.rs");
        for v in &vs {
            assert!(
                all.contains(&format!("Self::{v}")),
                "Cue::{v} is not in Cue::ALL (its sound file would not be read)"
            );
        }
        let top = secs_up(START_COUNT)
            .max(secs_up(RESUME_COUNT))
            .max(ALERT_FROM);
        for n in 1..=top {
            assert!(Cue::ALL.contains(&Cue::Tick(n)), "the tick {n}");
        }
    }

    #[test]
    fn the_count_ticks_rise_and_are_not_the_ready_cue() {
        let p: Vec<f64> = [3, 2, 1]
            .iter()
            .map(|n| Cue::Tick(*n).notes()[0].pitch)
            .collect();
        assert!(p[0] < p[1] && p[1] < p[2]);
        // One note each; the ready cue's fallback has three.
        assert!([3, 2, 1].iter().all(|n| Cue::Tick(*n).notes().len() == 1));
        assert_eq!(Cue::PartnerReady.notes().len(), 3);
    }

    /// The go is not a tick and not the ready cue, and comes after the ticks in `Cue::ALL`.
    #[test]
    fn the_go_is_neither_a_tick_nor_the_ready_cue() {
        let go = Cue::Go.notes();
        for c in [Cue::PartnerReady, Cue::Tick(3), Cue::Tick(2), Cue::Tick(1)] {
            assert_ne!(go, c.notes(), "{}", c.name());
        }
        assert_ne!(Cue::PartnerReady.notes(), Cue::Tick(1).notes());
        // The go's notes and the ready cue's file.
        assert_eq!(go, [note(TICK, 1.26, 0), note(TICK, 1.68, 90)]);
        assert_eq!(Cue::PartnerReady.file_name(), "coop-ready");
        assert_eq!(Cue::ALL.last(), Some(&Cue::Go), "after tick 1");
    }

    #[test]
    fn the_queue_hands_out_notes_when_due() {
        let t = Instant::now();
        let mut q = Queue::default();
        q.start(Cue::Tick(2), t);
        q.start(Cue::Go, t + Duration::from_secs(1));
        assert_eq!(q.take_due(t), Cue::Tick(2).notes(), "at once");
        assert!(q.take_due(t + Duration::from_millis(999)).is_empty());
        let mut later = Vec::new();
        for ms in (1000..3000).step_by(10) {
            later.extend(q.take_due(t + Duration::from_millis(ms)));
        }
        assert_eq!(later, Cue::Go.notes(), "a second later, all of it");
        assert!(q.is_empty());
    }
}

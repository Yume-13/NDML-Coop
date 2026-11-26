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

//! The respawn's protection (pure): how long a ship that flies back in
//! cannot be hurt, and the watch that reports what it took in that time.
//!
//! At the fly-in's start `invulnerableTime` and `iFrames` are both set to
//! [`FRAMES`].
//! * `invulnerableTime > 0` turns away every damage path of the game's.
//! * `iFrames` gives the game's own look of a ship that cannot be hit.
//! * `oPlayer_Alarm_3` gives control back on the fly-in's first frame.

#![forbid(unsafe_code)]

/// The game's fly-in, in frames: `oPlayer_Alarm_3` sets `alarm[0] = 180`.
pub const FLY_IN: u32 = 180;
/// The frame of the fly-in on which control is back (`oPlayer_Alarm_3` sets
/// `oPowers.playerControl` itself).
pub const CONTROL_BACK: u32 = 0;
/// The protection floor after control is back: `invulnerableTime` and
/// `iFrames`.
pub const BEFORE_INVULNERABLE: u32 = 74;
pub const BEFORE_IFRAMES: u32 = 180;

/// The protected frames from the fly-in's start: the fly-in, or control
/// back plus the protection floor, whichever ends later.
pub const FRAMES: u32 = max(
    FLY_IN,
    CONTROL_BACK + max(BEFORE_INVULNERABLE, BEFORE_IFRAMES),
);

const fn max(a: u32, b: u32) -> u32 {
    if a > b { a } else { b }
}

/// Whether the ship is protected on `frame` (0: the fly-in's start).
#[must_use]
pub fn protected(frame: u32) -> bool {
    frame < FRAMES
}

/// What the ship took over the protected frames, reported at their end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    /// Frames watched (the ship's steps from the fly-in's start).
    pub frames: u32,
    pub hull: f64,
    pub shield: f64,
    pub lowest_hull: f64,
    pub lowest_shield: f64,
    /// `invulnerableTime` as read on the first and the last frame.
    pub first_invulnerable: f64,
    pub last_invulnerable: f64,
}

impl Report {
    /// Hurt inside the protected frames (the hull or the shield fell).
    #[must_use]
    pub fn hurt(&self) -> bool {
        self.lowest_hull < self.hull || self.lowest_shield < self.shield
    }
}

/// Watches the protected frames: one [`Watch::frame`] per ship step from
/// the fly-in's start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Watch {
    report: Report,
}

impl Watch {
    /// At the fly-in's start, with the ship's hull and shield then.
    #[must_use]
    pub fn start(hull: f64, shield: f64) -> Self {
        Self {
            report: Report {
                frames: 0,
                hull,
                shield,
                lowest_hull: hull,
                lowest_shield: shield,
                first_invulnerable: f64::NAN,
                last_invulnerable: f64::NAN,
            },
        }
    }

    /// One ship step (the first is the fly-in's own frame). Returns the
    /// report once the protected frames are over.
    pub fn frame(&mut self, hull: f64, shield: f64, invulnerable: f64) -> Option<Report> {
        let r = &mut self.report;
        if r.frames == 0 {
            r.first_invulnerable = invulnerable;
        }
        r.last_invulnerable = invulnerable;
        r.lowest_hull = r.lowest_hull.min(hull);
        r.lowest_shield = r.lowest_shield.min(shield);
        r.frames += 1;
        (!protected(r.frames)).then_some(*r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_covers_the_fly_in_and_the_protection_floor() {
        // Starts with the fly-in.
        assert!(protected(0));
        // The protection floor: its invulnerableTime end lies inside, its
        // iFrames end is the window's.
        assert!(protected(BEFORE_INVULNERABLE));
        assert!(protected(BEFORE_IFRAMES - 1));
        // The fly-in's end (collisions back) is the window's end too.
        assert!(protected(FLY_IN - 1));
        assert!(!protected(FLY_IN));
        assert!(!protected(CONTROL_BACK + BEFORE_IFRAMES));
        assert_eq!(FRAMES, 180);
    }

    #[test]
    fn the_watch_reports_at_the_end_with_the_lowest_reads() {
        let mut w = Watch::start(100.0, 50.0);
        let mut report = None;
        for f in 0..FRAMES {
            let shield = if f == 90 { 40.0 } else { 50.0 };
            let r = w.frame(100.0, shield, f64::from(FRAMES - f));
            assert_eq!(r.is_some(), f == FRAMES - 1, "frame {f}");
            report = report.or(r);
        }
        let r = report.unwrap();
        assert_eq!(r.frames, FRAMES);
        assert_eq!((r.first_invulnerable, r.last_invulnerable), (180.0, 1.0));
        assert_eq!(r.lowest_shield, 40.0);
        assert!(r.hurt());
        let mut w = Watch::start(100.0, 50.0);
        let r = (0..FRAMES).find_map(|_| w.frame(100.0, 50.0, 1.0)).unwrap();
        assert!(!r.hurt());
    }
}

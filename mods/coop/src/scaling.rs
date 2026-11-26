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

//! Enemy scaling and save protection: switches ([`Flags`], atomics, no lock) that the ticks
//! keep current and the hot hooks read.
//!
//! * Health: after `enemyInit`, `hull`, `hullMax` and `hullPrevious` times
//!   `enemy_health_mult` (a few later absolute writes undo it).
//! * Damage: `enemyDamageScaling`'s result times `enemy_damage_mult`.
//! * Whose values: the host's, sent in `SessionSettings` ([`session_mults`]).
//! * Saves: `triggerSaveRun` and `deleteRunFiles` skipped from a Start taken while linked
//!   (`start_held`) through the whole co-op run it makes (`save_protect`, kept after a
//!   disconnect), and while the host's run settings are held (`rules_held`).
//! * Leaderboards: no upload while linked or in such a run ([`upload_blocked`]).

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Switches the hot hooks read.
#[derive(Debug)]
pub struct Flags {
    scaling: AtomicBool,
    save_protect: AtomicBool,
    /// The host's run settings may be in the flags (`loadout`).
    rules_held: AtomicBool,
    /// A Start taken while linked waits for its run's first frame
    /// (`state::Core::start_held`).
    start_held: AtomicBool,
    linked: AtomicBool,
    health_mult: AtomicU64,
    damage_mult: AtomicU64,
    blocked_uploads: AtomicU32,
}

impl Flags {
    /// All off, with these multipliers for when scaling turns on.
    #[must_use]
    pub fn new(health_mult: f64, damage_mult: f64) -> Self {
        Self {
            scaling: AtomicBool::new(false),
            save_protect: AtomicBool::new(false),
            rules_held: AtomicBool::new(false),
            start_held: AtomicBool::new(false),
            linked: AtomicBool::new(false),
            health_mult: AtomicU64::new(health_mult.to_bits()),
            damage_mult: AtomicU64::new(damage_mult.to_bits()),
            blocked_uploads: AtomicU32::new(0),
        }
    }

    /// Whether a partner is linked now (from the ticks).
    pub fn set_linked(&self, linked: bool) {
        self.linked.store(linked, Ordering::Relaxed);
    }

    /// Whether a leaderboard upload must be stopped now ([`upload_blocked`]).
    #[must_use]
    pub fn uploads_blocked(&self) -> bool {
        upload_blocked(
            self.linked.load(Ordering::Relaxed),
            self.save_protect.load(Ordering::Relaxed),
        )
    }

    /// Count one stopped upload; the count so far, this one included.
    pub fn count_blocked_upload(&self) -> u32 {
        self.blocked_uploads.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The multipliers to apply from now on ([`session_mults`]).
    pub fn set_mults(&self, health: f64, damage: f64) {
        self.health_mult.store(health.to_bits(), Ordering::Relaxed);
        self.damage_mult.store(damage.to_bits(), Ordering::Relaxed);
    }

    /// Set both switches (from the ticks).
    pub fn set(&self, scaling: bool, save_protect: bool) {
        self.scaling.store(scaling, Ordering::Relaxed);
        self.save_protect.store(save_protect, Ordering::Relaxed);
    }

    /// The health multiplier while scaling is on.
    #[must_use]
    pub fn health_mult(&self) -> Option<f64> {
        let m = f64::from_bits(self.health_mult.load(Ordering::Relaxed));
        (self.scaling.load(Ordering::Relaxed) && is_active(m)).then_some(m)
    }

    /// The damage multiplier while scaling is on.
    #[must_use]
    pub fn damage_mult(&self) -> Option<f64> {
        let m = f64::from_bits(self.damage_mult.load(Ordering::Relaxed));
        (self.scaling.load(Ordering::Relaxed) && is_active(m)).then_some(m)
    }

    /// Whether saves must be skipped.
    #[must_use]
    pub fn save_protect(&self) -> bool {
        self.save_protect.load(Ordering::Relaxed)
    }

    /// Whether the host's run settings may be in the flags now.
    pub fn set_rules_held(&self, held: bool) {
        self.rules_held.store(held, Ordering::Relaxed);
    }

    /// Whether a Start taken while linked waits for its run.
    pub fn set_start_held(&self, held: bool) {
        self.start_held.store(held, Ordering::Relaxed);
    }

    /// Whether the resume file's writes are skipped: saves protected, a Start
    /// taken while linked, or the host's run settings held.
    #[must_use]
    pub fn resume_writes_off(&self) -> bool {
        self.save_protect()
            || self.start_held.load(Ordering::Relaxed)
            || self.rules_held.load(Ordering::Relaxed)
    }
}

/// The range a multiplier is clamped to, as the config file's are.
pub const MULT_RANGE: (f64, f64) = (0.1, 10.0);

/// The (health, damage) multipliers in effect: the host's when sent (clamped
/// to [`MULT_RANGE`]; not a number keeps `own`), else `own`.
#[must_use]
pub fn session_mults(own: (f64, f64), host: Option<(f32, f32)>) -> (f64, f64) {
    let Some((h, d)) = host else {
        return own;
    };
    let take = |v: f32, mine: f64| {
        let v = f64::from(v);
        if v.is_finite() {
            // The wire's f32 back to the file's decimals (1.8, not
            // 1.7999999523, which scales 240 to 431.99999).
            ((v * 1e4).round() / 1e4).clamp(MULT_RANGE.0, MULT_RANGE.1)
        } else {
            mine
        }
    };
    (take(h, own.0), take(d, own.1))
}

/// The respawn time from the host's `SessionSettings`, clamped to 5..600 s.
/// `None`: no valid seconds, keep the file's.
#[must_use]
pub fn host_respawn(secs: f32) -> Option<Duration> {
    let s = f64::from(secs);
    if !s.is_finite() || s <= 0.0 {
        return None;
    }
    let s = (s * 1e3).round() / 1e3;
    Some(Duration::from_secs_f64(s.clamp(5.0, 600.0)))
}

/// A multiplier worth applying (finite, positive, not 1).
#[must_use]
pub fn is_active(mult: f64) -> bool {
    mult.is_finite() && mult > 0.0 && (mult - 1.0).abs() > 1e-9
}

/// `value × mult` if both are usable numbers, else `None` (leave it alone).
#[must_use]
pub fn scaled(value: f64, mult: f64) -> Option<f64> {
    let v = value * mult;
    (value.is_finite() && v.is_finite()).then_some(v)
}

/// `(scaling, save_protect)`: scaling follows the live link, save protection
/// the latch `coop_run` (this run started linked).
#[must_use]
pub fn switches(linked: bool, in_run: bool, coop_run: bool) -> (bool, bool) {
    let scaling = linked && in_run && coop_run;
    let save_protect = in_run && coop_run;
    (scaling, save_protect)
}

/// No Steam leaderboard upload while linked or in a run that started linked.
#[must_use]
pub fn upload_blocked(linked: bool, coop_run: bool) -> bool {
    linked || coop_run
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_leaderboard_upload_in_a_coop_session() {
        // Solo, no partner: the game's own uploads.
        assert!(!upload_blocked(false, false));
        // Linked (menu, lobby, a run): none.
        assert!(upload_blocked(true, false));
        assert!(upload_blocked(true, true));
        // The partner left during a co-op run: still none, to its end.
        assert!(upload_blocked(false, true));
        let f = Flags::new(1.0, 1.0);
        assert!(!f.uploads_blocked());
        f.set_linked(true);
        assert!(f.uploads_blocked());
        f.set_linked(false);
        f.set(false, true);
        assert!(f.uploads_blocked(), "the co-op run's latch");
        f.set(false, false);
        assert!(!f.uploads_blocked());
        assert_eq!(f.count_blocked_upload(), 1);
        assert_eq!(f.count_blocked_upload(), 2);
    }

    #[test]
    fn flags_are_off_until_set() {
        let f = Flags::new(1.5, 1.0);
        assert_eq!(f.health_mult(), None);
        assert!(!f.save_protect());
        f.set(true, true);
        assert_eq!(f.health_mult(), Some(1.5));
        assert_eq!(f.damage_mult(), None, "1.0 is skipped");
        assert!(f.save_protect());
        f.set(false, false);
        assert_eq!(f.health_mult(), None);
    }

    #[test]
    fn the_resume_file_stays_off_while_the_hosts_settings_are_held() {
        let f = Flags::new(1.0, 1.0);
        assert!(!f.resume_writes_off());
        f.set(false, true);
        assert!(f.resume_writes_off(), "the co-op run latch");
        f.set(false, false);
        f.set_rules_held(true);
        assert!(f.resume_writes_off(), "held, not latched");
        assert!(!f.save_protect(), "the latch itself is unchanged");
        f.set_rules_held(false);
        assert!(!f.resume_writes_off());
        // A Start taken while linked, before its run's first tick.
        f.set_start_held(true);
        assert!(f.resume_writes_off(), "a linked Start, before its run");
        assert!(!f.save_protect());
        f.set_start_held(false);
        assert!(!f.resume_writes_off());
    }

    /// A joining game applies the host's multipliers once they arrived.
    #[test]
    fn the_hosts_multipliers_count() {
        let own = (1.0, 1.0);
        assert_eq!(session_mults(own, None), own);
        let (h, d) = session_mults(own, Some((1.8, 1.0)));
        assert!((h - 1.8).abs() < 1e-12 && (d - 1.0).abs() < 1e-12);
        assert_eq!(
            scaled(240.0, h),
            Some(432.0),
            "the file's 1.8, not the f32's"
        );
        let (h, d) = session_mults((1.8, 1.0), Some((1.0, 2.5)));
        assert!((h - 1.0).abs() < 1e-12 && (d - 2.5).abs() < 1e-12);
        // Clamped as the file's are; not a number: the file's stays.
        assert_eq!(session_mults(own, Some((50.0, 0.0))), (10.0, 0.1));
        assert_eq!(
            session_mults((1.8, 1.2), Some((f32::NAN, f32::INFINITY))),
            (1.8, 1.2)
        );
        // The flags take them at once.
        let f = Flags::new(1.0, 1.0);
        f.set(true, true);
        assert_eq!(f.health_mult(), None);
        f.set_mults(1.8, 1.25);
        assert_eq!(f.health_mult(), Some(1.8));
        assert_eq!(f.damage_mult(), Some(1.25));
    }

    /// The host's respawn time: clamped as the file's, garbage ignored.
    #[test]
    fn the_hosts_respawn_counts() {
        assert_eq!(host_respawn(45.0), Some(Duration::from_secs(45)));
        assert_eq!(host_respawn(12.5), Some(Duration::from_millis(12_500)));
        assert_eq!(host_respawn(1.0), Some(Duration::from_secs(5)));
        assert_eq!(host_respawn(1e9), Some(Duration::from_secs(600)));
        for bad in [0.0, -3.0, f32::NAN, f32::INFINITY] {
            assert_eq!(host_respawn(bad), None, "{bad}");
        }
    }

    #[test]
    fn scaling_math() {
        assert_eq!(scaled(100.0, 1.5), Some(150.0));
        assert_eq!(scaled(f64::NAN, 1.5), None);
        assert_eq!(scaled(1e308, 10.0), None);
        assert!(!is_active(1.0));
        assert!(!is_active(0.0));
        assert!(!is_active(f64::INFINITY));
        assert!(is_active(0.5));
    }

    #[test]
    fn switch_rules() {
        // Solo, no link: everything off (the unmodded game).
        assert_eq!(switches(false, true, false), (false, false));
        // Linked at the menu: nothing (the latch is per run).
        assert_eq!(switches(true, false, false), (false, false));
        // A co-op run: both on.
        assert_eq!(switches(true, true, true), (true, true));
        // The partner left mid-run: no more scaling, saves stay off.
        assert_eq!(switches(false, true, true), (false, true));
        // Linked in the middle of a solo run: still vanilla.
        assert_eq!(switches(true, true, false), (false, false));
    }
}

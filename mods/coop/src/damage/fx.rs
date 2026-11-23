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

//! Death effects in co-op: whose build fires for a kill (off in this build:
//! [`super::DamageSettings::kill_fx_on`]).
//!
//! * The killer is the world's kill credit (the peer whose report took the
//!   host's hull to 0, until the hull changes after it).
//! * On a death credited to a partner the host skips the killer-build parts
//!   ([`KILLER_BUILD`]) of the enemy's death; the partner's game ran its own.
//! * A contested kill (a partner's claim within [`CONTESTED_FRAMES`] of the
//!   host's kill) fires both builds ([`Fx::contested`]); an older claim
//!   counts as a late double fire ([`Fx::double_late`]).
//! * Every death-path part that reads `oPowers` is in [`KILLER_BUILD`] or
//!   [`DEATH_READS_KEPT`].

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ndml_net::msg::NetId;

/// Parts of an enemy's death that come from the killer's build, skipped on
/// the host for a death credited to a partner: (script, why).
pub const KILLER_BUILD: &[(&str, &str)] = &[
    (
        "spawnRupture",
        "Rupture's blast (`checkDestroy*` call it when `oPowers.rupture` is set)",
    ),
    (
        "degenerations",
        "in a death path only: Deflagration's last damage-over-time tick (`oPowers.deflagration`), \
         with the killer's burn stats",
    ),
    ("spawnPurge", "Deflagration's purge blast"),
    (
        "procEffectsOnDestroy",
        "on-kill procs: Warpath, Priority Zero, construct assembly",
    ),
    (
        "spontaneousGeneration",
        "Spontaneous Generation (`oPowers.spontaneousCount`)",
    ),
];

/// Death-path reads of `oPowers` that stay the host's: (script, why).
pub const DEATH_READS_KEPT: &[(&str, &str)] = &[
    (
        "checkDestroy",
        "its own reads (`rupture`, `deflagration`, `railgunBlastIndex`) only choose the parts above",
    ),
    ("checkDestroyReduced", "as `checkDestroy`"),
    ("checkDestroyNoExplosion", "as `checkDestroy`"),
    (
        "corrosiveSpread",
        "spreads the enemy's own corrosion stacks: on the host only the host's hits put them there",
    ),
    (
        "spawnTreasure",
        "the drops: the enemy's own death, credited once by the host (its chances, Research and \
         rerolls, are the host's build)",
    ),
];

/// A partner's claim this many running frames or less before a
/// host-credited death of the same enemy is a contested kill (a round trip).
pub const CONTESTED_FRAMES: u64 = 30;

/// The host's kill-effect counts (the credit itself is the world's
/// `last_damager`).
#[derive(Debug, Default)]
pub struct HostKills {
    /// Partners' claims: enemy -> the newest claim's number and the running
    /// frame it was first seen.
    claims: HashMap<NetId, (u16, u64)>,
    /// Running frames (a freeze does not count towards the window).
    frame: u64,
    pub fx: Fx,
}

/// Counters (host).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fx {
    /// Deaths credited to a partner, and killer-build parts skipped in them.
    pub partner_deaths: u64,
    pub skipped: u64,
    /// Host-credited deaths after a partner's claim within
    /// [`CONTESTED_FRAMES`] (both builds fired), and after an older one.
    pub contested: u64,
    pub double_late: u64,
    /// All deaths seen (the rate's base).
    pub deaths: u64,
}

impl HostKills {
    /// A running frame (the caller counts only those: frozen frames are not
    /// part of the window).
    pub fn frame(&mut self) {
        self.frame += 1;
    }

    /// A report entry claimed the kill of `net_id` with claim number `claim`
    /// (0: no claim). Reports repeat until acknowledged, so the same number
    /// again is the same claim, not a new one: its window keeps the frame
    /// the claim was first seen.
    pub fn claimed(&mut self, net_id: NetId, claim: u16) {
        if claim == 0 {
            return;
        }
        let frame = self.frame;
        let e = self.claims.entry(net_id).or_insert((claim, frame));
        if e.0 != claim {
            *e = (claim, frame);
        }
    }

    /// A killer-build part was skipped.
    pub fn skipped(&mut self) {
        self.fx.skipped += 1;
    }

    /// `net_id` died here (its death path ran); `partner`: its kill names a
    /// partner (the world's credit).
    pub fn died(&mut self, net_id: NetId, partner: bool) {
        self.fx.deaths += 1;
        let claim = self.claims.remove(&net_id);
        if partner {
            self.fx.partner_deaths += 1;
        } else if let Some((_, at)) = claim {
            if self.frame.saturating_sub(at) <= CONTESTED_FRAMES {
                self.fx.contested += 1;
            } else {
                self.fx.double_late += 1;
            }
        }
    }

    /// The enemy is gone without a death (despawned, run end).
    pub fn gone(&mut self, net_id: NetId) {
        self.claims.remove(&net_id);
    }

    /// The counts as one line.
    #[must_use]
    pub fn line(&self) -> String {
        let f = self.fx;
        #[allow(clippy::cast_precision_loss)] // counts
        let pct = |n: u64| 100.0 * n as f64 / f.deaths.max(1) as f64;
        format!(
            "kill effects: deaths {}, credited to a partner {} (killer-build parts skipped {}), contested {} ({:.1} %), double fires outside the window {}",
            f.deaths,
            f.partner_deaths,
            f.skipped,
            f.contested,
            pct(f.contested),
            f.double_late
        )
    }
}

/// Whether `script` (a `gml_Script_` name or its bare name) is a
/// killer-build part.
#[must_use]
pub fn is_killer_build(script: &str) -> bool {
    let bare = script.strip_prefix("gml_Script_").unwrap_or(script);
    KILLER_BUILD.iter().any(|(s, _)| *s == bare)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contested_within_a_round_trip_and_late_double_fires_are_told_apart() {
        let mut k = HostKills::default();
        k.frame();
        // The partner claimed, the host's own hit killed it 10 frames on.
        k.claimed(8, 1);
        for _ in 0..10 {
            k.frame();
        }
        k.died(8, false);
        assert_eq!((k.fx.contested, k.fx.double_late), (1, 0));
        // A claim long before (refused, then the host killed it): late.
        k.claimed(9, 1);
        for _ in 0..(CONTESTED_FRAMES + 5) {
            k.frame();
        }
        k.died(9, false);
        assert_eq!((k.fx.contested, k.fx.double_late), (1, 1));
        // A claim whose report was the kill: credited, not contested.
        k.claimed(10, 1);
        k.died(10, true);
        assert_eq!((k.fx.partner_deaths, k.fx.contested), (1, 1));
        assert!(k.line().contains("contested 1"));
    }

    #[test]
    fn a_repeated_claim_number_is_the_same_claim() {
        let mut k = HostKills::default();
        k.claimed(5, 0);
        k.died(5, false);
        assert_eq!((k.fx.contested, k.fx.double_late), (0, 0), "no claim");
        // Claim 1, resent until acked: its window runs from the first.
        k.claimed(6, 1);
        for _ in 0..(CONTESTED_FRAMES + 5) {
            k.frame();
            k.claimed(6, 1);
        }
        k.died(6, false);
        assert_eq!((k.fx.contested, k.fx.double_late), (0, 1));
        // A new number is a new claim.
        k.claimed(7, 1);
        for _ in 0..(CONTESTED_FRAMES + 5) {
            k.frame();
        }
        k.claimed(7, 2);
        k.died(7, false);
        assert_eq!((k.fx.contested, k.fx.double_late), (1, 1));
    }

    #[test]
    fn a_freeze_does_not_count_towards_the_window() {
        // The caller advances the window on running frames only: a claim,
        // a long freeze (no frames), then the host's own kill 10 running
        // frames after the claim is contested, not a late double.
        let mut k = HostKills::default();
        k.claimed(3, 1);
        for _ in 0..10 {
            k.frame();
        }
        k.died(3, false);
        assert_eq!((k.fx.contested, k.fx.double_late), (1, 0));
    }

    #[test]
    fn an_overdue_refusal_then_the_games_own_kill_keeps_the_credit() {
        // The credit is the world's (it stays with the last peer who
        // damaged the enemy through a refusal): the death counts as the
        // partner's, never as a double fire, however late.
        let mut k = HostKills::default();
        k.claimed(4, 1);
        for _ in 0..(CONTESTED_FRAMES * 20) {
            k.frame();
        }
        k.died(4, true);
        assert_eq!(
            (k.fx.partner_deaths, k.fx.double_late, k.fx.contested),
            (1, 0, 0)
        );
    }

    #[test]
    fn killer_build_names() {
        assert!(is_killer_build("gml_Script_spawnRupture"));
        assert!(is_killer_build("procEffectsOnDestroy"));
        assert!(!is_killer_build("gml_Script_spawnTreasure"));
        let kept: Vec<&str> = DEATH_READS_KEPT.iter().map(|(s, _)| *s).collect();
        assert!(KILLER_BUILD.iter().all(|(s, _)| !kept.contains(s)));
    }
}

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

//! Death effects in co-op: the game's on-enemy-death mods and whose build
//! fires for a kill.
//!
//! An on-death mod is in force for the run's kills if either player has it,
//! and it does not stack. By kind ([`ON_DEATH`], [`Kind`]):
//!
//! 1. A world effect: one per death, the killer's if the killer has it,
//!    otherwise the host's, otherwise a partner's ([`world_effect_by`]).
//! 2. An own-build effect: every death counts once for each player who has
//!    the mod, whoever made the kill.
//! 3. A status effect: it triggers from the stacks of the player who put the
//!    status on the enemy, for that player's build.
//!
//! The killer is the world's kill credit ([`HostKills`]). The contested-kill
//! counts are off ([`super::DamageSettings::kill_fx_on`]).
//!
//! Every part of a death path that reads `oPowers` (the player's build) has
//! a row.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ndml_net::msg::{NetId, PeerId};

/// How a part of an enemy's death follows the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Kind 1, a world effect: one per death if either player has it
    /// ([`world_effect_by`]). The co-op world glue runs it on one game: the
    /// host's own call, or the host skips it and says whose it is
    /// (`OnDeathMsg::Given`), and that player's game calls the script as its
    /// copy dies by the host's word, with its own build.
    World(World),
    /// Kind 2, an own-build effect: it runs on every game for every death
    /// (the host's enemy's own death there; a copy dying by the host's word,
    /// or by its own kill, on a joining game), each game with its own build:
    /// a player without the mod gets nothing from it.
    OwnBuild,
    /// Kind 3, from a status a player put on the enemy: each game for its
    /// own player's stacks on the enemy. A row says where it does not apply.
    OwnStatus,
    /// Not an on-death mod: it stays where the enemy dies (the host).
    Kept,
}

/// A world effect's handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct World {
    /// The `oPowers` variable that says the player has it (above 0.5).
    pub flag: &'static str,
    /// The game reads the flag before calling the part (in the death path):
    /// while only a partner has it the host's flag is lent for the death
    /// path's span, so the game's own test of the death says whether it
    /// applies, and the call is then handed over.
    pub lend: bool,
    /// The call's first argument is the player's own (an `oPowers`
    /// variable read on the game that plays it), not the host's.
    pub own_arg0: Option<&'static str>,
    /// The object the part makes. One the down rule destroys
    /// (`rules::downed::DESTROYED`: a construct) is never made for a player
    /// who is down ([`World::kept_from_the_down`]).
    pub makes: &'static str,
}

impl World {
    /// A player who is down does not play it: what it makes is destroyed by
    /// the down rule on the next run tick (a Spontaneous Generation swarm).
    /// An effect that is only a blast at the enemy's place still happens.
    #[must_use]
    pub fn kept_from_the_down(self) -> bool {
        crate::rules::downed::DESTROYED
            .iter()
            .any(|b| b.name == self.makes)
    }
}

/// One row: the part (bare script name), the mods it is, its kind, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub script: &'static str,
    pub mods: &'static str,
    pub kind: Kind,
    pub why: &'static str,
}

/// Every death-path part that reads the player's build ([`Kind`]). World
/// effects number 0.. in this order on the wire (`OnDeathMsg` parts):
/// append only.
pub const ON_DEATH: &[Part] = &[
    Part {
        script: "spawnRupture",
        mods: "Rupture",
        kind: Kind::World(World {
            flag: "rupture",
            lend: true,
            own_arg0: Some("railgunBlastIndex"),
            makes: "oBurstQuick",
        }),
        why: "an `oBurstQuick` blast of the player's build, called by the three death paths as \
              the dying enemy when `oPowers.rupture` is set and the enemy is of a kind that ruptures",
    },
    Part {
        script: "spontaneousGeneration",
        mods: "Spontaneous Generation",
        kind: Kind::World(World {
            flag: "spontaneousCount",
            lend: false,
            own_arg0: None,
            makes: "oConstructSwarm",
        }),
        why: "a chance (from the enemy's `hullMax` and the player's `spontaneousCount`) of an \
              `oConstructSwarm` of the player's at the enemy; `enemyDestroyed` calls it for every \
              death with on-destroy effects, the flag is read inside",
    },
    Part {
        script: "procEffectsOnDestroy",
        mods: "Warpath, Priority Zero",
        kind: Kind::OwnBuild,
        why: "Warpath's stack (`warpathStacks`, an `oWarpathStack` orbiting the ship) and Priority \
              Zero's construct assembly progress: the player's own counters",
    },
    Part {
        script: "corrosiveSpread",
        mods: "Corrosion",
        kind: Kind::OwnStatus,
        why: "spreads the enemy's corrosion stacks with the player's radius and duration: the \
              host's from its enemy, a joining player's own from its copy (copy-owned stacks; \
              `corrosion_glue`)",
    },
    Part {
        script: "degenerations",
        mods: "Deflagration",
        kind: Kind::OwnStatus,
        why: "in a death path only: Deflagration's last damage-over-time tick with the player's \
              burn stats. The host's from its enemy's burns (its own: a partner's never reach \
              it). A joining player's own burns on a host kill: not played (no death path runs \
              on the copy; a replay is per-mod code), and the copy's burn variables are not \
              copy-owned (a repair can carry the host's onto it)",
    },
    Part {
        script: "spawnPurge",
        mods: "Deflagration",
        kind: Kind::OwnStatus,
        why: "Deflagration's purge blast, from the enemy's `totalDegen`: as `degenerations`",
    },
    Part {
        script: "spawnTreasure",
        mods: "Research (body), Challenger",
        kind: Kind::Kept,
        why: "the shared drops (XP orbs, bounties, power-ups), rolled once by the host; its build \
              reads (Research's power-up chance and table, rerolls, Challenger's level-ups) are \
              the host's, a partner's would need per-mod code",
    },
    Part {
        script: "checkDestroy",
        mods: "",
        kind: Kind::Kept,
        why: "its own reads (`rupture`, `deflagration`, `railgunBlastIndex`) only choose the parts \
              above",
    },
    Part {
        script: "checkDestroyReduced",
        mods: "",
        kind: Kind::Kept,
        why: "as `checkDestroy`",
    },
    Part {
        script: "checkDestroyNoExplosion",
        mods: "",
        kind: Kind::Kept,
        why: "as `checkDestroy`",
    },
];

/// The world effects with their wire numbers.
pub fn world_parts() -> impl Iterator<Item = (u8, &'static Part, World)> {
    ON_DEATH
        .iter()
        .filter_map(|p| match p.kind {
            Kind::World(w) => Some((p, w)),
            _ => None,
        })
        .zip(0u8..)
        .map(|((p, w), i)| (i, p, w))
}

/// The own-build parts.
pub fn own_build_parts() -> impl Iterator<Item = &'static Part> {
    ON_DEATH.iter().filter(|p| p.kind == Kind::OwnBuild)
}

/// Whose world effect an enemy's death gets (kind 1): exactly one player's
/// if anyone has it.
///
/// * the killer's, when the killer is a partner with it;
/// * else the host's, when the host has it;
/// * else the partner's with the lowest id who has it.
///
/// For an effect `kept` from the down ([`World::kept_from_the_down`]) a
/// player who is down is passed over; if only downed players have it,
/// nobody plays it.
///
/// `killer`: the world's kill credit, or a partner whose confirmed claim on
/// the enemy is still open; `None` or the host: the host's own kill or the
/// world's. `with`: the partners whose last word was that they have it.
/// `down`: whether a player is down. `None`: nobody plays it.
#[must_use]
pub fn world_effect_by(
    host: PeerId,
    host_has: bool,
    killer: Option<PeerId>,
    with: &[PeerId],
    kept: bool,
    down: impl Fn(PeerId) -> bool,
) -> Option<PeerId> {
    let can = |p: PeerId| !(kept && down(p));
    if let Some(k) = killer.filter(|k| *k != host && with.contains(k) && can(*k)) {
        return Some(k);
    }
    if host_has && can(host) {
        return Some(host);
    }
    with.iter().copied().filter(|p| *p != host && can(*p)).min()
}

/// A partner's claim this many running frames or less before a
/// host-credited death of the same enemy is a contested kill (both builds
/// fire): about a round trip. Older: a double fire outside the window.
pub const CONTESTED_FRAMES: u64 = 30;

/// The host's kill-effect counts. The credit itself is the world's
/// `last_damager`, read when a death path begins and named by the kill's
/// `Despawn`. A refusal after the bound does not change it.
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
    /// Deaths credited to a partner.
    pub partner_deaths: u64,
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
            "kill effects: deaths {}, credited to a partner {}, contested {} ({:.1} %), double fires outside the window {}",
            f.deaths,
            f.partner_deaths,
            f.contested,
            pct(f.contested),
            f.double_late
        )
    }
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
    fn one_world_effect_per_death_if_either_player_has_it() {
        let (host, joining) = (PeerId::HOST, PeerId(1));
        let mine = Some(joining);
        // Neither has it: none, whoever killed.
        for killer in [None, mine] {
            assert_eq!(
                world_effect_by(host, false, killer, &[], false, |_| false),
                None
            );
        }
        // Only the host: the host's, for the joining player's kills too.
        for killer in [None, mine] {
            assert_eq!(
                world_effect_by(host, true, killer, &[], false, |_| false),
                Some(host)
            );
        }
        // Only the joining player: that player's, for the host's kills and
        // the world's too.
        for killer in [None, mine] {
            assert_eq!(
                world_effect_by(host, false, killer, &[joining], false, |_| false),
                Some(joining)
            );
        }
        // Both: the killer's, one.
        assert_eq!(
            world_effect_by(host, true, None, &[joining], false, |_| false),
            Some(host)
        );
        assert_eq!(
            world_effect_by(host, true, mine, &[joining], false, |_| false),
            Some(joining)
        );
        // The host as its own killer is the host's kill.
        assert_eq!(
            world_effect_by(host, true, Some(host), &[joining], false, |_| false),
            Some(host)
        );
    }

    #[test]
    fn world_effect_with_more_partners_goes_to_the_killer_then_the_host_then_the_lowest_id() {
        let (host, a, b) = (PeerId::HOST, PeerId(1), PeerId(2));
        // b killed it, b has it: b's.
        assert_eq!(
            world_effect_by(host, false, Some(b), &[a, b], false, |_| false),
            Some(b)
        );
        // a killed it without it, b has it, the host not: b's (one).
        assert_eq!(
            world_effect_by(host, false, Some(a), &[b], false, |_| false),
            Some(b)
        );
        // Nobody's kill, both partners have it: the lowest id.
        assert_eq!(
            world_effect_by(host, false, None, &[b, a], false, |_| false),
            Some(a)
        );
        // A stale word naming the host itself counts for nothing.
        assert_eq!(
            world_effect_by(host, false, None, &[host], false, |_| false),
            None
        );
    }

    /// Which world effects a downed player is passed over for: the ones
    /// making what the down rule destroys (the swarm), not a blast.
    #[test]
    fn a_downed_player_is_passed_over_only_for_what_the_down_rule_destroys() {
        let kept: Vec<(&str, bool)> = world_parts()
            .map(|(_, p, w)| (p.script, w.kept_from_the_down()))
            .collect();
        assert_eq!(
            kept,
            vec![("spawnRupture", false), ("spontaneousGeneration", true)]
        );
    }

    /// Every world effect, every case of who has it, who killed and who is
    /// down: never a downed player for a kept effect, otherwise the rule as
    /// before; once whenever a player who can play it has it.
    #[test]
    fn world_effects_with_a_player_down() {
        let (host, joining) = (PeerId::HOST, PeerId(1));
        for (_, part, w) in world_parts() {
            let kept = w.kept_from_the_down();
            for (host_down, joining_down) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let down = |p: PeerId| if p == host { host_down } else { joining_down };
                for (host_has, joining_has) in
                    [(false, false), (true, false), (false, true), (true, true)]
                {
                    let with: &[PeerId] = if joining_has { &[joining] } else { &[] };
                    for killer in [None, Some(joining)] {
                        let by = world_effect_by(host, host_has, killer, with, kept, down);
                        let up_has = (host_has && !host_down) || (joining_has && !joining_down);
                        let any_has = host_has || joining_has;
                        let case = format!(
                            "{} host down {host_down} joining down {joining_down} host has {host_has} joining has {joining_has} killer {killer:?}: {by:?}",
                            part.script
                        );
                        if kept {
                            assert!(by.is_none_or(|p| !down(p)), "{case}");
                            assert_eq!(by.is_some(), up_has, "{case}");
                        } else {
                            assert_eq!(
                                by,
                                world_effect_by(host, host_has, killer, with, false, |_| false),
                                "{case}"
                            );
                            assert_eq!(by.is_some(), any_has, "{case}");
                        }
                    }
                }
            }
        }
        // The swarm with both having it: the joining player's kill while
        // that player is down is the host's; the host down, the joining
        // player's.
        let kept = true;
        assert_eq!(
            world_effect_by(host, true, Some(joining), &[joining], kept, |p| p
                == joining),
            Some(host)
        );
        assert_eq!(
            world_effect_by(host, true, None, &[joining], kept, |p| p == host),
            Some(joining)
        );
    }

    #[test]
    fn on_death_table() {
        let names: Vec<&str> = ON_DEATH.iter().map(|p| p.script).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "one row per part");
        // The wire's parts are a u8 bit set.
        assert!(world_parts().count() <= 8);
        let world: Vec<(u8, &str)> = world_parts().map(|(i, p, _)| (i, p.script)).collect();
        assert_eq!(
            world,
            [(0, "spawnRupture"), (1, "spontaneousGeneration")],
            "append only: the wire numbers them"
        );
        assert_eq!(
            own_build_parts().map(|p| p.script).collect::<Vec<_>>(),
            ["procEffectsOnDestroy"]
        );
    }
}

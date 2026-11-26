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

//! Enemy wind-ups: what an enemy shows on itself before it attacks (a charge
//! warning, a charge-up graphic). Pure: the rule and its inputs; the glue
//! (`shots_glue.rs`) reads the game.
//!
//! A joining game's copy runs its own wind-ups on its own phase clock and
//! random draws, so they drift from the host's shots. The host's are shared
//! through the shot path; the copy's own are scratch (made, then removed
//! before anything draws them). An enemy's blasts go the same way
//! ([`blast_shared`]).
//!
//! A wind-up is an instance that a live enemy's own code makes during its
//! life and that is drawn on that enemy, with no hit of its own:
//!
//! * its object is [`eligible`] and its maker a [`candidate`];
//! * by the end of the frame it [`follows`] an enemy or a shared instance;
//! * it is not a continuous effect that each game makes itself ([`Overlaps`]).
//!
//! What a shared shot's ending makes is the host's wherever it is ([`free`]):
//! a joining game's copy of the shot is removed without its Destroy event.

#![forbid(unsafe_code)]

/// What an object is, for [`eligible`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one fact each, read by one rule
pub struct ObjectFacts {
    /// Not an enemy, pickup, marker or shot-family object (`Class::Other`).
    pub other: bool,
    /// A player's object by family name (`oPlayer*` in its ancestry).
    pub player_family: bool,
    /// Under a root that stays each game's own other than `oEffect`.
    pub local_root: bool,
    /// Under `oEffectPooled` (the game reuses them).
    pub pooled: bool,
    /// An object the host replicates as a dependency.
    pub dependency: bool,
}

/// Whether instances of an object can be wind-ups at all.
#[must_use]
pub fn eligible(o: ObjectFacts) -> bool {
    o.other && !o.player_family && !o.local_root && !o.pooled && !o.dependency
}

/// Who made an instance (the create's `self`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one fact each, read by one rule
pub struct Maker {
    /// A replicated enemy (host: tracked, so past its first tick; joining
    /// game: a copy).
    pub enemy: bool,
    /// Its set-up runs now (a joining game's copy being made, or not ticked
    /// yet): what it makes there is each game's own set-up.
    pub set_up: bool,
    /// Dying: its hull at 0 or below, or its death path or Destroy event
    /// running (a death's presentation is each game's own).
    pub dying: bool,
    /// A wind-up itself (noted, shared or scratch).
    pub windup: bool,
    /// A shared shot (host: shared; joining game: made from the host's,
    /// outside the joining player's own site) whose Destroy event runs now:
    /// its ending.
    pub ending: bool,
    /// A shared instance that is no shot and follows nothing (what a shot's
    /// ending made, or what that made): its makes are its lineage's.
    pub kin: bool,
    /// Dying, and what it makes now the other game's instance does not make
    /// itself ([`end_unseen`]).
    pub end_unseen: bool,
}

/// Whether a create of an [`eligible`] object by `m` is a wind-up candidate
/// (kept as one when it [`follows`] at the frame's end).
#[must_use]
pub fn candidate(m: Maker) -> bool {
    m.windup || free(m) || (m.enemy && !m.set_up && !m.dying)
}

/// Whether a maker is a shared shot's ending ([`Maker::ending`]): a shared
/// shot (`shared_shot`) whose Destroy event runs now (`in_destroy`), not
/// inside the joining player's own site (`own_player_site`, always false on
/// the host). What the joining player ends there is its own.
#[must_use]
pub fn ending(shared_shot: bool, in_destroy: bool, own_player_site: bool) -> bool {
    shared_shot && in_destroy && !own_player_site
}

/// Whether a maker is of a shared shot's ending's lineage ([`Maker::kin`]):
/// a shared instance that is no shot (`shared_non_shot`: an eligible object
/// the host sent) and follows nothing; one that follows an enemy is a
/// wind-up, whose makes keep the follow test.
#[must_use]
pub fn kin(shared_non_shot: bool, follows: bool) -> bool {
    shared_non_shot && !follows
}

/// Whether a candidate by `m` is the host's wherever it is: made by a
/// shared shot's ending or by what that made. It skips the follow and
/// continuous-effect tests.
#[must_use]
pub fn free(m: Maker) -> bool {
    m.ending || m.kin
}

/// What an instance's `leader` or `shooter` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lead {
    /// A replicated enemy (its copy on a joining game).
    Enemy,
    /// A shared attack or a wind-up (made or scratch on a joining game).
    Shared,
    /// Something else (the player, a controller, an effect).
    Other,
    /// Nothing (`noone`, unset, gone).
    Nothing,
}

/// Whether an instance with these leads is drawn on an enemy (a wind-up).
#[must_use]
pub fn follows(leads: &[Lead]) -> bool {
    leads
        .iter()
        .any(|l| matches!(l, Lead::Enemy | Lead::Shared))
}

/// Quick repeats after which an object of an enemy type is a continuous
/// effect.
pub const CONTINUOUS_AFTER: u32 = 3;

/// A repeat within this many frames (half a second) of the maker's last make
/// of the object is quick. Makes in the same frame are no repeat.
pub const QUICK_FRAMES: u64 = 30;

/// "One at a time": per maker instance and object, the frame of its last
/// candidate; per enemy type and object, how often one came again within
/// [`QUICK_FRAMES`]. Made times only, so a game that removed its own (the
/// joining game's scratch) counts as the host does.
#[derive(Debug, Default)]
pub struct Overlaps {
    last: std::collections::HashMap<(usize, u32), u64>,
    count: std::collections::HashMap<(u32, u32), u32>,
    /// Per enemy type and object: made in a frame its maker made an enemy
    /// shot in, and made at all ([`Self::note_attack`]).
    attack: std::collections::HashMap<(u32, u32), (u32, u32)>,
}

/// Makes of an object by an enemy type after which [`Overlaps::attack_bound`]
/// can say yes: two Juggernaut volleys (four bursts each).
pub const ATTACK_BOUND_AFTER: u32 = 8;
/// The share of those makes (tenths) that must be in a frame their maker
/// fired in. Each game decides from its own creates, and both must decide
/// alike. A volley's bursts are all made in the frame of their shots.
pub const ATTACK_BOUND_TENTHS: u32 = 9;

impl Overlaps {
    /// A new candidate of `obj` by `maker` (of object `maker_obj`) in
    /// `frame`. Returns whether `obj` is now a continuous effect of that
    /// enemy type.
    pub fn note(&mut self, maker: usize, maker_obj: u32, obj: u32, frame: u64) -> bool {
        if let Some(&f) = self.last.get(&(maker, obj))
            && f < frame
            && frame - f <= QUICK_FRAMES
        {
            *self.count.entry((maker_obj, obj)).or_default() += 1;
        }
        self.last.insert((maker, obj), frame);
        if self.last.len() > 4096 {
            self.last.retain(|_, f| frame - *f <= QUICK_FRAMES);
        }
        self.continuous(maker_obj, obj)
    }

    /// A make of `obj` by an enemy of type `maker_obj` this frame, and
    /// whether that maker made an enemy shot (shot, blast or beam) in the
    /// same frame. Returns whether `obj` now comes with that type's attacks.
    pub fn note_attack(&mut self, maker_obj: u32, obj: u32, with_shot: bool) -> bool {
        let e = self.attack.entry((maker_obj, obj)).or_default();
        e.0 += u32::from(with_shot);
        e.1 += 1;
        self.attack_bound(maker_obj, obj)
    }

    /// Whether `obj` comes with the enemy type `maker_obj`'s attacks: after
    /// [`ATTACK_BOUND_AFTER`] makes, at least [`ATTACK_BOUND_TENTHS`] tenths
    /// of them in a frame its maker made an enemy shot in. A volley's bursts
    /// always are; thrusters only where a frame of thrust meets a frame of
    /// fire.
    #[must_use]
    pub fn attack_bound(&self, maker_obj: u32, obj: u32) -> bool {
        let (with, all) = self.attack_counts(maker_obj, obj);
        all >= ATTACK_BOUND_AFTER && 10 * with >= ATTACK_BOUND_TENTHS * all
    }

    /// Makes of `obj` by the enemy type `maker_obj` in a frame it fired in,
    /// and in all (the log line's numbers).
    #[must_use]
    pub fn attack_counts(&self, maker_obj: u32, obj: u32) -> (u32, u32) {
        self.attack
            .get(&(maker_obj, obj))
            .copied()
            .unwrap_or_default()
    }

    /// Whether `obj` made by the enemy type `maker_obj` is each game's own:
    /// continuous, and its makes so far ([`ATTACK_BOUND_AFTER`] at least)
    /// show it does not come with its attacks. Until then it stays the
    /// host's.
    #[must_use]
    pub fn own(&self, maker_obj: u32, obj: u32) -> bool {
        self.continuous(maker_obj, obj)
            && self.attack_counts(maker_obj, obj).1 >= ATTACK_BOUND_AFTER
            && !self.attack_bound(maker_obj, obj)
    }

    /// Whether `obj` is a continuous effect of the enemy type `maker_obj`.
    #[must_use]
    pub fn continuous(&self, maker_obj: u32, obj: u32) -> bool {
        self.count
            .get(&(maker_obj, obj))
            .is_some_and(|n| *n >= CONTINUOUS_AFTER)
    }

    /// Forget everything (a run starts or ends).
    pub fn clear(&mut self) {
        self.last.clear();
        self.count.clear();
        self.attack.clear();
    }
}

/// The blasts' kind (`shots::KINDS` index).
pub const BLAST_KIND: u8 = 1;

/// Whether an enemy blast made now is the host's whoever made it: a blast
/// (`kind`) a live enemy makes past its set-up, or one a dying enemy makes
/// where the other game's instance does not ([`end_unseen`]).
#[must_use]
pub fn blast_shared(kind: u8, m: Maker) -> bool {
    kind == BLAST_KIND && m.enemy && !m.set_up && (!m.dying || m.end_unseen)
}

/// The end rule: whether what a dying enemy makes now is made where the
/// other game's instance makes nothing of the kind.
///
/// * In its Destroy event (`in_destroy`): both games run it. Each game's own.
/// * Elsewhere (the step that ends it, a script of its death path): the
///   copy runs none of it at the host's word, so it is the host's. Not when
///   the death is the joining player's predicted kill (`partner_kill`): the
///   copy then ran its whole death path itself.
#[must_use]
pub fn end_unseen(in_destroy: bool, partner_kill: bool) -> bool {
    !in_destroy && !partner_kill
}

#[cfg(test)]
mod tests {
    use super::*;

    const EFFECT: ObjectFacts = ObjectFacts {
        other: true,
        player_family: false,
        local_root: false,
        pooled: false,
        dependency: false,
    };

    #[test]
    fn only_hitless_objects_of_no_local_family_are_eligible() {
        assert!(eligible(EFFECT), "an effect or a parentless warning");
        for o in [
            ObjectFacts {
                other: false,
                ..EFFECT
            },
            ObjectFacts {
                player_family: true,
                ..EFFECT
            },
            ObjectFacts {
                local_root: true,
                ..EFFECT
            },
            ObjectFacts {
                pooled: true,
                ..EFFECT
            },
            ObjectFacts {
                dependency: true,
                ..EFFECT
            },
        ] {
            assert!(!eligible(o), "{o:?}");
        }
    }

    #[test]
    fn a_live_enemy_past_its_set_up_or_a_wind_up_makes_candidates() {
        let enemy = Maker {
            enemy: true,
            ..Maker::default()
        };
        assert!(candidate(enemy));
        assert!(!candidate(Maker {
            set_up: true,
            ..enemy
        }));
        assert!(!candidate(Maker {
            dying: true,
            ..enemy
        }));
        assert!(!candidate(Maker::default()), "a controller, an effect");
        assert!(
            candidate(Maker {
                windup: true,
                ..Maker::default()
            }),
            "inside a with of a wind-up (the warning's ring)"
        );
    }

    #[test]
    fn a_shared_shot_ends_in_its_destroy_event_outside_the_joining_player_s_site() {
        // The bomb's timer ends it on the host, or the joining copy's own
        // timer first (outside the joining player's site): its ending either
        // way, so the copy's burst is scratch and the host's is the one
        // shown.
        assert!(ending(true, true, false));
        // The joining player ended it (its own site): its own burst; the
        // host removes its instance without the Destroy event, so it makes
        // none.
        assert!(!ending(true, true, true));
        assert!(!ending(true, false, false), "the host's end here: no event");
        // An enemy's own death (the Hammerhead's, the Triangle Mine's):
        // no shared shot, each game's own.
        assert!(!ending(false, true, false));
        // The spawner the host sent places its warnings (no leader); a
        // charge warning on its enemy keeps the wind-up rule.
        assert!(kin(true, false));
        assert!(!kin(true, true));
        assert!(!kin(false, false), "the joining copy's own, not shared");
    }

    #[test]
    fn what_a_shared_shot_makes_as_it_ends_and_what_that_makes_are_the_host_s() {
        // The bomb's ending makes the spawner; the spawner (shared, no
        // shot, following nothing) makes the warnings.
        let ending = Maker {
            ending: true,
            ..Maker::default()
        };
        let kin = Maker {
            kin: true,
            windup: true,
            ..Maker::default()
        };
        for m in [ending, kin] {
            assert!(candidate(m) && free(m), "{m:?}");
        }
        // An enemy's own death (the Hammerhead's spawner, the Triangle
        // Mine's): no shared shot ends there, so each game's own.
        let dying_enemy = Maker {
            enemy: true,
            dying: true,
            ..Maker::default()
        };
        assert!(!candidate(dying_enemy) && !free(dying_enemy));
        // A wind-up on an enemy keeps its follow test.
        let windup = Maker {
            windup: true,
            ..Maker::default()
        };
        assert!(candidate(windup) && !free(windup));
        let enemy = Maker {
            enemy: true,
            ..Maker::default()
        };
        assert!(!free(enemy));
        // A shot in its life (not ending) makes nothing of this rule.
        assert!(!candidate(Maker::default()));
    }

    #[test]
    fn a_wind_up_follows_an_enemy_or_a_shared_attack() {
        assert!(follows(&[Lead::Enemy, Lead::Nothing]));
        assert!(follows(&[Lead::Nothing, Lead::Shared]));
        assert!(!follows(&[Lead::Other, Lead::Nothing]));
        assert!(!follows(&[]));
    }

    #[test]
    fn an_object_made_again_within_half_a_second_is_continuous() {
        let mut o = Overlaps::default();
        // A charge warning: one an attack cycle.
        for f in [10u64, 400, 800, 1200] {
            assert!(!o.note(1, 761, 469, f));
        }
        // Thrusters: every 4 frames.
        for f in (100..140).step_by(4) {
            o.note(1, 761, 484, f);
        }
        assert!(o.continuous(761, 484));
        assert!(!o.continuous(761, 469));
        assert!(!o.continuous(800, 484), "per enemy type");
        // A set made in one frame is no repeat.
        for _ in 0..5 {
            assert!(!o.note(2, 761, 495, 50));
        }
        // Repeats past the bound are not quick.
        for f in [60u64, 60 + QUICK_FRAMES + 1, 60 + 2 * QUICK_FRAMES + 2] {
            assert!(!o.note(3, 761, 309, f));
        }
        o.clear();
        assert!(!o.continuous(761, 484));
    }

    #[test]
    fn a_continuous_effect_that_comes_with_its_maker_s_attacks_stays_the_host_s() {
        let mut o = Overlaps::default();
        // A Juggernaut's volley: a burst with each pair of shots, 4 frames
        // apart, a volley every 1.5 s: continuous by the quick repeats, and
        // made in its maker's firing frames every time.
        for v in 0..3u64 {
            for k in 0..4u64 {
                let f = 100 + v * 90 + k * 4;
                o.note(1, 556, 309, f);
                o.note_attack(556, 309, true);
            }
        }
        assert!(o.continuous(556, 309));
        assert!(o.attack_bound(556, 309));
        assert!(!o.own(556, 309), "the host's: the copy's volleys are not");
        // Its thrusters: every 4 frames, a shot frame now and then.
        for f in (100..400).step_by(4) {
            o.note(1, 556, 484, f);
            o.note_attack(556, 484, f % 64 == 0);
        }
        assert!(o.continuous(556, 484) && !o.attack_bound(556, 484));
        assert!(o.own(556, 484), "movement: the copy makes them itself");
        // A dashing enemy's thrusters, made in its firing frames six times
        // in ten: still its own (the two games' shares sit either side of
        // a half; only all but a few in the firing frames is an attack's).
        for k in 0..40u32 {
            o.note_attack(557, 484, k % 10 < 6);
        }
        assert_eq!(o.attack_counts(557, 484), (24, 40));
        assert!(!o.attack_bound(557, 484));
        // One make in ten outside a firing frame (a burst a frame late):
        // still the attack's.
        for k in 0..40u32 {
            o.note_attack(558, 309, k % 10 != 0);
        }
        assert!(o.attack_bound(558, 309));
        // Continuous before its attack share is known: still the host's.
        let mut q = Overlaps::default();
        for k in 0..(ATTACK_BOUND_AFTER - 1) {
            q.note(1, 556, 484, 100 + u64::from(k) * 4);
            q.note_attack(556, 484, false);
        }
        assert!(q.continuous(556, 484) && !q.own(556, 484), "not known yet");
        q.note(1, 556, 484, 200);
        q.note_attack(556, 484, false);
        assert!(q.own(556, 484), "known: its own");
        // Not yet sure: too few makes.
        let mut p = Overlaps::default();
        for _ in 1..ATTACK_BOUND_AFTER {
            p.note_attack(556, 309, true);
        }
        assert!(!p.attack_bound(556, 309));
        // Not continuous: a wind-up either way (own is false).
        assert!(!p.own(556, 309));
        o.clear();
        assert!(!o.attack_bound(556, 309));
    }

    #[test]
    fn an_enemy_s_blasts_in_its_life_are_the_host_s() {
        let enemy = Maker {
            enemy: true,
            ..Maker::default()
        };
        assert!(blast_shared(BLAST_KIND, enemy));
        assert!(!blast_shared(0, enemy), "shots go their own way");
        assert!(!blast_shared(
            BLAST_KIND,
            Maker {
                dying: true,
                ..enemy
            }
        ));
        assert!(!blast_shared(BLAST_KIND, Maker::default()));
    }

    /// A dying enemy's maker facts as `windup_maker` gives them: the end
    /// rule's two inputs.
    fn dying(in_destroy: bool, partner_kill: bool) -> Maker {
        Maker {
            enemy: true,
            dying: true,
            end_unseen: end_unseen(in_destroy, partner_kill),
            ..Maker::default()
        }
    }

    /// The blasts the joining screen shows for one end: the host's when it
    /// is sent, plus the joining copy's own when the copy makes one (`copy`:
    /// its maker facts then) and it is not scratch.
    fn on_the_joining_screen(host: Maker, copy: Option<Maker>) -> u32 {
        u32::from(blast_shared(BLAST_KIND, host))
            + u32::from(copy.is_some_and(|m| !blast_shared(BLAST_KIND, m)))
    }

    /// Exactly one blast on the joining screen at every end (the host's
    /// screen has its own instance's, always one). In the Destroy event
    /// (every blast the game makes at an end: a Dweller drone's, a
    /// missile's, a mine's) and elsewhere in a death (the step that ends it).
    #[test]
    fn one_blast_on_each_screen_at_every_end() {
        // 1. The host kills it, and 3. it ends itself at the host: the
        // joining copy runs its Destroy event at the host's word (with the
        // host's last state): its own blast; the host's stays the host's.
        assert_eq!(
            on_the_joining_screen(dying(true, false), Some(dying(true, false))),
            1
        );
        // Made in the step that ends it: the joining copy runs none of that
        // at the host's word, so the host's is sent.
        assert_eq!(on_the_joining_screen(dying(false, false), None), 1);
        // 2. The joining player kills it: the copy's whole death path ran
        // first, its own; the host's death (that player's report) keeps its
        // own.
        assert_eq!(
            on_the_joining_screen(dying(true, true), Some(dying(true, true))),
            1
        );
        assert_eq!(
            on_the_joining_screen(dying(false, true), Some(dying(false, true))),
            1
        );
        // 4. It ends itself at the joining game (its own step there, before
        // the host's word): scratch, and the copy waits for the host's end
        // (`checkDestroy` is the host's for a copy), case 3.
        assert_eq!(
            on_the_joining_screen(dying(false, false), Some(dying(false, false))),
            1
        );
        assert!(
            blast_shared(BLAST_KIND, dying(false, false)),
            "scratch on the joining screen"
        );
        // In its life: the host's, the joining copy's scratch.
        let live = Maker {
            enemy: true,
            ..Maker::default()
        };
        assert_eq!(on_the_joining_screen(live, Some(live)), 1);
    }
}

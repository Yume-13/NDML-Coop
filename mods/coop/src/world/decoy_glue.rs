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

//! What the host's enemies choose of the joining player's things (decoy,
//! Cosmic Egg, constructs), by `chooseTarget`'s own rule ([`pick`]). The
//! joining player's things are proxies on the host, which the game's
//! `instance_nearest` does not find.
//!
//! * Each game sends its `decoySignal`, `cloaked` and `tacticalBeacon`
//!   ([`LureState`]) on a change and every 2 s.
//! * Host, after `chooseTarget` aimed at a joining player's avatar: the rule
//!   over that player's proxies; a pick becomes the target and is sent in
//!   `AimThings`. A chaser whose proxy goes returns to the avatar.
//! * Joining game: the host's word for one of its own things resolves to its
//!   own instance.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use ndml_net::msg::{LureState, Message, PeerId};
use ndml_net::shots::ThingRef;
use ndml_sdk::{Call, Instance, ModContext, Value, Variable};

use super::names::builtin_var as bv;
use super::{ROLE_HOST, WorldHooks, read};

/// The facts are sent again this often (frames) without a change.
const RESEND_EVERY: u64 = 120;
/// Object indices scanned for `oPlayerConstruct` children (only those
/// `object_exists` takes are asked; index 0 is no object in this build).
const OBJECT_SCAN: u32 = 1400;

struct Handles {
    decoy: Option<Variable>,
    cloaked: Option<Variable>,
    beacon: Option<Variable>,
}

static DH: OnceLock<Handles> = OnceLock::new();

/// The objects of the rule (the same indices on both games).
#[derive(Debug, Clone)]
struct Objects {
    powers: u32,
    egg: u16,
    bastion: u16,
    mine: u16,
    /// Every `oPlayerConstruct` child (the Bastion and mines included).
    constructs: Vec<u16>,
    /// `oConstructAlly` and its children (a decoy's kinds).
    allies: Vec<u16>,
    /// Roots to search this game's own instances by: `oPlayerConstruct`,
    /// `oConstructAlly`, `oCosmicEgg`.
    roots: Vec<u32>,
}

/// What a host enemy chases of a joining player's.
#[derive(Debug, Clone)]
struct Chase {
    thing: ThingRef,
    proxy: Value,
    enemy: Value,
}

/// Per-session state.
#[derive(Debug, Default)]
pub(super) struct DecoyGlue {
    objects: Option<Objects>,
    tried: bool,
    sent: Option<LureState>,
    sent_frame: u64,
    partners: HashMap<PeerId, LureState>,
    /// Host: enemies and shots chasing a partner's thing, by key.
    chasing: HashMap<usize, Chase>,
    counts: Counts,
}

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    /// Host: calls with some of the partner's things drawn but the ship (or
    /// nothing) picked.
    ship_kept: u64,
}

impl DecoyGlue {
    /// Forget what was sent, the partners' lures and the chases.
    pub(super) fn clear(&mut self) {
        self.sent = None;
        self.partners.clear();
        self.chasing.clear();
    }
}

/// Resolve the handles.
pub(super) fn register(ctx: &ModContext) {
    let _ = DH.set(Handles {
        decoy: ctx.variable("decoySignal").ok(),
        cloaked: ctx.variable("cloaked").ok(),
        beacon: ctx.variable("tacticalBeacon").ok(),
    });
}

/// The nearest of `items` to `at` (squared distance).
fn nearest<'a, T>(
    items: impl Iterator<Item = (&'a T, (f32, f32))>,
    at: (f32, f32),
) -> Option<(&'a T, f32)>
where
    T: 'a,
{
    items
        .map(|(t, p)| (t, (p.0 - at.0).powi(2) + (p.1 - at.1).powi(2)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// `chooseTarget`'s rule over one player's things: `ship` the squared
/// distance to that player's ship (`None`: cloaked), `things` (kind, place,
/// index): 0 egg, 1 ally, 2 construct, 3 mine, 4 Bastion. Returns the index
/// picked, or `None` (the ship, or nothing).
#[must_use]
pub fn pick(
    at: (f32, f32),
    ship: Option<f32>,
    facts: LureState,
    things: &[(u8, (f32, f32), usize)],
) -> Option<usize> {
    let of = |k: u8| things.iter().filter(move |t| t.0 == k).map(|t| (&t.2, t.1));
    let mut cur: Option<(Option<usize>, f32)> = ship.map(|d| (None, d));
    if let Some((i, d)) = nearest(of(0), at)
        && cur.is_none_or(|c| d < c.1)
    {
        cur = Some((Some(*i), d));
    }
    if facts.decoy_signal
        && let Some((i, d)) = nearest(of(1), at)
        && cur.is_none_or(|c| d < c.1)
    {
        cur = Some((Some(*i), d));
    }
    if cur.is_none() {
        let any = things
            .iter()
            // Every `oPlayerConstruct` but the Bastion; mines only with the beacon.
            .filter(|t| t.0 == 1 || t.0 == 2 || (t.0 == 3 && facts.tactical_beacon))
            .map(|t| (&t.2, t.1));
        if let Some((i, d)) = nearest(any, at) {
            cur = Some((Some(*i), d));
        }
    }
    cur.and_then(|c| c.0)
}

impl WorldHooks {
    fn decoy_objects(&self, call: &Call<'_>) -> Option<Objects> {
        {
            let st = self.lock();
            if st.decoy.objects.is_some() || st.decoy.tried {
                return st.decoy.objects.clone();
            }
        }
        let a = |n: &str| self.asset(call, n);
        let o = (|| {
            let root = a("oPlayerConstruct")?;
            let ally = a("oConstructAlly")?;
            let egg = a("oCosmicEgg")?;
            let known: Vec<u32> = (0..OBJECT_SCAN)
                .filter(|&i| self.object_known(call, i))
                .collect();
            let constructs: Vec<u16> = known
                .iter()
                .copied()
                .filter(|&i| i != root && self.ancestor(call, i, Some(root)))
                .filter_map(|i| u16::try_from(i).ok())
                .collect();
            let allies: Vec<u16> = known
                .iter()
                .copied()
                .filter(|&i| i == ally || self.ancestor(call, i, Some(ally)))
                .filter_map(|i| u16::try_from(i).ok())
                .collect();
            Some(Objects {
                powers: a("oPowers")?,
                egg: u16::try_from(egg).ok()?,
                bastion: u16::try_from(a("oShieldBastion")?).ok()?,
                mine: u16::try_from(a("oConstructMine")?).ok()?,
                constructs,
                allies,
                roots: vec![root, ally, egg],
            })
        })();
        let mut st = self.lock();
        st.decoy.tried = true;
        st.decoy.objects.clone_from(&o);
        match &o {
            None => log::warn!(
                "co-op decoys: an object of the rule was not found; the joining player's decoys stay off"
            ),
            Some(r) => log::info!(
                "co-op decoys: the rule's objects: {} constructs {:?}, {} ally kinds {:?}, egg {}",
                r.constructs.len(),
                r.constructs,
                r.allies.len(),
                r.allies,
                r.egg
            ),
        }
        o
    }

    /// Run step: this game's facts to the partners, and the host's check of
    /// its chasers.
    pub(super) fn decoy_run_step(&self, call: &Call<'_>, me: Option<PeerId>) -> Vec<Message> {
        let (Some(me), Some(o), Some(h)) = (me, self.decoy_objects(call), DH.get()) else {
            return Vec::new();
        };
        let mut facts = LureState {
            peer: me,
            decoy_signal: false,
            cloaked: false,
            tactical_beacon: false,
        };
        let truthy = |v: Option<f64>| v.is_some_and(|x| x > 0.5);
        let _ = call.with(&Value::object(o.powers), |p, _| {
            facts.decoy_signal = truthy(h.decoy.and_then(|v| p.get_f64(v)));
            facts.cloaked = truthy(h.cloaked.and_then(|v| p.get_f64(v)));
            facts.tactical_beacon = truthy(h.beacon.and_then(|v| p.get_f64(v)));
        });
        if self.role() == ROLE_HOST {
            self.decoy_release(call);
        }
        let mut st = self.lock();
        let frame = st.frame;
        let mut out = Vec::new();
        if st.decoy.sent != Some(facts) || frame.saturating_sub(st.decoy.sent_frame) >= RESEND_EVERY
        {
            st.decoy.sent = Some(facts);
            st.decoy.sent_frame = frame;
            out.push(Message::LureState(facts));
        }
        out
    }

    /// Run step: the partners' facts.
    pub(super) fn decoy_inbox(&self, inbox: &mut Vec<Message>) {
        let mut st = self.lock();
        let me = st.me;
        inbox.retain(|m| match m {
            Message::LureState(l) => {
                if Some(l.peer) != me {
                    st.decoy.partners.insert(l.peer, *l);
                }
                false
            }
            _ => true,
        });
    }

    /// Host, after `chooseTarget` of `me` (an enemy or a shared shot) aimed
    /// at partner `p`'s avatar: `p`'s thing the rule picks, written as the
    /// target. `None`: the ship stands.
    pub(super) fn decoy_pick(
        &self,
        call: &Call<'_>,
        me: &Instance<'_>,
        partner: PeerId,
    ) -> Option<ThingRef> {
        let key = me.key();
        let facts = {
            let mut st = self.lock();
            if Some(partner) == st.me {
                return None;
            }
            let f = st.decoy.partners.get(&partner).copied();
            if f.is_none() {
                st.decoy.chasing.remove(&key);
            }
            f?
        };
        let rule = self.decoy_objects(call)?;
        let links = self.shot.things.get()?;
        let (Some(ex), Some(ey)) = (read(me, bv::X), read(me, bv::Y)) else {
            return None;
        };
        let mut wanted = rule.constructs.clone();
        wanted.extend(rule.allies.iter().copied());
        wanted.push(rule.egg);
        let proxies: Vec<(u16, u16, Value)> = (links.proxies_of)(&wanted)
            .into_iter()
            .filter(|(owner, ..)| *owner == partner)
            .map(|(_, k, obj, id)| (k, obj, id))
            .collect();
        let mut things = Vec::with_capacity(proxies.len());
        for (i, (_, obj, id)) in proxies.iter().enumerate() {
            let kind = if *obj == rule.egg {
                0
            } else if rule.allies.contains(obj) {
                1
            } else if *obj == rule.bastion {
                4
            } else if *obj == rule.mine {
                3
            } else {
                2
            };
            let mut at = None;
            let _ = call.with(id, |inst, _| {
                if let (Some(px), Some(py)) = (read(&inst, bv::X), read(&inst, bv::Y)) {
                    at = Some((px, py));
                }
            });
            if let Some(at) = at {
                things.push((kind, at, i));
            }
        }
        let hd = &self.h;
        let ship = (!facts.cloaked)
            .then(|| {
                let tx = hd.target_x.and_then(|v| me.get_f64(v))?;
                let ty = hd.target_y.and_then(|v| me.get_f64(v))?;
                Some((tx as f32 - ex).powi(2) + (ty as f32 - ey).powi(2))
            })
            .flatten();
        let Some(chosen) = pick((ex, ey), ship, facts, &things) else {
            self.decoy_none(key, (ex, ey), ship, &things);
            return None;
        };
        let (k, _, proxy) = proxies[chosen].clone();
        let at = things.iter().find(|t| t.2 == chosen).map(|t| t.1)?;
        let target = hd.target?;
        me.set(target, proxy.clone()).ok()?;
        for (v, val) in [
            (hd.target_x, f64::from(at.0)),
            (hd.target_y, f64::from(at.1)),
            (hd.target_hs, 0.0),
            (hd.target_vs, 0.0),
        ] {
            if let Some(v) = v {
                let _ = me.set_f64(v, val);
            }
        }
        let thing = ThingRef {
            owner: partner,
            key: k,
        };
        let enemy = me.builtin_value(bv::ID).ok()?;
        let mut st = self.lock();
        st.decoy.chasing.insert(
            key,
            Chase {
                thing,
                proxy,
                enemy,
            },
        );
        Some(thing)
    }

    /// Host: no pick for `key` (the first few logged).
    fn decoy_none(
        &self,
        key: usize,
        (ex, ey): (f32, f32),
        ship: Option<f32>,
        things: &[(u8, (f32, f32), usize)],
    ) {
        let mut st = self.lock();
        st.decoy.chasing.remove(&key);
        if things.is_empty() {
            return;
        }
        st.decoy.counts.ship_kept += 1;
        if st.decoy.counts.ship_kept <= 5 {
            let near = things
                .iter()
                .map(|t| {
                    let d = ((t.1.0 - ex).powi(2) + (t.1.1 - ey).powi(2)).sqrt();
                    format!("kind {} at {d:.0} px", t.0)
                })
                .collect::<Vec<_>>()
                .join(", ");
            log::info!(
                "co-op decoys: kept the ship at {:.0} px over {near}",
                ship.map_or(-1.0, f32::sqrt)
            );
        }
    }

    /// Host, run step: a chaser whose proxy is gone (or now drawn for
    /// another thing) goes back to its player's avatar, and is said so.
    fn decoy_release(&self, call: &Call<'_>) {
        let Some(links) = self.shot.things.get() else {
            return;
        };
        let chasing: Vec<(usize, Chase)> = self
            .lock()
            .decoy
            .chasing
            .iter()
            .map(|(k, c)| (*k, c.clone()))
            .collect();
        for (key, c) in chasing {
            if (links.proxy)(c.thing).as_ref() == Some(&c.proxy) {
                continue;
            }
            self.lock().decoy.chasing.remove(&key);
            if !self.exists(call, &c.enemy) {
                continue;
            }
            let _ = call.with(&c.enemy, |inst, _| self.force_at(&inst, c.thing.owner));
            self.aim_released(key);
        }
    }

    /// Joining game: the host's word for one of this game's own things: its
    /// instance here, and place.
    pub(super) fn decoy_own(&self, call: &Call<'_>, thing: ThingRef) -> Option<(Value, f32, f32)> {
        let o = self.decoy_objects(call)?;
        let links = self.shot.things.get()?;
        let mut found = None;
        for root in &o.roots {
            let _ = call.with(&Value::object(*root), |inst, _| {
                if found.is_none()
                    && (links.key_of)(&inst) == Some(thing)
                    && let (Ok(id), Some(x), Some(y)) = (
                        inst.builtin_value(bv::ID),
                        read(&inst, bv::X),
                        read(&inst, bv::Y),
                    )
                {
                    found = Some((id, x, y));
                }
            });
            if found.is_some() {
                break;
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::pick;
    use ndml_net::msg::{LureState, PeerId};

    fn facts(decoy: bool, cloaked: bool, beacon: bool) -> LureState {
        LureState {
            peer: PeerId(2),
            decoy_signal: decoy,
            cloaked,
            tactical_beacon: beacon,
        }
    }

    #[test]
    fn the_rule_is_choose_targets() {
        let at = (0.0, 0.0);
        let ship = Some(100.0 * 100.0);
        let ally_near = [(1, (50.0, 0.0), 7)];
        let ally_far = [(1, (500.0, 0.0), 7)];
        // A closer decoy with Decoy Signal; not without; not a farther one.
        assert_eq!(
            pick(at, ship, facts(true, false, false), &ally_near),
            Some(7)
        );
        assert_eq!(pick(at, ship, facts(false, false, false), &ally_near), None);
        assert_eq!(pick(at, ship, facts(true, false, false), &ally_far), None);
        // A closer egg, whatever the facts.
        assert_eq!(
            pick(at, ship, facts(false, false, false), &[(0, (10.0, 0.0), 3)]),
            Some(3)
        );
        // Constructs only with no target: cloaked; the Bastion never, a
        // mine only with the beacon.
        let things = [
            (2, (300.0, 0.0), 1),
            (4, (5.0, 0.0), 2),
            (3, (20.0, 0.0), 3),
        ];
        assert_eq!(pick(at, ship, facts(false, false, false), &things), None);
        assert_eq!(pick(at, None, facts(false, true, false), &things), Some(1));
        assert_eq!(pick(at, None, facts(false, true, true), &things), Some(3));
        // Cloaked, no Decoy Signal: an ally is a construct like the others.
        assert_eq!(
            pick(at, None, facts(false, true, false), &ally_far),
            Some(7)
        );
        // Cloaked with a decoy: the decoy, wherever it is.
        assert_eq!(pick(at, None, facts(true, true, false), &ally_far), Some(7));
    }
}

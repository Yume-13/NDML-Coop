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

//! Shared enemy shots: the game side of [`crate::world::shots`], a child of
//! `glue` for the hooks' state.
//!
//! * Creates: the host notes each family shot; a joining game makes the
//!   family's creates of anyone but the mod scratch.
//! * Controller End Step (after the frame's collisions): the host reads new
//!   shots and finds ended ones; a joining game removes scratch, applies the
//!   host's ends and makes the host's shots due at its shown time.
//! * Each function that can end a shot ([`sh::END_SITES`]) is bracketed:
//!   before it, a joining game vetoes the world's ends of shared shots and
//!   every site of scratch shots, and the host keeps a shot `destroyOutside`
//!   would end while it is in the joining view; after it, an end is
//!   attributed to the site's class.
//!
//! Ids are checked with `instance_exists` before calls on them; no lock is
//! held across a game call.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use ndml_net::interp::Vec2;
use ndml_net::msg::{Message, NetId, PeerId, StateValue, WorldView};
use ndml_net::shots::{
    AimSubject, AimThings, FiredShot, ShotFire, ShotGone, ShotSteer, ThingRef, cause, steer,
};
use ndml_sdk::{Call, Instance, ModContext, Value};

use super::super::classify::is_instance;
use super::super::names::builtin_var as bv;
use super::super::shots::{self as sh, Action, EndClass, Kind, Rect};
use super::super::state::{self, Stats};
use super::{ROLE_CLIENT, ROLE_HOST, ROLE_OFF, WorldHooks, hook_or_warn, object_of, read};
use crate::damage::glue::PlayerSide;

/// The wire index of `x` and `y` in [`state::BUILTINS`] (the depth and the
/// rest are written by the state after the create).
const BUILTIN_X: u8 = 9;
const BUILTIN_Y: u8 = 10;
/// The beams' kind ([`sh::KINDS`] index).
const BEAM_KIND: u8 = 2;
/// `beamSFX` and `loopSFX` ([`WorldHooks::shot_beam_sound`]).
static SOUND_VARS: OnceLock<(Option<ndml_sdk::Variable>, Option<ndml_sdk::Variable>)> =
    OnceLock::new();

/// Both roles' tables (inside the world's lock).
#[derive(Debug, Default)]
pub(super) struct ShotsState {
    pub(super) host: sh::HostShots,
    pub(super) client: sh::ClientShots,
    /// Object -> its kind ([`sh::KINDS`]), or none (cached).
    family: HashMap<u32, Option<u8>>,
    /// Object -> a beam-kind object the host owns whoever makes it, noted.
    host_beam: HashMap<u32, bool>,
    /// Object -> an area (`oEnemyAoE`), cached.
    area_of: HashMap<u32, bool>,
    /// Inside a site that runs: host, a world site of a shared shot (what
    /// it makes is shared); joining game, a player site (what it makes
    /// there is this game's own).
    world_site: u32,
    player_site: u32,
    /// Host: `StateNames` to send ahead of this frame's births.
    names_out: Vec<Message>,
    /// The host is there (a joining game vetoes world ends only then).
    linked: bool,
    /// The partners' avatars by key, and the frame they were found in.
    avatars: Option<(u64, Arc<HashSet<usize>>)>,
    /// What enemies and shared shots chase where it is a player's thing
    /// (`aim.rs`): host, what it said; joining game, what it was told.
    pub(super) aims_host: crate::world::aim::HostAims,
    pub(super) aims_client: crate::world::aim::ClientAims,
}

impl ShotsState {
    /// Forget the tables, the sites and the aims; the caches stay.
    pub(super) fn clear(&mut self) {
        self.host.clear();
        self.client.clear();
        self.names_out.clear();
        self.avatars = None;
        self.world_site = 0;
        self.player_site = 0;
        self.aims_host.clear();
        self.aims_client.clear();
    }
}

/// What a site's after-hook needs.
#[derive(Debug)]
pub(super) struct SiteNote {
    key: usize,
    id: Value,
    object: u32,
    cause: u8,
    /// The site counter it raised (1: world, host; 2: player, joining).
    depth: u8,
}

/// Where the shared shots' extra state lives on the hooks.
#[derive(Debug, Default)]
pub(super) struct ShotHooks {
    /// Client: the mod is making a shot now (its create is let through).
    pub(super) making: std::sync::atomic::AtomicBool,
    /// The mirror's player-side rule (black holes).
    pub(super) side: OnceLock<PlayerSide>,
    /// The mirror's lookups for a player's thing (what enemies chase).
    pub(super) things: OnceLock<ThingLinks>,
}

/// A player's thing by the mirror's key: this game's own mirrored instance
/// to its [`ThingRef`] (owner side), and a partner's [`ThingRef`] to the
/// proxy drawn for it here (viewer side). Read-only lookups.
pub struct ThingLinks {
    /// This game's own mirrored instance to its [`ThingRef`].
    pub key_of: Box<ThingKeyFn>,
    /// A partner's [`ThingRef`] to the proxy drawn for it here.
    pub proxy: Box<dyn Fn(ThingRef) -> Option<Value> + Send + Sync>,
    /// The proxies drawn here for the partner's instances of these objects
    /// (the owner's object indices): (owner, key, object, proxy).
    pub proxies_of: Box<ProxiesOfFn>,
}

/// [`ThingLinks::proxies_of`].
pub type ProxiesOfFn = dyn Fn(&[u16]) -> Vec<(PeerId, u16, u16, Value)> + Send + Sync;

/// [`ThingLinks::key_of`].
pub type ThingKeyFn = dyn Fn(&Instance<'_>) -> Option<ThingRef> + Send + Sync;

impl std::fmt::Debug for ThingLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThingLinks")
    }
}

impl WorldHooks {
    /// The transfer's reflection builtins are there.
    pub(super) fn shots_on(&self) -> bool {
        self.h.reflect.is_some()
    }

    /// The mirror's player-side rule, set once at start (`lib.rs`).
    pub fn set_shot_side(&self, f: PlayerSide) {
        let _ = self.shot.side.set(f);
    }

    /// The mirror's lookups for a player's thing, set once at start.
    pub fn set_thing_links(&self, l: ThingLinks) {
        let _ = self.shot.things.set(l);
    }

    /// The kind of `obj` ([`sh::KINDS`]): a child of `oEnemyProjectile`,
    /// `oEnemyBlast` or `oEnemyBeam`, or none (cached).
    fn shot_kind(&self, call: &Call<'_>, obj: u32) -> Option<u8> {
        let roots = {
            let st = self.lock();
            if let Some(k) = st.shots.family.get(&obj) {
                return *k;
            }
            st.objects.as_ref().map(|o| o.shots.clone())?
        };
        let charges = self
            .lock()
            .objects
            .as_ref()
            .map_or_default(|o| o.charges.clone());
        let kind = roots
            .iter()
            .position(|&r| self.ancestor(call, obj, Some(r)))
            .and_then(|i| u8::try_from(i).ok())
            .or_else(|| {
                // A charge-up: counted with the beams it warns of.
                charges
                    .iter()
                    .any(|&r| self.ancestor(call, obj, Some(r)))
                    .then_some(BEAM_KIND)
            });
        self.lock().shots.family.insert(obj, kind);
        kind
    }

    /// Whether `obj` (of kind `kind`) is a beam, chain lightning, charge-up
    /// or area the host owns whoever makes it: every beam-kind object (an
    /// area, `oEnemyAoE`, included). Noted in `host_beam`.
    fn host_beam(&self, obj: u32, kind: u8) -> bool {
        if kind != BEAM_KIND {
            return false;
        }
        self.lock().shots.host_beam.insert(obj, true);
        true
    }

    /// Sounds (`crate::sound`, read-only): whether instances of `obj` are
    /// the host's world on a joining game: an `oSolid` child that is not a
    /// [`classify::LOCAL`] missile, or an enemy shot, beam, charge-up or area
    /// while shared shots are on. `None` while this thread holds the world
    /// lock (the caller keeps the answers it had).
    ///
    /// [`classify::LOCAL`]: super::super::classify::LOCAL
    #[must_use]
    pub fn sound_world_object(&self, call: &Call<'_>, obj: u32) -> Option<bool> {
        if crate::game::held_at("world").is_some() {
            return None;
        }
        Some(match self.class_of(call, obj, false) {
            Some(super::Class::Enemy { local, .. }) => !local,
            Some(_) => match self.shot_kind(call, obj) {
                Some(BEAM_KIND) => self.host_beam(obj, BEAM_KIND),
                Some(_) => self.shots_on(),
                None => false,
            },
            None => false,
        })
    }

    /// Whether `obj` is an area (`oEnemyAoE`). Cached.
    fn area_object(&self, call: &Call<'_>, obj: u32) -> bool {
        let area = {
            let st = self.lock();
            if let Some(&b) = st.shots.area_of.get(&obj) {
                return b;
            }
            st.objects.as_ref().and_then(|o| o.area)
        };
        let b = area.is_some_and(|a| self.ancestor(call, obj, Some(a)));
        self.lock().shots.area_of.insert(obj, b);
        b
    }

    /// Before a create of an enemy shot: the note to record after it, or
    /// `None` (not a shared shot).
    pub(super) fn shot_create(
        &self,
        call: &Call<'_>,
        role: u8,
        obj: u32,
    ) -> Option<super::PartNote> {
        if !self.shots_on() || role == ROLE_OFF {
            return None;
        }
        let kind = self.shot_kind(call, obj)?;
        let host_beam = self.host_beam(obj, kind);
        if kind != 0 && !host_beam && !self.attack_shared(call, role) {
            // A blast or beam an enemy makes: each game's own (its copy
            // makes it too).
            return None;
        }
        // The mod's own make: once (what the shot's own Create makes is
        // scratch like any other).
        let scratch = role == ROLE_CLIENT && !self.shot.making.swap(false, Ordering::Relaxed);
        Some(super::PartNote::SharedShot { scratch })
    }

    /// Whether a blast or beam made now is shared: what a shared shot makes,
    /// and on the host what a world site of a shared shot makes. On a
    /// joining screen: the mod's own make, and what a made shot makes
    /// (scratch: the host's comes), except inside a site of its own player's.
    fn attack_shared(&self, call: &Call<'_>, role: u8) -> bool {
        let maker = call.instance().map(|i| i.key());
        let st = self.lock();
        if role == ROLE_HOST {
            st.shots.world_site > 0 || maker.is_some_and(|k| st.shots.host.shared(k))
        } else {
            self.shot.making.load(Ordering::Relaxed)
                || (st.shots.player_site == 0
                    && maker.is_some_and(|k| st.shots.client.shared(k).is_some()))
        }
    }

    /// After that create: the host notes the shot (read at the End Step); a
    /// joining game notes scratch.
    pub(super) fn shot_created(&self, call: &Call<'_>, scratch: bool) {
        let Some(id) = call.result().filter(is_instance) else {
            return;
        };
        let mut found = None;
        let _ = call.with(&id, |inst, _| found = Some((inst.key(), object_of(&inst))));
        let Some((key, Some(obj))) = found else {
            return;
        };
        let role = self.role();
        // The maker (`self` of the create): an enemy's assignment, or a
        // shared shot's own player (a split shot's children).
        let maker = call.instance().map(|i| i.key());
        let mut st = self.lock();
        if role == ROLE_HOST {
            let frame = st.frame;
            let target = maker.and_then(|k| {
                st.host
                    .assignment(k)
                    .or_else(|| st.shots.host.target_of(k).flatten())
            });
            let wait = u8::from(st.shots.host_beam.get(&obj).copied().unwrap_or(false));
            let born_ms = if wait > 0 { st.repl.now_ms.max(1) } else { 0 };
            st.shots.host.born.push(sh::Born {
                key,
                value: id,
                object: obj,
                frame,
                target,
                wait,
                born_ms,
            });
        } else if role == ROLE_CLIENT && scratch {
            st.shots.client.scratch.push((key, id));
            st.shots.client.scratch_keys.insert(key);
        }
    }

    /// Before a site: the site's action on the shot it touches (`self` or
    /// `other`).
    fn shot_site_before(
        &self,
        call: &mut Call<'_>,
        name: &'static str,
        class: EndClass,
    ) -> Option<SiteNote> {
        let role = self.role();
        if role == ROLE_OFF || !self.shots_on() {
            return None;
        }
        let me = call.instance();
        let other = call.other();
        let found = self.shot_at(role, me.as_ref(), other.as_ref());
        let site = found.map(|(key, id, object, kind)| {
            let class = match class {
                EndClass::ByRule => {
                    // The site's other party (not the shot) decides.
                    let party = [me.as_ref(), other.as_ref()]
                        .into_iter()
                        .flatten()
                        .find(|i| i.key() != key);
                    let player = match (party, self.shot.side.get()) {
                        (Some(p), Some(side)) => (side.0)(call, p),
                        _ => false,
                    };
                    if player {
                        EndClass::Player
                    } else {
                        EndClass::World
                    }
                }
                c => c,
            };
            (key, id, object, kind, class)
        });
        let outside = name == sh::DESTROY_OUTSIDE;
        if let Some((key, _, _, kind, class)) = &site {
            let veto = if role == ROLE_CLIENT {
                let own_party = *class == EndClass::World
                    && self.own_attack_party(call, *key, me.as_ref(), other.as_ref());
                let linked = self.lock().shots.linked;
                sh::client_action(*class, *kind, linked, own_party) == Action::Veto
            } else {
                false
            };
            if veto {
                call.skip_original();
                if outside {
                    let _ = call.set_return(Value::Bool(false));
                }
                return None;
            }
            if role == ROLE_HOST && outside && self.shot_keep_for_view(call, me.as_ref()) {
                call.skip_original();
                let _ = call.set_return(Value::Bool(false));
                return None;
            }
        }
        let (key, id, object, kind, class) = site?;
        let depth = match (role, class, kind) {
            (ROLE_HOST, EndClass::World, _) => 1,
            (ROLE_CLIENT, EndClass::Player, Kind::Shared) => 2,
            _ => 0,
        };
        {
            let mut st = self.lock();
            match depth {
                1 => st.shots.world_site += 1,
                2 => st.shots.player_site += 1,
                _ => {}
            }
        }
        Some(SiteNote {
            key,
            id,
            object,
            cause: sh::cause_of(class),
            depth,
        })
    }

    /// Joining game, at a world site of a shared shot: whether its other
    /// party is an enemy attack that is not scratch. Two attacks meet on
    /// both screens alike.
    fn own_attack_party(
        &self,
        call: &Call<'_>,
        shot: usize,
        me: Option<&Instance<'_>>,
        other: Option<&Instance<'_>>,
    ) -> bool {
        let Some(party) = [me, other].into_iter().flatten().find(|i| i.key() != shot) else {
            return false;
        };
        let Some(obj) = object_of(party) else {
            return false;
        };
        let key = party.key();
        {
            let st = self.lock();
            if st.shots.client.scratch_keys.contains(&key) {
                return false;
            }
        }
        self.shot_kind(call, obj).is_some()
    }

    /// A joining game removes a shot, scratch or the host's ended one:
    /// silently, except a beam-kind object the host owns, whose Destroy
    /// event runs (it stops the beam's sound loop; an area shows its end).
    /// A scratch area goes silently (its Destroy can make more of its kind).
    fn shot_remove(&self, call: &Call<'_>, id: &Value, scratch: bool) {
        let mut obj = None;
        let _ = call.with(id, |inst, _| obj = object_of(&inst));
        let beam = obj.is_some_and(|o| {
            self.lock()
                .shots
                .host_beam
                .get(&o)
                .copied()
                .unwrap_or(false)
                && !(scratch && self.area_object(call, o))
        });
        match self.h.destroy {
            Some(b) if beam => {
                let _ = call.call_builtin(b, &[id.clone(), Value::Bool(true)]);
            }
            _ => self.destroy_silently(call, id),
        }
    }

    /// Client: a host beam just made here whose loop plays on the host
    /// (`beamSFX` and `loopSFX` set) runs its user event 0, so the beam
    /// sounds here and `loopSFX` is this game's own loop.
    fn shot_beam_sound(&self, call: &Call<'_>, id: &Value) {
        let (Some(eu), Some(vars)) = (self.h.event_user, SOUND_VARS.get()) else {
            return;
        };
        let (Some(beam_sfx), Some(loop_sfx)) = *vars else {
            return;
        };
        let _ = call.with(id, |inst, _| {
            let set = |v| {
                inst.get_value(v)
                    .is_some_and(|x| !matches!(x, Value::Undefined))
            };
            if set(beam_sfx) && set(loop_sfx) {
                let _ = inst.call_builtin(eu, &[0.0.into()]);
            }
        });
    }

    /// Before a collision with an enemy beam on a joining game: a scratch
    /// beam does nothing here.
    fn scratch_beam_veto(&self, call: &mut Call<'_>) {
        if self.role() != ROLE_CLIENT || !self.shots_on() {
            return;
        }
        let keys = [call.instance(), call.other()].map(|i| i.map(|i| i.key()));
        let scratch = {
            let st = self.lock();
            keys.iter()
                .flatten()
                .any(|k| st.shots.client.scratch_keys.contains(k))
        };
        if scratch {
            call.skip_original();
        }
    }

    /// Whether a shot (captured into `stats`) holds a live reference to
    /// another enemy attack that has no twin in the other game: such a shot
    /// stays each game's own. Its own id does not count.
    fn shot_follows_local(&self, call: &Call<'_>, key: usize, stats: &Stats) -> bool {
        stats.not_replicated.iter().any(|(k, obj, _)| {
            *k != key
                && matches!(
                    self.class_of(call, u32::from(*obj), false),
                    Some(super::Class::EnemyShot)
                )
        })
    }

    /// The shared or scratch shot among a site's `self` and `other`.
    fn shot_at(
        &self,
        role: u8,
        me: Option<&ndml_sdk::Instance<'_>>,
        other: Option<&ndml_sdk::Instance<'_>>,
    ) -> Option<(usize, Value, u32, Kind)> {
        let st = self.lock();
        [me, other].into_iter().flatten().find_map(|i| {
            let key = i.key();
            if role == ROLE_HOST {
                if let Some(s) = st.shots.host.live.get(&key) {
                    return Some((key, s.value.clone(), s.object, Kind::Shared));
                }
                st.shots
                    .host
                    .born
                    .iter()
                    .find(|b| b.key == key)
                    .map(|b| (key, b.value.clone(), b.object, Kind::Shared))
            } else if let Some(m) = st.shots.client.shared(key) {
                Some((key, m.value.clone(), m.object, Kind::Shared))
            } else {
                st.shots
                    .client
                    .scratch
                    .iter()
                    .find(|s| s.0 == key)
                    .map(|s| (key, s.1.clone(), 0, Kind::Scratch))
            }
        })
    }

    /// Host, before `destroyOutside` on a shared shot: keep it while it is
    /// inside the joining view (the function's margin plus one step).
    fn shot_keep_for_view(&self, call: &Call<'_>, me: Option<&ndml_sdk::Instance<'_>>) -> bool {
        let Some(me) = me else { return false };
        let (Some(x), Some(y)) = (read(me, bv::X), read(me, bv::Y)) else {
            return false;
        };
        let step = read(me, bv::SPEED).map_or(0.0, f64::from);
        let margin = call
            .arg_value(0)
            .and_then(|v| v.as_f64())
            .unwrap_or(sh::OUTSIDE_MARGIN);
        let view = self.lock().shots.host.view_at(Instant::now());
        sh::keep_for_view(view, f64::from(x), f64::from(y), margin, step)
    }

    /// After a site: the shot it touched gone (or changed into another
    /// object) is an end of the site's class.
    fn shot_site_after(&self, call: &Call<'_>, note: Option<SiteNote>) {
        let Some(n) = note else { return };
        {
            let mut st = self.lock();
            match n.depth {
                1 => st.shots.world_site = st.shots.world_site.saturating_sub(1),
                2 => st.shots.player_site = st.shots.player_site.saturating_sub(1),
                _ => {}
            }
        }
        let mut obj = None;
        let gone = !self.exists(call, &n.id);
        if !gone {
            let _ = call.with(&n.id, |inst, _| obj = object_of(&inst));
        }
        let changed = !gone && n.object != 0 && obj.is_some_and(|o| o != n.object);
        if !(gone || changed) {
            return;
        }
        let c = if changed { cause::CHANGED } else { n.cause };
        self.shot_ended(n.key, Some(c));
    }

    /// After a player's sweeping site ([`EndClass::PlayerSweep`]): every
    /// shared shot gone now is that player's end.
    fn shot_sweep_after(&self, call: &Call<'_>) {
        let role = self.role();
        let live: Vec<(usize, Value)> = {
            let st = self.lock();
            if role == ROLE_HOST {
                st.shots
                    .host
                    .live
                    .iter()
                    .map(|(k, s)| (*k, s.value.clone()))
                    .collect()
            } else {
                st.shots
                    .client
                    .made
                    .values()
                    .filter(|m| m.gone_at.is_none())
                    .map(|m| (m.key, m.value.clone()))
                    .collect()
            }
        };
        for (key, id) in live {
            if !self.exists(call, &id) {
                self.shot_ended(key, Some(cause::PLAYER));
            }
        }
    }

    /// The shot at `key` ended here with `cause` (`None`: no known site).
    fn shot_ended(&self, key: usize, c: Option<u8>) {
        let role = self.role();
        let mut st = self.lock();
        if role == ROLE_HOST {
            let now = st.repl.now_ms;
            let c = c.unwrap_or(cause::UNKNOWN);
            let ended = st.shots.host.end(key, now, c);
            if let Some(id) = ended {
                st.shots.aims_host.forget(AimSubject::Shot(id));
            }
            if ended.is_none() {
                // Never read: it ended in its birth frame.
                st.shots.host.born.retain(|b| b.key != key);
            }
        } else {
            let shown = st.repl.shown_ms.unwrap_or(0);
            let host_now = st.repl.host_now_ms.unwrap_or(shown);
            let id = st.shots.client.by_key.get(&key).copied();
            if let Some(id) = id {
                st.shots.client.gone_here(id, shown, host_now, c);
            }
        }
    }

    /// Run step, before the role's frame: take the shots' messages out of
    /// the inbox.
    pub(super) fn shots_inbox(&self, call: &Call<'_>, inbox: &mut Vec<Message>, role: u8) {
        if !self.shots_on() {
            return;
        }
        let view_hash = sh::view_names_hash();
        let mut remove = Vec::new();
        let mut resynced = None;
        {
            let mut st = self.lock();
            inbox.retain(|m| match m {
                Message::ShotFire(f) if role == ROLE_CLIENT => {
                    for s in &f.shots {
                        st.shots.client.fire(s.clone());
                    }
                    false
                }
                Message::ShotGone(g) if role == ROLE_CLIENT => {
                    for e in &g.ends {
                        st.shots.client.host_end(*e);
                    }
                    false
                }
                Message::ShotGone(g) if role == ROLE_HOST => {
                    for e in &g.ends {
                        if let Some(v) = st.shots.host.report(*e) {
                            remove.push(v);
                        }
                    }
                    false
                }
                Message::WorldInputs(v) if v.names == view_hash => {
                    if role == ROLE_HOST
                        && let &[x, y, w, h] = v.values.as_slice()
                    {
                        st.shots.host.view = Some((Rect { x, y, w, h }, Instant::now()));
                    }
                    false
                }
                Message::ShotSteer(c) if role == ROLE_CLIENT => {
                    for ch in &c.changes {
                        st.shots.client.steer_change(*ch);
                    }
                    false
                }
                Message::AimThings(a) if role == ROLE_CLIENT => {
                    for c in &a.changes {
                        st.shots.aims_client.change(*c);
                    }
                    false
                }
                Message::ShotFire(_)
                | Message::ShotGone(_)
                | Message::ShotState(_)
                | Message::ShotSteer(_)
                | Message::AimThings(_) => false,
                // In the inbox's order: what came before it is gone with
                // the copies, what follows is the host's new state. Kept
                // for the world.
                Message::WorldResync { .. } if role == ROLE_CLIENT => {
                    let stale = st.shots.client.resync();
                    resynced = Some(stale.len());
                    remove.extend(stale);
                    true
                }
                _ => true,
            });
        }
        if let Some(n) = resynced {
            log::info!("co-op shots: resync; {n} shot copies made before it removed");
        }
        // A frozen copy too (activated first, then removed).
        for v in remove {
            self.destroy_silently(call, &v);
        }
    }

    /// Host, the partner back (a resync): every live shared shot goes again
    /// at the next running read ([`sh::HostShots::resync`]); the name tables
    /// start again with the generic transfer's.
    pub(super) fn shots_resync(&self) {
        if !self.shots_on() {
            return;
        }
        let n = {
            let mut st = self.lock();
            let frame = st.frame;
            st.shots.names_out.clear();
            st.shots.host.resync(frame)
        };
        log::info!("co-op shots: resync; {n} live shared shots sent again when play goes on");
    }

    /// Run step, after the role's frame: what to send, and the link.
    pub(super) fn shots_run_step(
        &self,
        role: u8,
        view: Option<crate::render::View>,
        linked: bool,
    ) -> Vec<Message> {
        if !self.shots_on() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut st = self.lock();
        st.shots.linked = linked;
        let frame = st.frame;
        if role == ROLE_HOST {
            out.append(&mut st.shots.names_out);
            let fires = std::mem::take(&mut st.shots.host.fires);
            let (parts, _) = ShotFire::split(fires);
            out.extend(parts.into_iter().map(Message::ShotFire));
            let ends = std::mem::take(&mut st.shots.host.ends);
            out.extend(ShotGone::split(&ends).into_iter().map(Message::ShotGone));
            let steers = std::mem::take(&mut st.shots.host.steers);
            out.extend(
                ShotSteer::split(&steers)
                    .into_iter()
                    .map(Message::ShotSteer),
            );
            let aims = std::mem::take(&mut st.shots.aims_host.out);
            out.extend(AimThings::split(&aims).into_iter().map(Message::AimThings));
        } else if role == ROLE_CLIENT {
            let ends = std::mem::take(&mut st.shots.client.reports);
            out.extend(ShotGone::split(&ends).into_iter().map(Message::ShotGone));
            if let Some(v) = view
                && frame.saturating_sub(st.shots.client.view_sent) >= sh::VIEW_EVERY
            {
                st.shots.client.view_sent = frame;
                out.push(Message::WorldInputs(WorldView {
                    time_ms: st.repl.now_ms,
                    names: sh::view_names_hash(),
                    values: vec![v.x, v.y, v.w, v.h],
                }));
            }
        }
        out
    }

    /// After the controller's End Step: the frame's shot work (see the
    /// module docs).
    pub(super) fn shots_end_step(&self, call: &Call<'_>) {
        if !self.shots_on() {
            return;
        }
        let role = self.role();
        if role == ROLE_OFF || self.lock().frozen {
            return;
        }
        if role == ROLE_HOST {
            self.shots_host_read(call);
            self.shots_host_sweep(call);
            self.shots_host_steer(call);
        } else {
            self.shots_scratch_clear(call);
            self.shots_client_sweep(call);
            self.shots_client_ends(call);
            self.shots_client_make(call);
            // What the ends' Destroy events and the makes' Create events
            // made is scratch too: removed before anything draws it.
            self.shots_scratch_clear(call);
        }
    }

    /// Host: read the shots born since the last read (within the budget).
    fn shots_host_read(&self, call: &Call<'_>) {
        let Some(r) = self.h.reflect else { return };
        let (born, frame, now) = {
            let mut st = self.lock();
            let take = st.shots.host.born.len().min(sh::READS_PER_FRAME);
            let mut born: Vec<_> = st.shots.host.born.drain(..take).collect();
            // A host beam waits one read (its first alarm shapes it).
            let mut later = Vec::new();
            born.retain_mut(|b| {
                if b.wait == 0 {
                    return true;
                }
                b.wait -= 1;
                later.push(b.clone());
                false
            });
            if !later.is_empty() {
                let rest = std::mem::take(&mut st.shots.host.born);
                st.shots.host.born = later;
                st.shots.host.born.extend(rest);
            }
            (born, st.frame, st.repl.now_ms)
        };
        for b in born {
            let at = if b.born_ms > 0 { b.born_ms } else { now };
            self.shot_send(call, &r, b, (frame, at), true);
        }
    }

    /// Host: read one shot (or an attack a shot follows, `promote` false:
    /// shared from now on, [`FiredShot::replaces`]) and queue its birth.
    /// `promote`: an enemy attack it follows with no twin by id is shared
    /// first (one level down, no further).
    fn shot_send(
        &self,
        call: &Call<'_>,
        r: &state::Reflect,
        b: sh::Born,
        (_, now): (u64, u32),
        promote: bool,
    ) {
        let (key, id, obj) = (b.key, b.value, b.object);
        if !self.exists(call, &id) {
            return;
        }
        let net_of = |k: usize, v: &Value| self.lock().host.net_id_for(k, v);
        let mut stats = Stats::default();
        let c = state::capture(call, r, &id, &net_of, &mut stats);
        let Ok(object) = u16::try_from(obj) else {
            return;
        };
        // References to an attack the joining game has no copy of by id
        // (a ball each game makes itself): by place.
        let (place_names, places) = self.shot_places(call, r, (key, &id, object), &c.vars, promote);
        if places.is_none() && self.shot_follows_local(call, key, &stats) {
            return;
        }
        let (places, place_fills) = places.unwrap_or_default();
        let (names, vars) = self.gen_index_vars(object, &c.vars);
        let builtins = c
            .builtins
            .into_iter()
            .map(|(i, v)| (i, v.compact()))
            .collect();
        // Its player: the maker's, else the nearest at its place now.
        let pos = shot_pos(call, &id);
        let steer = self.shot_steer_here(call, &id);
        let mut st = self.lock();
        let target = if let Some(p) = b.target {
            Some(p)
        } else {
            pos.and_then(|p| st.host.nearest_player(p))
        };
        let sid = st.shots.host.next_id();
        st.shots.names_out.extend(names);
        st.shots.names_out.extend(place_names);
        st.shots.host.fires.push(FiredShot {
            id: sid,
            object,
            time_ms: now,
            vars,
            builtins,
            alarms: c.alarms,
            target,
            steer,
            places,
            place_fills,
            replaces: !promote,
        });
        st.shots.host.live.insert(
            key,
            sh::HostShot {
                id: sid,
                value: id.clone(),
                object: obj,
                target,
                steer,
            },
        );
        drop(st);
        // Aimed at the other player now, not at the next re-pick.
        self.shot_aim_now(call, &id, target, steer == steer::PLAYER);
    }

    /// Host: a shot's references to enemy attacks with no net id, as place
    /// references ([`FiredShot::places`]), the `StateNames` their indices
    /// need, and each attack's numbers and strings
    /// ([`FiredShot::place_fills`]). `None` when one has no place: the shot
    /// then follows something only this game has.
    fn shot_places(
        &self,
        call: &Call<'_>,
        refl: &state::Reflect,
        (own, id, object): (usize, &Value, u16),
        vars: &[(String, StateValue)],
        promote: bool,
    ) -> (Vec<Message>, Option<Places>) {
        let mut names = Vec::new();
        let mut out = Vec::new();
        let mut fills = Vec::new();
        for (name, v) in vars {
            let obj = match v {
                StateValue::Unresolved(o) | StateValue::Singleton(o) if *o != 0 => *o,
                _ => continue,
            };
            if !matches!(
                self.class_of(call, u32::from(obj), false),
                Some(super::Class::EnemyShot)
            ) {
                continue;
            }
            let Some(target) = state::get_var(call, refl, id, name).filter(is_instance) else {
                return (names, None);
            };
            let Some(p) = shot_pos(call, &target) else {
                return (names, None);
            };
            if promote {
                self.shot_promote(call, refl, own, &target);
            }
            let (n, ix) = self.gen_index_vars(object, &[(name.clone(), StateValue::Undefined)]);
            names.extend(n);
            let Some((i, _)) = ix.first() else {
                continue;
            };
            let Ok(place) = u8::try_from(out.len()) else {
                continue;
            };
            out.push((*i, obj, p.x, p.y));
            // The attack's own numbers and strings, for a receiver's instance
            // that lacks some.
            let net_of = |k: usize, v: &Value| self.lock().host.net_id_for(k, v);
            let mut stats = Stats::default();
            let simple: Vec<(String, StateValue)> =
                state::capture(call, refl, &target, &net_of, &mut stats)
                    .vars
                    .into_iter()
                    .filter(|(_, v)| {
                        matches!(
                            v,
                            StateValue::Real(_)
                                | StateValue::Int(_)
                                | StateValue::Bool(_)
                                | StateValue::Str(_)
                        )
                    })
                    .collect();
            let (n, ix) = self.gen_index_vars(obj, &simple);
            names.extend(n);
            fills.extend(ix.into_iter().map(|(i, v)| (place, i, v)));
        }
        (names, Some((out, fills)))
    }

    /// Host: an enemy attack a shared shot follows (each game's own until
    /// now, a Dreadnought's ball) is shared from now on, its birth queued
    /// ahead of the shot's ([`FiredShot::replaces`]).
    fn shot_promote(&self, call: &Call<'_>, refl: &state::Reflect, own: usize, target: &Value) {
        let mut found = None;
        let _ = call.with(target, |inst, _| {
            found = Some((inst.key(), object_of(&inst)));
        });
        let Some((key, Some(obj))) = found else {
            return;
        };
        let (frame, now) = {
            let st = self.lock();
            if key == own
                || st.shots.host.shared(key)
                || st.shots.host.born.iter().any(|b| b.key == key)
            {
                return;
            }
            (st.frame, st.repl.now_ms)
        };
        let born = sh::Born {
            key,
            value: target.clone(),
            object: obj,
            frame,
            target: None,
            wait: 0,
            born_ms: 0,
        };
        self.shot_send(call, refl, born, (frame, now), false);
    }

    /// A shot's `rotSpeed` (0: flies straight).
    fn shot_turn(&self, call: &Call<'_>, id: &Value) -> f64 {
        self.h
            .rot_speed
            .and_then(|v| {
                let mut t = None;
                let _ = call.with(id, |inst, _| t = inst.get_f64(v));
                t
            })
            .unwrap_or(0.0)
    }

    /// Point a shared shot after a player at that player now: their avatar
    /// when they are not this game's, else this game's ship unless the
    /// game's own choice is already a player's thing (cloak, decoys,
    /// constructs). `after`: the shot is after a player. One that steers at
    /// something of the world keeps it.
    pub(super) fn shot_aim_now(
        &self,
        call: &Call<'_>,
        id: &Value,
        target: Option<PeerId>,
        after: bool,
    ) {
        let Some(p) = target.filter(|_| after) else {
            return;
        };
        if Some(p) != self.lock().me {
            let _ = call.with(id, |inst, _| self.force_at(&inst, p));
            return;
        }
        if self.shot_steer_here(call, id) == steer::PLAYER {
            return;
        }
        let Some((ship, x, y)) = self.own_ship(call) else {
            return;
        };
        let h = &self.h;
        let _ = call.with(id, |inst, _| {
            if let Some(v) = h.target
                && inst.set(v, ship.clone()).is_ok()
            {
                for (v, val) in [(h.target_x, x), (h.target_y, y)] {
                    if let Some(v) = v {
                        let _ = inst.set_f64(v, f64::from(val));
                    }
                }
            }
        });
    }

    /// Host, after `chooseTarget` for an enemy or a shared shot: tells the
    /// joining games ([`AimThings`]) when it chases a thing of this game's
    /// player other than the ship. For a subject assigned to a partner,
    /// `pick` is that partner's thing (`decoy_glue`); `None` is its avatar.
    pub(super) fn aim_host(&self, call: &Call<'_>, me: &Instance<'_>, pick: Option<ThingRef>) {
        let Some(links) = self.shot.things.get() else {
            return;
        };
        let key = me.key();
        let (subject, remote, now) = {
            let st = self.lock();
            let subject = st
                .host
                .net_id(key)
                .map(AimSubject::Enemy)
                .or_else(|| st.shots.host.live.get(&key).map(|s| AimSubject::Shot(s.id)));
            let Some(subject) = subject else { return };
            let remote = st
                .host
                .assignment(key)
                .or_else(|| st.shots.host.target_of(key).flatten())
                .is_some_and(|p| Some(p) != st.me);
            (subject, remote, st.repl.now_ms)
        };
        let mut thing = None;
        if !remote
            && let Some(v) = self.h.target
            && let Some(t) = me.get_value(v).filter(is_instance)
        {
            let _ = call.with(&t, |inst, _| thing = (links.key_of)(&inst));
        }
        let me_peer = self.lock().me;
        let thing = if remote {
            pick
        } else {
            thing.filter(|t| Some(t.owner) == me_peer)
        };
        self.lock().shots.aims_host.note(subject, thing, now);
    }

    /// Host: the subject at `key` is back on its player's avatar (its thing
    /// went), said to the joining games.
    pub(super) fn aim_released(&self, key: usize) {
        let (subject, now) = {
            let st = self.lock();
            let subject = st
                .host
                .net_id(key)
                .map(AimSubject::Enemy)
                .or_else(|| st.shots.host.live.get(&key).map(|s| AimSubject::Shot(s.id)));
            (subject, st.repl.now_ms)
        };
        if let Some(subject) = subject {
            self.lock().shots.aims_host.note(subject, None, now);
        }
    }

    /// Joining game, after `chooseTarget` for a copy or a made shot: when the
    /// host said it chases a partner's thing, point it at the proxy drawn
    /// for that thing here (its place; a proxy does not move by speed).
    /// Returns whether it did (then the usual rules are left out).
    pub(super) fn aim_client(&self, call: &Call<'_>, me: &Instance<'_>) -> bool {
        let Some(links) = self.shot.things.get() else {
            return false;
        };
        let key = me.key();
        let (thing, mine) = {
            let st = self.lock();
            let subject = st.client.net_id(key).map(AimSubject::Enemy).or_else(|| {
                st.shots
                    .client
                    .by_key
                    .get(&key)
                    .map(|id| AimSubject::Shot(*id))
            });
            let Some(subject) = subject else {
                return false;
            };
            let shown = st.repl.shown_ms.unwrap_or(0);
            let thing = st.shots.aims_client.at(subject, shown);
            (thing, st.me)
        };
        let Some(thing) = thing else {
            return false;
        };
        // This game's own thing (the host picked this player's decoy, egg
        // or construct): its own instance here.
        if Some(thing.owner) == mine {
            let Some((own, x, y)) = self.decoy_own(call, thing) else {
                return false;
            };
            let h = &self.h;
            let Some(v) = h.target else { return false };
            if me.set(v, own).is_err() {
                return false;
            }
            for (v, val) in [
                (h.target_x, f64::from(x)),
                (h.target_y, f64::from(y)),
                (h.target_hs, 0.0),
                (h.target_vs, 0.0),
            ] {
                if let Some(v) = v {
                    let _ = me.set_f64(v, val);
                }
            }
            return true;
        }
        let Some(proxy) = (links.proxy)(thing) else {
            return false;
        };
        let mut at = None;
        let _ = call.with(&proxy, |inst, _| {
            if let (Some(x), Some(y)) = (read(&inst, bv::X), read(&inst, bv::Y)) {
                at = Some((x, y));
            }
        });
        let Some((x, y)) = at else {
            return false;
        };
        let h = &self.h;
        let Some(v) = h.target else { return false };
        if me.set(v, proxy).is_err() {
            return false;
        }
        for (v, val) in [
            (h.target_x, f64::from(x)),
            (h.target_y, f64::from(y)),
            (h.target_hs, 0.0),
            (h.target_vs, 0.0),
        ] {
            if let Some(v) = v {
                let _ = me.set_f64(v, val);
            }
        }
        true
    }

    /// Whether a shot's `target` names a player-side instance here (a ship,
    /// an avatar, a construct, a decoy), so the shot is after a player.
    pub(super) fn shot_after_player(&self, call: &Call<'_>, id: &Value) -> bool {
        self.shot_steer_here(call, id) == steer::PLAYER
    }

    /// What a shot steers at here ([`steer`]): a player's thing, something
    /// of the world, or not known (no target).
    fn shot_steer_here(&self, call: &Call<'_>, id: &Value) -> u8 {
        let (Some(v), Some(side)) = (self.h.target, self.shot.side.get()) else {
            return steer::UNKNOWN;
        };
        let mut t = None;
        let _ = call.with(id, |inst, _| t = inst.get_value(v));
        let Some(t) = t.filter(is_instance) else {
            return steer::UNKNOWN;
        };
        let avatars = self.avatar_keys(call);
        let mut found = false;
        let mut player = false;
        let _ = call.with(&t, |inst, _| {
            found = true;
            // A partner's avatar is that player here (the mirror's rule does
            // not count it as a player's thing).
            player |= avatars.contains(&inst.key()) || (side.0)(call, &inst);
        });
        match (found, player) {
            (_, true) => steer::PLAYER,
            (true, false) => steer::WORLD,
            (false, false) => steer::UNKNOWN,
        }
    }

    /// The partners' avatars here (their ship entities' instances), by key;
    /// found once a frame.
    fn avatar_keys(&self, call: &Call<'_>) -> Arc<HashSet<usize>> {
        let (frame, ships) = {
            let st = self.lock();
            if let Some((f, k)) = &st.shots.avatars
                && *f == st.frame
            {
                return Arc::clone(k);
            }
            let ships: Vec<Value> = st
                .ents
                .written()
                .into_iter()
                .filter(|(k, _, _)| *k == super::EntityKey::ship(k.owner))
                .map(|(_, v, _)| v)
                .collect();
            (st.frame, ships)
        };
        let mut keys = HashSet::new();
        for v in ships {
            let _ = call.with(&v, |i, _| {
                keys.insert(i.key());
            });
        }
        let keys = Arc::new(keys);
        self.lock().shots.avatars = Some((frame, Arc::clone(&keys)));
        keys
    }

    /// This game's ship: the `oPlayer` instance nearest the local ship's
    /// place (id, x, y).
    fn own_ship(&self, call: &Call<'_>) -> Option<(Value, f32, f32)> {
        let (player, at) = {
            let st = self.lock();
            (st.objects.as_ref().and_then(|o| o.player)?, st.local.pos)
        };
        let mut best: Option<(Value, f32, f32, f32)> = None;
        let _ = call.with(&Value::object(player), |inst, _| {
            let (Some(x), Some(y)) = (read(&inst, bv::X), read(&inst, bv::Y)) else {
                return;
            };
            let Ok(id) = inst.builtin_value("id") else {
                return;
            };
            let d = (x - at.x).hypot(y - at.y);
            if best.as_ref().is_none_or(|b| d < b.3) {
                best = Some((id, x, y, d));
            }
        });
        best.map(|(id, x, y, _)| (id, x, y))
    }

    /// Host: what each turning shared shot steers at now; a change goes to
    /// the joining games ([`ShotSteer`]). One steering at an enemy attack
    /// that is not shared (a Dreadnought's ball its orb feeders steer at)
    /// shares that attack now ([`Self::shot_promote`]): the joining game has
    /// no such attack of its own.
    fn shots_host_steer(&self, call: &Call<'_>) {
        let live: Vec<(usize, Value)> = self
            .lock()
            .shots
            .host
            .live
            .iter()
            .map(|(k, s)| (*k, s.value.clone()))
            .collect();
        for (key, id) in live {
            if self.shot_turn(call, &id) <= 0.0 {
                continue;
            }
            let s = self.shot_steer_here(call, &id);
            if s == steer::WORLD {
                self.shot_promote_steered(call, key, &id);
            }
            let mut st = self.lock();
            let now = st.repl.now_ms;
            st.shots.host.steer_now(key, now, s);
        }
    }

    /// Host: the enemy attack a shared shot steers at (its `target`), shared
    /// from now on when it is not yet ([`Self::shots_host_steer`]).
    fn shot_promote_steered(&self, call: &Call<'_>, own: usize, id: &Value) {
        let (Some(refl), Some(v)) = (self.h.reflect, self.h.target) else {
            return;
        };
        let mut t = None;
        let _ = call.with(id, |inst, _| t = inst.get_value(v));
        let Some(t) = t.filter(is_instance) else {
            return;
        };
        let mut obj = None;
        let _ = call.with(&t, |inst, _| obj = object_of(&inst));
        let Some(obj) = obj else { return };
        if matches!(
            self.class_of(call, obj, false),
            Some(super::Class::EnemyShot)
        ) {
            self.shot_promote(call, &refl, own, &t);
        }
    }

    /// Host: shared shots gone without a site's word (an unknown path), or
    /// changed into another object.
    fn shots_host_sweep(&self, call: &Call<'_>) {
        let live: Vec<(usize, Value, u32)> = self
            .lock()
            .shots
            .host
            .live
            .iter()
            .map(|(k, s)| (*k, s.value.clone(), s.object))
            .collect();
        for (key, id, obj) in live {
            match self.shot_presence(call, &id, obj) {
                Here::Same | Here::Asleep => {}
                Here::Changed => self.shot_ended(key, Some(cause::CHANGED)),
                Here::Gone => self.shot_ended(key, None),
            }
        }
    }

    /// Where a tracked shot is now.
    fn shot_presence(&self, call: &Call<'_>, id: &Value, obj: u32) -> Here {
        if self.exists(call, id) {
            let mut now = None;
            let _ = call.with(id, |inst, _| now = object_of(&inst));
            return if now.is_some_and(|o| o != obj) {
                Here::Changed
            } else {
                Here::Same
            };
        }
        match self.presence(call, id) {
            super::Presence::Asleep => {
                // A freeze deactivated it and the resume brought back only
                // the region around this game's view: a shared shot lives
                // while it is in either player's view, so it runs again.
                self.set_asleep(call, id, false);
                Here::Asleep
            }
            _ => Here::Gone,
        }
    }

    /// Client: scratch shots go.
    fn shots_scratch_clear(&self, call: &Call<'_>) {
        let scratch = {
            let mut st = self.lock();
            st.shots.client.scratch_keys.clear();
            std::mem::take(&mut st.shots.client.scratch)
        };
        for (_, id) in scratch {
            if !self.exists(call, &id) {
                continue;
            }
            self.shot_remove(call, &id, true);
        }
    }

    /// Client: made shots gone with no site's word (an unknown path), or
    /// changed into another object (the local player's reflect: reported).
    fn shots_client_sweep(&self, call: &Call<'_>) {
        let made: Vec<(usize, Value, u32)> = {
            let st = self.lock();
            st.shots
                .client
                .made
                .values()
                .filter(|m| m.gone_at.is_none())
                .map(|m| (m.key, m.value.clone(), m.object))
                .collect()
        };
        for (key, id, obj) in made {
            match self.shot_presence(call, &id, obj) {
                Here::Same | Here::Asleep => {}
                Here::Changed => self.shot_ended(key, Some(cause::CHANGED)),
                Here::Gone => self.shot_ended(key, None),
            }
        }
    }

    /// Client: the host's ends due at the shown time.
    fn shots_client_ends(&self, call: &Call<'_>) {
        let remove = {
            let mut st = self.lock();
            let Some(shown) = st.repl.shown_ms else {
                return;
            };
            st.shots.client.apply_steers(shown);
            let due = st.shots.client.take_ends(shown);
            due.into_iter()
                .map(|(id, _, _)| st.shots.client.apply_end(id))
                .collect::<Vec<_>>()
        };
        for v in remove.into_iter().flatten() {
            self.shot_remove(call, &v, false);
        }
    }

    /// Client: make the host's shots due at the shown time.
    fn shots_client_make(&self, call: &Call<'_>) {
        let due = {
            let mut st = self.lock();
            let Some(shown) = st.repl.shown_ms else {
                return;
            };
            st.shots.client.take_due(shown)
        };
        for d in due {
            let waits = d.waited < sh::WAIT_FRAMES && {
                let st = self.lock();
                d.shot.vars.iter().any(|(_, v)| {
                    names_pending(v, &|n| {
                        st.client.id_of(n).is_none() && st.client.is_pending(n)
                    })
                })
            };
            if waits {
                self.lock().shots.client.wait(d);
                continue;
            }
            self.shot_make(call, &d);
        }
    }

    /// Client: make one shot and give it the host's state. A reference to an
    /// enemy whose copy is not here is `noone` (a death shot's maker is gone
    /// on the host too).
    fn shot_make(&self, call: &Call<'_>, due: &sh::Due) {
        let (Some(refl), Some(create)) = (self.h.reflect, self.h.instance_create) else {
            return;
        };
        let shot = &due.shot;
        let mut noone = 0u64;
        let vars: Vec<(String, StateValue)> = {
            let named = self.gen_named_vars(shot.object, &shot.vars);
            let st = self.lock();
            named
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        missing_to_noone(v, &|n| st.client.id_of(n).is_some(), &mut noone),
                    )
                })
                .collect()
        };
        let builtins: Vec<(u8, StateValue)> = shot
            .builtins
            .iter()
            .map(|(i, v)| (*i, v.clone().expanded()))
            .collect();
        let num = |ix: u8| {
            builtins.iter().find_map(|(i, v)| match v {
                StateValue::Real(n) if *i == ix => Some(*n),
                _ => None,
            })
        };
        let (Some(px), Some(py)) = (num(BUILTIN_X), num(BUILTIN_Y)) else {
            return;
        };
        if shot.replaces {
            self.shot_replace_own(call, &refl, u32::from(shot.object), (px, py), &vars);
        }
        self.shot.making.store(true, Ordering::Relaxed);
        let made = call
            .call_script(
                create,
                &[px.into(), py.into(), Value::object(u32::from(shot.object))],
            )
            .ok()
            .filter(is_instance);
        self.shot.making.store(false, Ordering::Relaxed);
        let Some(id) = made else {
            return;
        };
        let mut key = None;
        let _ = call.with(&id, |inst, _| key = Some(inst.key()));
        let Some(key) = key else {
            return;
        };
        let id_of = |n: NetId| self.lock().client.id_of(n);
        let mut stats = Stats::default();
        state::apply(
            call,
            &refl,
            &id,
            &vars,
            &builtins,
            Some(&shot.alarms),
            &id_of,
            &mut stats,
        );
        self.lock().shots.client.made_for(
            shot.id,
            key,
            id.clone(),
            u32::from(shot.object),
            shot.target,
            shot.steer,
        );
        self.shot_place_refs(call, &refl, &id, shot);
        self.shot_beam_sound(call, &id);
        // Its `target` came from the host's game: after a player on the
        // host, aimed at its player here now.
        self.shot_aim_now(call, &id, shot.target, shot.steer == steer::PLAYER);
    }

    /// Client: a made shot's place references: this game's instance of the
    /// attack's object nearest the host's place (within
    /// [`sh::PLACE_NEAR_PX`]), else `noone` (the state wrote that already);
    /// on it, the host attack's variables it lacks.
    fn shot_place_refs(&self, call: &Call<'_>, r: &state::Reflect, id: &Value, shot: &FiredShot) {
        for (place, &(ix, o, x, y)) in shot.places.iter().enumerate() {
            let named = self.gen_named_vars(shot.object, &[(ix, StateValue::Undefined)]);
            let Some((name, _)) = named.first() else {
                continue;
            };
            let mut best: Option<(Value, f32)> = None;
            let _ = call.with(&Value::object(u32::from(o)), |inst, _| {
                let (Some(ix), Some(iy)) = (read(&inst, bv::X), read(&inst, bv::Y)) else {
                    return;
                };
                let d = (ix - x).hypot(iy - y);
                if d <= sh::PLACE_NEAR_PX
                    && best.as_ref().is_none_or(|b| d < b.1)
                    && let Ok(v) = inst.builtin_value("id")
                {
                    best = Some((v, d));
                }
            });
            if let Some((v, _)) = best
                && state::set_var(call, r, id, name, &v)
            {
                self.place_fill(call, r, &v, o, place, &shot.place_fills);
            }
        }
    }

    /// Client, before making an attack the host shares from now on
    /// ([`FiredShot::replaces`]): silently removes one unshared local
    /// instance of `object`, the nearest whose references to replicated
    /// enemies match the host's state, else the nearest within
    /// [`sh::PLACE_NEAR_PX`].
    fn shot_replace_own(
        &self,
        call: &Call<'_>,
        r: &state::Reflect,
        object: u32,
        (x, y): (f64, f64),
        vars: &[(String, StateValue)],
    ) {
        // Its references to replicated enemies, as this game's copies.
        let refs: Vec<(String, Value)> = {
            let st = self.lock();
            vars.iter()
                .filter_map(|(n, v)| match v {
                    StateValue::Instance(net) => Some((n.clone(), st.client.id_of(*net)?)),
                    _ => None,
                })
                .collect()
        };
        let mut all: Vec<(Value, usize, f64)> = Vec::new();
        let _ = call.with(&Value::object(object), |inst, _| {
            let (Some(ix), Some(iy)) = (read(&inst, bv::X), read(&inst, bv::Y)) else {
                return;
            };
            if let Ok(v) = inst.builtin_value("id") {
                all.push((v, inst.key(), (f64::from(ix) - x).hypot(f64::from(iy) - y)));
            }
        });
        {
            let st = self.lock();
            all.retain(|(_, k, _)| st.shots.client.shared(*k).is_none());
        }
        let key_of = |v: &Value| {
            let mut k = None;
            let _ = call.with(v, |i, _| k = Some(i.key()));
            k
        };
        let want: Vec<(&str, Option<usize>)> =
            refs.iter().map(|(n, v)| (n.as_str(), key_of(v))).collect();
        let same = |cand: &Value| {
            !want.is_empty()
                && want.iter().all(|(n, k)| {
                    k.is_some() && state::get_var(call, r, cand, n).and_then(|v| key_of(&v)) == *k
                })
        };
        let nearest =
            |v: Vec<(Value, usize, f64)>| v.into_iter().min_by(|a, b| a.2.total_cmp(&b.2));
        let matched: Vec<_> = all.iter().filter(|c| same(&c.0)).cloned().collect();
        let best = if matched.is_empty() {
            nearest(all).filter(|c| c.2 <= f64::from(sh::PLACE_NEAR_PX))
        } else {
            nearest(matched)
        };
        if let Some((id, _, _)) = &best {
            self.destroy_silently(call, id);
        }
    }

    /// Client: write on `attack` (a place reference's instance, of object
    /// `object`) the host attack's variables it lacks (`fills` of place
    /// `place`); what it has stays its own.
    fn place_fill(
        &self,
        call: &Call<'_>,
        r: &state::Reflect,
        attack: &Value,
        object: u16,
        place: usize,
        fills: &[sh::PlaceFill],
    ) {
        let mine: Vec<(u16, StateValue)> = fills
            .iter()
            .filter(|(p, _, _)| usize::from(*p) == place)
            .map(|(_, i, v)| (*i, v.clone()))
            .collect();
        if mine.is_empty() {
            return;
        }
        for (name, v) in self.gen_named_vars(object, &mine) {
            let has = state::get_var(call, r, attack, &name)
                .is_some_and(|h| !matches!(h, Value::Undefined));
            if has {
                continue;
            }
            let value = match v {
                StateValue::Real(n) => Value::Real(n),
                StateValue::Int(n) => Value::Int(n),
                StateValue::Bool(b) => Value::Bool(b),
                StateValue::Str(s) => Value::Str(s),
                _ => continue,
            };
            let _ = state::set_var(call, r, attack, &name, &value);
        }
    }
}

/// A shot's place references and their attacks' variables
/// ([`FiredShot::places`], [`FiredShot::place_fills`]).
type Places = (Vec<sh::Place>, Vec<sh::PlaceFill>);

/// A shot's place.
fn shot_pos(call: &Call<'_>, id: &Value) -> Option<Vec2> {
    let mut p = None;
    let _ = call.with(id, |inst, _| {
        if let (Some(x), Some(y)) = (read(&inst, bv::X), read(&inst, bv::Y)) {
            p = Some(Vec2::new(x, y));
        }
    });
    p
}

/// Where a tracked shot is.
enum Here {
    Same,
    Changed,
    Asleep,
    Gone,
}

/// Whether `v` names an enemy (deep) for which `pending` holds.
fn names_pending(v: &StateValue, pending: &dyn Fn(NetId) -> bool) -> bool {
    match v {
        StateValue::Instance(n) => pending(*n),
        StateValue::Array(items) => items.iter().any(|i| names_pending(i, pending)),
        StateValue::Struct(m) => m.iter().any(|(_, i)| names_pending(i, pending)),
        StateValue::Method { bound, .. } => names_pending(bound, pending),
        _ => false,
    }
}

/// `v` with every enemy reference whose copy is not here made `noone`
/// (`Unresolved(0)`: the transfer writes `noone`), counted in `n`.
fn missing_to_noone(v: StateValue, here: &dyn Fn(NetId) -> bool, n: &mut u64) -> StateValue {
    match v {
        StateValue::Instance(id) if !here(id) => {
            *n += 1;
            StateValue::Unresolved(0)
        }
        StateValue::Array(items) => StateValue::Array(
            items
                .into_iter()
                .map(|i| missing_to_noone(i, here, n))
                .collect(),
        ),
        StateValue::Struct(m) => StateValue::Struct(
            m.into_iter()
                .map(|(k, i)| (k, missing_to_noone(i, here, n)))
                .collect(),
        ),
        StateValue::Method { func, bound } => StateValue::Method {
            func,
            bound: Box::new(missing_to_noone(*bound, here, n)),
        },
        other => other,
    }
}

/// Bracket every site of [`sh::END_SITES`].
pub(super) fn register(ctx: &ModContext, w: &Arc<WorldHooks>) {
    let _ = SOUND_VARS.set((ctx.variable("beamSFX").ok(), ctx.variable("loopSFX").ok()));
    let mut n = 0;
    for &(name, class) in sh::END_SITES {
        if class == EndClass::PlayerSweep {
            let (c1, c2) = (Arc::clone(w), Arc::clone(w));
            let r = ctx.hook(
                name,
                move |_| c1.role() != ROLE_OFF && c1.shots_on(),
                move |call, on| {
                    if on {
                        c2.shot_sweep_after(call);
                    }
                },
            );
            n += usize::from(r.is_ok());
            hook_or_warn(name, r);
            continue;
        }
        let (c1, c2) = (Arc::clone(w), Arc::clone(w));
        let r = ctx.hook(
            name,
            move |call| c1.shot_site_before(call, name, class),
            move |call, note| c2.shot_site_after(call, note),
        );
        n += usize::from(r.is_ok());
        hook_or_warn(name, r);
    }
    // A joining game's scratch beams touch nothing.
    for name in sh::BEAM_COLLISIONS {
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.scratch_beam_veto(call)),
        );
    }
    log::info!("co-op shots: shared enemy shots on ({n} end sites bracketed)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_copy_is_noone_and_a_present_one_stays() {
        let here = |n: NetId| n == 1;
        let mut n = 0;
        let v = StateValue::Array(vec![StateValue::Instance(1), StateValue::Instance(2)]);
        let out = missing_to_noone(v, &here, &mut n);
        assert_eq!(
            out,
            StateValue::Array(vec![StateValue::Instance(1), StateValue::Unresolved(0)])
        );
        assert_eq!(n, 1);
        assert!(names_pending(
            &StateValue::Struct(vec![("a".into(), StateValue::Instance(2))]),
            &|n| n == 2
        ));
        assert!(!names_pending(&StateValue::Real(2.0), &|_| true));
    }
}

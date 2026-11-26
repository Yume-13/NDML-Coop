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

//! Game side of the generic mirror (see the module doc of [`super`]).
//!
//! Hooks: the create functions and `reflect` (lineage). The run tick
//! ([`MirrorHooks::run_step`]) reads `with (all)` on the owner and keeps the
//! proxies on the viewer. Id-based calls check `instance_exists` first: a GML
//! error inside a mod's call ends the process.

#![forbid(unsafe_code)]
#![allow(clippy::cast_precision_loss)]
// One tick function per side reads and sends in one pass.
#![allow(clippy::too_many_lines, clippy::single_match_else, clippy::unused_self)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use ndml_net::Message;
use ndml_net::mirror::{MirrorFrame, MirrorLeader, MirrorLeaders, MirrorLooks};
use ndml_net::msg::{NetId, PeerId};
use ndml_sdk::{Builtin, Call, Instance, ModContext, Script, Value};

use super::{
    MirrorSettings, Read, Receiver, Sender, Shown, is_local_aid, is_player_family, is_screen_ui,
    leader_changes,
};

mod draw;

/// Game functions that make or reuse an instance and return it. Not covered:
/// trail segments made inline in `oTrail_Step_2`, `refreshEffects`, menus.
const CREATE_SCRIPTS: [&str; 4] = [
    "gml_Script_instance_create",
    "gml_Script_instance_activate_pooled",
    "gml_Script_createSafe",
    "gml_Script_shockwave_create_depth",
];

/// GML `all`.
const ALL: f64 = -3.0;
/// The game's reflect script ([`super::reflect_mark`]).
const REFLECT_SCRIPT: &str = "gml_Script_reflect";
/// Viewer: proxies created per frame at most (pooled ones are reused first).
const CREATES_PER_FRAME: usize = 96;
/// Viewer: idle proxies kept at most (the rest are destroyed).
const POOL_MAX: usize = 1024;

#[derive(Debug, Clone, Copy)]
struct H {
    create: Option<Script>,
    activate: Option<Builtin>,
    destroy: Option<Builtin>,
    exists: Option<Builtin>,
    name: Option<Builtin>,
    parent: Option<Builtin>,
    asset: Option<Builtin>,
    sprite_exists: Option<Builtin>,
    var_set: Option<Builtin>,
    find: Option<Builtin>,
    /// The game's "follow this instance" variable (hit flashes, glows,
    /// following bursts, enemy parts).
    leader: Option<ndml_sdk::Variable>,
}

/// The shared world as the mirror needs it: net id of a local instance and
/// back.
pub trait WorldLinks {
    /// The net id of the replicated instance at `key`, if it is instance `id`.
    fn net_of(&self, key: usize, id: &Value) -> Option<NetId>;
    /// This game's live instance of `net`.
    fn instance_of(&self, net: NetId) -> Option<Value>;
    /// Shift for the partner's own entries this frame (the ending's
    /// formation); `None`: none.
    fn view_shift(&self) -> Option<(f64, f64)> {
        None
    }
}

/// No shared world (tests, mirror without replication).
#[derive(Debug)]
pub struct NoWorld;

impl WorldLinks for NoWorld {
    fn net_of(&self, _: usize, _: &Value) -> Option<NetId> {
        None
    }
    fn instance_of(&self, _: NetId) -> Option<Value> {
        None
    }
}

/// Where the viewer draws a mirrored instance.
enum Place {
    /// Where the owner has it (it follows nothing replicated).
    Owner,
    /// On its leader here, at the owner's offset.
    On(f64, f64),
    /// Not at all: no instance of its leader here.
    Hidden,
}

/// A follower further than this from its leader is not drawn on it (px):
/// across the arena's wrap, or a leader the game only names.
const FOLLOW_MAX_PX: f64 = 400.0;
/// A key's leader is kept this long after the key was last shown (ms of
/// shown time).
const LEADER_KEEP_MS: u32 = 10_000;
/// A follower's offset is sent again when it moves this much (half px).
const FOLLOW_RESEND: i16 = 2;
/// A leader change rides with its key's poses this many owner frames
/// (besides its reliable message): one lost frame does not lose it.
const FRAME_LEADER_SENDS: u8 = 3;

#[derive(Debug, Default)]
struct Proxy {
    id: Option<Value>,
    sprite: Option<i32>,
    depth: Option<i32>,
    /// The partner's object it stands for (its look), for the idle pool.
    object: u16,
}

#[derive(Debug, Default)]
struct State {
    active: bool,
    /// The run the proxy tables belong to ([`MirrorInput::run`]).
    run: Option<(usize, Option<u64>)>,
    frame: u64,
    /// Object index → player family (by name and ancestry).
    families: HashMap<u32, bool>,
    /// Objects drawn on the screen layer (`oGuiObject` descendants): the
    /// owner's own HUD, never sent even when a player-side instance made them.
    gui: HashSet<u32>,
    /// The ship's aiming and locating aids ([`super::LOCAL_AIDS`]): drawn
    /// for the local player only, so never sent; still player-side.
    aids: HashSet<u32>,
    names: HashMap<u32, String>,
    ship: Option<u32>,
    powers: Option<u32>,
    attack: Option<u32>,
    /// The proxies' object ([`draw::PAINTER`]; `attack` without it).
    painter: Option<u32>,
    /// Draw code objects that read other objects' variables
    /// ([`draw::FOREIGN_READS`]): code object -> (object, its name, variable).
    foreign: HashMap<u32, Vec<(u32, &'static str, &'static str)>>,
    /// Draw code objects whose path reads are kept in every trial
    /// ([`draw::PATH_READS`]): code object -> variables.
    path_reads: HashMap<u32, &'static [&'static str]>,
    draw_owner: draw::OwnerDraw,
    draw_view: draw::ViewerDraw,
    resolved: bool,
    /// Lineage: (instance key, object) of instances made by player-side ones.
    lineage: HashSet<(usize, u32)>,
    /// Create calls in progress (nested through Create events): whether each
    /// one's caller is player-side. Emptied every tick in case a hook was
    /// skipped.
    create_depth: Vec<bool>,
    /// `reflect` calls in progress (nested): self's (key, object) at entry.
    reflect_open: Vec<Option<(usize, u32)>>,
    /// `reflect` calls since the run began: (object before, object after,
    /// reflector player-side) -> calls.
    reflects: HashMap<(u32, u32, bool), u64>,
    sender: Sender,
    receiver: Receiver,
    /// This game's peer, and the partner whose frames the viewer draws (the
    /// mirror lookups, [`MirrorHooks::mirror_key_of`]).
    me: Option<PeerId>,
    partner: Option<PeerId>,
    /// Owner: own clock at the last frame sent ([`super::send_due`]).
    last_send_ms: Option<u32>,
    proxies: HashMap<u16, Proxy>,
    proxy_ids: HashSet<u64>,
    /// Viewer: idle proxies (hidden, still kept) by the object they stood
    /// for, taken by the next key of that object ([`POOL_MAX`] in all).
    pool: HashMap<u16, Vec<Value>>,
    pooled: usize,
    /// Viewer: the frame each key without a proxy was first shown.
    wanting: HashMap<u16, u64>,
    /// Owner: the leader last sent per key.
    leaders_sent: HashMap<u16, MirrorLeader>,
    /// Owner: changes riding with their keys' poses, and the sends left.
    leaders_carried: HashMap<u16, (MirrorLeader, u8)>,
    /// Viewer: the owner time of the change each key's leader is from (an
    /// older one, a late reliable copy, is not taken over a newer one).
    leader_at: HashMap<u16, u32>,
    /// Viewer: the partner's leaders per key, kept across a track's end
    /// until [`LEADER_KEEP_MS`] after the key was last shown.
    leaders: HashMap<u16, MirrorLeader>,
    /// Viewer: the shown time each key was last drawn at (pruning).
    leader_seen: HashMap<u16, u32>,
    /// Viewer: a frozen frame was seen since the last running one; proxies
    /// the game left deactivated after it are activated again.
    frozen_seen: bool,
    /// Viewer: leader changes not shown yet: the owner time of the frame
    /// they came with (sent just before it), the key, the leader (`None`:
    /// follows nothing replicated).
    leader_changes: Vec<(u32, u16, Option<MirrorLeader>)>,
    /// Viewer: the frame and the proxies drawn with the ending's shift in
    /// it ([`MirrorHooks::move_shifted`]).
    shifted_ids: (u64, Vec<Value>),
}

/// The mirror's hooks and state.
#[derive(Debug)]
pub struct MirrorHooks {
    settings: MirrorSettings,
    h: H,
    dh: draw::DrawH,
    observed: draw::Observed,
    st: Mutex<State>,
}

/// What the run tick needs from the session.
#[derive(Debug, Clone, Copy)]
pub struct MirrorInput {
    /// A linked co-op run in progress.
    pub active: bool,
    /// Which run: the run controller's key and its start stamp's bits.
    pub run: (usize, Option<u64>),
    pub me: PeerId,
    /// Own clock now, ms (stamps outgoing frames).
    pub now_ms: u32,
    /// The partner's clock as it should be shown now, ms.
    pub shown_ms: Option<u32>,
    /// The game is frozen (pause, upgrade screen): nothing is sent and no
    /// proxy is touched.
    pub frozen: bool,
}

/// Set while the viewer makes a proxy ([`MirrorHooks::assign_proxies`]):
/// the world's create veto lets it through whatever the painter object
/// counts as there.
static MAKING_PROXY: AtomicBool = AtomicBool::new(false);

/// Whether the create running now is a proxy the viewer makes.
#[must_use]
pub fn making_proxy() -> bool {
    MAKING_PROXY.load(Ordering::Relaxed)
}

fn id_key(v: &Value) -> Option<u64> {
    match v {
        Value::Ref(p) => Some(*p),
        other => other.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64),
    }
}

fn object_of(inst: &Instance<'_>) -> Option<u32> {
    crate::world::classify::object_index(&inst.builtin_value("object_index").ok()?)
}

/// `sprite_index` as a number (-1 = none); sprites come back as refs.
fn sprite_of(inst: &Instance<'_>) -> i32 {
    match inst.builtin_value("sprite_index") {
        Ok(Value::Ref(p)) => (p & 0xFFFF_FFFF) as u32 as i32,
        Ok(v) => v.as_f64().map_or(-1, |n| n as i32),
        Err(_) => -1,
    }
}

/// A sprite ref as the game passes it (`0x01000001_iiiiiiii`).
fn sprite_ref(index: i32) -> Value {
    Value::Ref(0x0100_0001_0000_0000 | u64::from(index as u32))
}

impl MirrorHooks {
    #[must_use]
    pub fn new(ctx: &ModContext, mut settings: MirrorSettings) -> Self {
        // The partner's things run their own Draw code only with the mod's
        // calls contained and their trial errors marked expected: without
        // that, off (one line), and nothing about it is sent.
        if settings.draw && !(crate::guard::calls_contained() && ctx.can_expect_errors()) {
            log::warn!(
                "co-op mirror draw: off (needs the GML guard on and a loader with 'errors expected' scopes); the partner's things draw as their sprites"
            );
            settings.draw = false;
        }
        let b = |n: &str| ctx.builtin(n).ok();
        let mut st = State::default();
        st.receiver.carry_ms = Some(settings.carry_ms);
        Self {
            h: H {
                create: ctx.script("gml_Script_instance_create").ok(),
                activate: b("instance_activate_object"),
                destroy: b("instance_destroy"),
                exists: b("instance_exists"),
                name: b("object_get_name"),
                parent: b("object_get_parent"),
                asset: b("asset_get_index"),
                sprite_exists: b("sprite_exists"),
                var_set: b("variable_instance_set"),
                find: b("instance_find"),
                leader: ctx.variable("leader").ok(),
            },
            dh: draw::DrawH::resolve(ctx),
            observed: draw::Observed::default(),
            settings,
            st: Mutex::new(st),
        }
    }

    /// The slow-frames line (`crate::slow_log`): this player's own
    /// player-side instances the mirror keys (shots, drones, constructs) and
    /// the partner's it draws here. One lock, no walk.
    #[must_use]
    pub fn slow_counts(&self) -> (usize, usize) {
        let st = self.lock();
        (st.sender.len(), st.receiver.len())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::game::lock_or_report(&self.st, "mirror")
    }

    /// Register the hooks.
    pub fn register(ctx: &ModContext, me: &Arc<Self>) {
        for name in CREATE_SCRIPTS {
            let (m, n) = (Arc::clone(me), Arc::clone(me));
            let r = ctx
                .hook_before(name, move |call| m.before_create(call))
                .and_then(|()| ctx.hook_after(name, move |call| n.after_create(call)));
            if let Err(e) = r {
                log::warn!("co-op mirror: {name} hook: {e}; lineage misses what it makes");
            }
        }
        let (m, n) = (Arc::clone(me), Arc::clone(me));
        let r = ctx
            .hook_before(REFLECT_SCRIPT, move |call| m.before_reflect(call))
            .and_then(|()| ctx.hook_after(REFLECT_SCRIPT, move |call| n.after_reflect(call)));
        if let Err(e) = r {
            log::warn!(
                "co-op mirror: {REFLECT_SCRIPT} hook: {e}; a reflected shot is not the player's"
            );
        }
        Self::register_draw(ctx, me);
        log::info!("co-op mirror on");
    }

    /// Whether the code calling a create function is player-side: its self or
    /// other is the ship, `oPowers`, a family member or found by lineage, or
    /// the call is nested in the Create event of something player-side code
    /// is making (`create_depth`).
    fn before_create(&self, call: &mut Call<'_>) {
        let (active, ship, powers, nested) = {
            let st = self.lock();
            (
                st.active,
                st.ship,
                st.powers,
                st.create_depth.iter().any(|&p| p),
            )
        };
        if !active {
            return;
        }
        let who = |inst: Option<Instance<'_>>| inst.and_then(|i| Some((object_of(&i)?, i.key())));
        let creators = [who(call.instance()), who(call.other())];
        let mut player_side = nested;
        for (o, key) in creators.into_iter().flatten() {
            if player_side {
                break;
            }
            // A creator not seen yet (made this frame, before the next pass
            // classified it) is classified now, or its first effects leak.
            player_side = Some(o) == ship
                || Some(o) == powers
                || self.classify(call, o)
                || self.lock().lineage.contains(&(key, o));
        }
        self.lock().create_depth.push(player_side);
    }

    /// Tag what player-side code made, at once (it may make more in the same
    /// frame).
    fn after_create(&self, call: &mut Call<'_>) {
        let player_side = {
            let mut st = self.lock();
            if !st.active {
                return;
            }
            st.create_depth.pop().unwrap_or(false)
        };
        let Some(made) = call.result() else { return };
        if !crate::world::classify::is_instance(&made) {
            return;
        }
        // Tagged or untagged by who made it now: a new life at a freed
        // address, or a pooled instance reused, is not the last life's.
        let _ = call.with(&made, |inst, _| {
            if let Some(o) = object_of(&inst) {
                super::retag(&mut self.lock().lineage, (inst.key(), o), player_side);
            }
        });
    }

    /// `reflect` (before): self's instance and object as the call begins.
    fn before_reflect(&self, call: &mut Call<'_>) {
        let entry = call
            .instance()
            .and_then(|i| Some((i.key(), object_of(&i)?)));
        self.lock().reflect_open.push(entry);
    }

    /// `reflect` (after): the clone `instance_change` made of a shot a
    /// player-side thing reflected is player-side by lineage
    /// ([`super::reflect_mark`]), so its hits on copies are the player's.
    fn after_reflect(&self, call: &mut Call<'_>) {
        let (before, active) = {
            let mut st = self.lock();
            (st.reflect_open.pop().flatten(), st.active)
        };
        if !active {
            return;
        }
        let after = call
            .instance()
            .and_then(|i| Some((i.key(), object_of(&i)?)));
        let reflector = call.other().is_some_and(|o| self.is_player_side(call, &o));
        let mark = super::reflect_mark(before, after, reflector);
        let names = {
            let mut st = self.lock();
            let pair = (
                before.map_or(0, |b| b.1),
                after.map_or(0, |a| a.1),
                reflector,
            );
            let n = st.reflects.entry(pair).or_default();
            *n += 1;
            let first = *n <= 3;
            if let Some(m) = mark {
                st.lineage.insert(m);
            }
            first.then(|| {
                let name = |o: u32| st.names.get(&o).cloned().unwrap_or_else(|| o.to_string());
                (name(pair.0), name(pair.1))
            })
        };
        if let Some((b, a)) = names {
            log::info!(
                "co-op mirror: reflect: {b} -> {a}, reflector player-side {reflector}, marked {}",
                mark.is_some()
            );
        }
    }

    /// Resolve the ship, `oPowers` and `oPlayerAttack` (once per run).
    fn resolve(&self, call: &Call<'_>) {
        if self.lock().resolved {
            return;
        }
        let asset = |n: &str| -> Option<u32> {
            let v = call.call_builtin(self.h.asset?, &[n.into()]).ok()?;
            crate::world::classify::object_index(&v)
        };
        let (ship, powers, attack) = (asset("oPlayer"), asset("oPowers"), asset("oPlayerAttack"));
        let painter = asset(draw::PAINTER);
        let mut foreign = HashMap::new();
        for (code, reads) in draw::FOREIGN_READS {
            let Some(c) = asset(code) else { continue };
            let reads: Vec<(u32, &'static str, &'static str)> = reads
                .iter()
                .filter_map(|&(o, var)| asset(o).map(|i| (i, o, var)))
                .collect();
            foreign.insert(c, reads);
        }
        let path_reads: HashMap<u32, &'static [&'static str]> = draw::PATH_READS
            .iter()
            .filter_map(|&(code, reads)| asset(code).map(|c| (c, reads)))
            .collect();
        let mut st = self.lock();
        st.foreign = foreign;
        st.path_reads = path_reads;
        st.ship = ship;
        st.powers = powers;
        st.attack = attack;
        st.painter = painter;
        if let Some(p) = painter {
            st.names.insert(p, draw::PAINTER.to_owned());
        }
        st.resolved = true;
        log::info!(
            "co-op mirror: oPlayer {ship:?}, oPowers {powers:?}, oPlayerAttack {attack:?}, proxies {} {painter:?}",
            draw::PAINTER
        );
    }

    /// Whether an instance is player-side by the mirror's rule: the local ship, `oPowers`, a
    /// player family member, made by player-side code (lineage; kept while the mirror runs), or a
    /// proxy (the partner's thing drawn here, so the world never takes one as an enemy's
    /// dependency).
    pub fn is_player_side(&self, call: &Call<'_>, inst: &Instance<'_>) -> bool {
        let Some(o) = object_of(inst) else {
            return false;
        };
        self.resolve(call);
        let id = inst.builtin_value("id").ok().as_ref().and_then(id_key);
        let (ship, powers, lineage, proxy) = {
            let st = self.lock();
            (
                st.ship,
                st.powers,
                st.lineage.contains(&(inst.key(), o)),
                id.is_some_and(|k| st.proxy_ids.contains(&k)),
            )
        };
        proxy || Some(o) == ship || Some(o) == powers || lineage || self.classify(call, o)
    }

    /// The [`draw::PATH_READS`] of a code object: its own entry, else its
    /// nearest ancestor's (a child's Draw that runs the parent's with
    /// `event_inherited`); none when no ancestor has one. Cached.
    pub(super) fn path_reads_of(&self, call: &Call<'_>, code: u32) -> &'static [&'static str] {
        if let Some(p) = self.lock().path_reads.get(&code) {
            return p;
        }
        let mut found: &'static [&'static str] = &[];
        let mut cur = code;
        for _ in 0..16 {
            let Some(parent) = self.h.parent.and_then(|b| {
                call.call_builtin(b, &[Value::object(cur)])
                    .ok()
                    .and_then(|p| crate::world::classify::object_index(&p))
                    // object_get_parent returns -100 (no parent).
                    .filter(|p| *p != cur && *p <= 0xFFFF)
            }) else {
                break;
            };
            if let Some(p) = self.lock().path_reads.get(&parent).copied() {
                found = p;
                break;
            }
            cur = parent;
        }
        self.lock().path_reads.insert(code, found);
        found
    }

    /// Classify an object by its name and ancestors' names (cached).
    fn classify(&self, call: &Call<'_>, object: u32) -> bool {
        if let Some(&f) = self.lock().families.get(&object) {
            return f;
        }
        let (Some(name_b), Some(parent_b)) = (self.h.name, self.h.parent) else {
            return false;
        };
        let mut chain = Vec::new();
        let mut cur = Some(object);
        while let Some(o) = cur {
            if chain.len() > 16 {
                break;
            }
            let v = Value::object(o);
            let name = call
                .call_builtin(name_b, std::slice::from_ref(&v))
                .ok()
                .and_then(|n| n.as_str().map(str::to_owned))
                .unwrap_or_default();
            chain.push(name);
            cur = call
                .call_builtin(parent_b, &[v])
                .ok()
                .and_then(|p| crate::world::classify::object_index(&p))
                .filter(|p| *p != o && *p < 100_000);
            if let Some(p) = cur {
                // object_get_parent returns -100 (no parent) as a number.
                if p > 0xFFFF {
                    cur = None;
                }
            }
        }
        let refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        let fam = is_player_family(&refs);
        let mut st = self.lock();
        st.families.insert(object, fam);
        if is_screen_ui(&refs) {
            st.gui.insert(object);
        }
        if is_local_aid(&refs) {
            st.aids.insert(object);
        }
        st.names
            .insert(object, chain.first().cloned().unwrap_or_default());
        fam
    }

    /// The run tick. Returns messages to broadcast.
    pub fn run_step(
        &self,
        call: &Call<'_>,
        input: MirrorInput,
        inbox: Vec<Message>,
        links: &dyn WorldLinks,
    ) -> Vec<Message> {
        if !input.active {
            self.deactivate(call);
            return Vec::new();
        }
        self.resolve(call);
        // A run straight after a run: let the last run's proxies and poses go.
        let new_run = {
            let mut st = self.lock();
            // The session's rule (`state.rs` `run_frame`): another controller,
            // or another start stamp where both are known.
            let (key, stamp) = input.run;
            let new = st
                .run
                .is_some_and(|(k, s)| k != key || (s.is_some() && stamp.is_some() && s != stamp));
            let kept = st.run.filter(|_| !new).and_then(|r| r.1);
            st.run = Some((key, stamp.or(kept)));
            new
        };
        if new_run {
            self.lock().receiver.new_run();
            let (n, there) = self.release_proxies(call);
            log::info!(
                "co-op mirror: a new run; {n} proxies of the last let go ({there} of them still there)"
            );
        }
        let frame = {
            let mut st = self.lock();
            if !st.active {
                st.active = true;
                log::info!("co-op mirror: run active");
            }
            st.frame += 1;
            st.frame
        };
        let mut out = Vec::new();
        if input.frozen {
            self.lock().draw_owner.frozen();
        } else if self.send_now(input.now_ms) {
            out = self.owner_frame(call, input, links);
        }
        self.viewer_frame(call, input, inbox, frame, links);
        out
    }

    /// Owner: whether this step sends a frame ([`super::send_due`]); noted
    /// as sent when it does.
    fn send_now(&self, now_ms: u32) -> bool {
        let mut st = self.lock();
        let due = super::send_due(st.last_send_ms, now_ms);
        if due {
            st.last_send_ms = Some(now_ms);
        }
        due
    }

    fn owner_frame(
        &self,
        call: &Call<'_>,
        input: MirrorInput,
        links: &dyn WorldLinks,
    ) -> Vec<Message> {
        self.lock().create_depth.clear();
        let mut leader_ids: Vec<(usize, Value)> = Vec::new();
        let (families, lineage, gui, aids, attack, skip) = {
            let st = self.lock();
            (
                st.families.clone(),
                st.lineage.clone(),
                st.gui.clone(),
                st.aids.clone(),
                st.attack,
                // The proxies' object is not sent.
                st.painter.into_iter().collect::<Vec<u32>>(),
            )
        };
        let mut reads = Vec::new();
        let mut ids: Vec<(usize, u32, Value)> = Vec::new();
        let mut unknown: HashSet<u32> = HashSet::new();
        let mut alive_lineage = HashSet::new();
        let _ = call.with(&Value::Real(ALL), |inst, _| {
            let Some(o) = object_of(&inst) else { return };
            if Some(o) == attack || skip.contains(&o) {
                return;
            }
            let key = inst.key();
            let fam = match families.get(&o) {
                Some(&f) => f,
                None => {
                    unknown.insert(o);
                    false
                }
            };
            let lin = lineage.contains(&(key, o)) && !gui.contains(&o);
            if lin {
                alive_lineage.insert((key, o));
                // Not sent before its object is classified (below): it
                // may be screen UI or an aiming aid, which are never sent.
                if !families.contains_key(&o) {
                    return;
                }
            }
            // The ship's aiming aids: the partner's screen shows its own.
            if aids.contains(&o) {
                return;
            }
            if !(fam || lin) {
                return;
            }
            let b = |n: &str| inst.builtin_f64(n).unwrap_or(0.0);
            // The id is read once: the frame's id list and the key's life.
            let mut life = 0;
            if let Ok(id) = inst.builtin_value("id") {
                life = id_key(&id).unwrap_or(0);
                ids.push((key, o, id));
            }
            if let Some(l) = self
                .h
                .leader
                .and_then(|v| inst.get_value(v))
                .filter(crate::world::classify::is_instance)
            {
                leader_ids.push((reads.len(), l));
            }
            reads.push(Read {
                inst: key,
                id: life,
                object: o,
                sprite: sprite_of(&inst),
                x: b("x"),
                y: b("y"),
                angle: b("image_angle"),
                frame: b("image_index"),
                xscale: b("image_xscale"),
                yscale: b("image_yscale"),
                alpha: b("image_alpha"),
                blend: b("image_blend") as u32,
                depth: b("depth"),
                visible: b("visible") != 0.0,
            });
        });
        for o in unknown {
            self.classify(call, o);
        }
        let follows = self.follows(call, &reads, &leader_ids, links);
        // The sounds for the first part (`crate::sound`): game calls, so
        // before the mirror lock.
        let sounds = crate::sound::glue::frame_sounds(call);
        let mut st = self.lock();
        st.lineage = alive_lineage;
        let (entries, looks) = st.sender.frame(&reads);
        let leaders = leader_changes(&mut st.leaders_sent, &entries, &follows, FOLLOW_RESEND);
        // Each change also rides with the key's poses of this frame and the
        // next FRAME_LEADER_SENDS - 1 (one lost frame does not lose it); a
        // newer change of the key replaces it.
        for l in &leaders {
            st.leaders_carried.insert(l.key, (*l, FRAME_LEADER_SENDS));
        }
        let carried: Vec<MirrorLeader> = st.leaders_carried.values().map(|(l, _)| *l).collect();
        st.leaders_carried.retain(|_, (_, n)| {
            *n -= 1;
            *n > 0
        });
        let frames = MirrorFrame::split(input.me, input.now_ms, &entries, &carried, sounds);
        let looks = MirrorLooks::split(input.me, &looks);
        let mut msgs: Vec<Message> = Vec::new();
        // Leaders before looks (both reliable and in order): the viewer
        // draws a key once its look is in, and by then it knows what the key
        // follows.
        for l in MirrorLeaders::split(input.me, input.now_ms, &leaders) {
            msgs.push(Message::MirrorLeaders(l));
        }
        for l in looks {
            msgs.push(Message::MirrorLooks(l));
        }
        for f in frames {
            msgs.push(Message::MirrorFrame(f));
        }
        drop(st);
        msgs.extend(self.owner_draw(call, input.me, input.now_ms, &ids));
        msgs
    }

    /// Owner: per read (by index), the replicated instance its `leader` names
    /// (net id) and its offset from it. Leaders that are not replicated (the
    /// ship, a part, a local effect), itself, or further than
    /// [`FOLLOW_MAX_PX`] are left out.
    fn follows(
        &self,
        call: &Call<'_>,
        reads: &[Read],
        leader_ids: &[(usize, Value)],
        links: &dyn WorldLinks,
    ) -> HashMap<usize, (NetId, f64, f64)> {
        let mut out = HashMap::new();
        for (i, id) in leader_ids {
            let Some(r) = reads.get(*i) else { continue };
            // Only a visible instance follows (scale and alpha are read
            // already: no game call).
            if !super::followable(r.alpha, r.xscale, r.yscale) || !self.exists(call, id) {
                continue;
            }
            let mut found = None;
            let _ = call.with(id, |li, _| {
                let b = |n: &str| li.builtin_f64(n).unwrap_or(f64::NAN);
                found = Some((li.key(), b("x"), b("y")));
            });
            let Some((key, lx, ly)) = found else { continue };
            if key == r.inst {
                continue;
            }
            let Some(net) = links.net_of(key, id) else {
                continue;
            };
            let (dx, dy) = (r.x - lx, r.y - ly);
            if dx.hypot(dy) <= FOLLOW_MAX_PX {
                out.insert(*i, (net, dx, dy));
            }
        }
        out
    }

    /// Viewer: where a follower is drawn.
    fn place(
        &self,
        call: &Call<'_>,
        leader: Option<&MirrorLeader>,
        links: &dyn WorldLinks,
    ) -> Place {
        let Some(l) = leader else { return Place::Owner };
        let Some(net) = l.net else {
            return Place::Owner;
        };
        let Some(id) = links.instance_of(net) else {
            return Place::Hidden;
        };
        if !self.exists(call, &id) {
            return Place::Hidden;
        }
        let mut pos = None;
        let _ = call.with(&id, |li, _| {
            if let (Some(x), Some(y)) = (li.builtin_f64("x"), li.builtin_f64("y")) {
                pos = Some((x, y));
            }
        });
        let Some((x, y)) = pos else {
            return Place::Hidden;
        };
        let (dx, dy) = l.offset();
        Place::On(x + dx, y + dy)
    }

    fn viewer_frame(
        &self,
        call: &Call<'_>,
        input: MirrorInput,
        inbox: Vec<Message>,
        frame: u64,
        links: &dyn WorldLinks,
    ) {
        let attack = {
            let mut st = self.lock();
            st.me = Some(input.me);
            for m in inbox {
                match m {
                    Message::MirrorFrame(mut f) if f.peer != input.me => {
                        crate::sound::glue::on_frame(f.peer, f.time_ms, f.part, f.sounds.take());
                        st.partner = Some(f.peer);
                        st.receiver.on_frame(f.time_ms, f.parts, &f.entries);
                        let at = f.time_ms;
                        st.leader_changes
                            .extend(f.leaders.iter().map(|x| (at, x.key, x.net.map(|_| *x))));
                    }
                    Message::MirrorLooks(l) if l.peer != input.me => st.receiver.on_looks(&l.looks),
                    Message::MirrorDraw(d) if d.peer() != input.me && self.settings.draw => {
                        st.draw_view.on_message(d);
                    }
                    Message::MirrorLeaders(l) if l.peer != input.me => {
                        let at = l.time_ms;
                        st.leader_changes
                            .extend(l.leaders.into_iter().map(|x| (at, x.key, x.net.map(|_| x))));
                    }
                    _ => {}
                }
            }
            // A change takes effect at the owner frame it was made at (its
            // message and the frames carrying it say which), not when it
            // arrives: applied at once, a hit flash would move to its next
            // enemy one display delay early.
            st.painter.or(st.attack)
        };
        if input.frozen {
            self.lock().frozen_seen = true;
            return;
        }
        // The game's unfreeze does not reactivate the painter object: wake
        // the proxies on the first running frame.
        if std::mem::take(&mut self.lock().frozen_seen) {
            self.wake_proxies(call);
        }
        let Some(attack) = attack else { return };
        let shown_at = {
            let st = self.lock();
            input.shown_ms.or_else(|| {
                st.receiver
                    .newest()
                    .map(|n| n.saturating_sub(super::DELAY_MS))
            })
        };
        let Some(t) = shown_at else { return };
        crate::sound::glue::shown(t);
        let (shown, leaders) = {
            let mut st = self.lock();
            let shown = st.receiver.shown(t);
            // Changes take effect at each key's own shown time (else by `t`).
            let at_of: HashMap<u16, u32> = shown.iter().map(|s| (s.key, s.at)).collect();
            let st2 = &mut *st;
            let due = super::take_due_leaders(&mut st2.leader_changes, &st2.leaders, &at_of, t);
            {
                let s = &mut *st;
                let _ = super::take_leader_changes(due, &mut s.leaders, &mut s.leader_at);
            }
            // A track's end does not drop its leader: a track that timed out
            // and comes back keeps it.
            for s in &shown {
                st.leader_seen.insert(s.key, t);
            }
            let stale: Vec<u16> = st
                .leaders
                .keys()
                .copied()
                .filter(|k| {
                    st.leader_seen
                        .get(k)
                        .is_none_or(|&seen| t.saturating_sub(seen) > LEADER_KEEP_MS)
                })
                .collect();
            for key in stale {
                // Never shown yet: kept until its first sample shows it.
                if !st.leader_seen.contains_key(&key) {
                    continue;
                }
                st.leader_seen.remove(&key);
                st.leader_at.remove(&key);
                st.leaders.remove(&key);
            }
            st.leader_seen
                .retain(|_, seen| t.saturating_sub(*seen) <= LEADER_KEEP_MS);
            let leaders: HashMap<u16, MirrorLeader> = shown
                .iter()
                .filter_map(|s| st.leaders.get(&s.key).map(|l| (s.key, *l)))
                .collect();
            (shown, leaders)
        };
        // Followers of a replicated instance go on its copy here, or are not
        // drawn without one (a flash on an enemy already killed here).
        let shift = links.view_shift();
        // Keys drawn with the ending's shift.
        let mut shifted: HashSet<u16> = HashSet::new();
        let mut shifted_ids: Vec<Value> = Vec::new();
        let shown: Vec<Shown> = shown
            .into_iter()
            .filter_map(|mut s| {
                let l = leaders.get(&s.key);
                match self.place(call, l, links) {
                    Place::On(x, y) => {
                        s.x = x;
                        s.y = y;
                        Some(s)
                    }
                    Place::Owner => {
                        if let Some((dx, dy)) = shift {
                            s.x += dx;
                            s.y += dy;
                            shifted.insert(s.key);
                        }
                        Some(s)
                    }
                    Place::Hidden => None,
                }
            })
            .collect();
        // Proxies whose instance is gone: hidden and kept for the next key of
        // the same object, or destroyed past the pool's size.
        let gone: Vec<(u16, Option<Value>, u16)> = {
            let mut st = self.lock();
            let keep: HashSet<u16> = shown.iter().map(|s| s.key).collect();
            st.wanting.retain(|k, _| keep.contains(k));
            let gone: Vec<u16> = st
                .proxies
                .keys()
                .copied()
                .filter(|k| !keep.contains(k))
                .collect();
            gone.into_iter()
                .filter_map(|k| st.proxies.remove(&k).map(|p| (k, p.id, p.object)))
                .collect()
        };
        for (key, id, object) in gone {
            self.lock().draw_view.forget(key);
            let Some(id) = id else { continue };
            let k = id_key(&id);
            if let Some(k) = k {
                self.lock().draw_view.proxy_of.remove(&k);
            }
            // Not one drawn with its object's own code (stale draw values).
            let room = {
                let st = self.lock();
                st.pooled < POOL_MAX && !st.draw_view.runs_code(object)
            };
            if room && self.hide(call, &id) {
                let mut st = self.lock();
                st.pool.entry(object).or_default().push(id);
                st.pooled += 1;
                continue;
            }
            if let Some(k) = k {
                self.lock().proxy_ids.remove(&k);
            }
            self.destroy_own(call, id);
        }
        self.assign_proxies(call, &shown, frame, attack);
        // The partner's draw variables of the times drawn.
        self.lock()
            .draw_view
            .advance(shown.iter().map(|s| (s.key, s.at)), t);
        for s in &shown {
            let have = self.lock().proxies.get(&s.key).and_then(|p| p.id.clone());
            // None: over this frame's create cap (see `assign_proxies`).
            let Some(id) = have else { continue };
            if self.apply(call, &id, s) {
                if shifted.contains(&s.key) {
                    shifted_ids.push(id.clone());
                }
            } else {
                // Its instance is gone (or deactivated past waking): the
                // table lets it go and the next frame makes it again, so a
                // proxy never stays tracked and absent.
                let mut st = self.lock();
                st.proxies.remove(&s.key);
                if let Some(k) = id_key(&id) {
                    st.proxy_ids.remove(&k);
                    st.draw_view.proxy_of.remove(&k);
                }
                drop(st);
                self.destroy_own(call, id);
            }
        }
        self.lock().shifted_ids = (frame, shifted_ids);
    }

    /// Give every shown key without a proxy one: an idle one first, then new
    /// ones up to [`CREATES_PER_FRAME`], newest keys first.
    fn assign_proxies(&self, call: &Call<'_>, shown: &[Shown], frame: u64, attack: u32) {
        let mut need: Vec<(u64, u16, usize)> = Vec::new();
        {
            let mut st = self.lock();
            for (i, s) in shown.iter().enumerate() {
                if st.proxies.get(&s.key).is_some_and(|p| p.id.is_some()) {
                    continue;
                }
                let since = *st.wanting.entry(s.key).or_insert(frame);
                // Pooled first: no create, no cap.
                let object = s.look.object;
                if let Some(id) = st.pool.get_mut(&object).and_then(Vec::pop) {
                    st.pooled -= 1;
                    if let Some(k) = id_key(&id) {
                        st.draw_view.proxy_of.insert(k, (s.key, object));
                    }
                    st.proxies.insert(
                        s.key,
                        Proxy {
                            id: Some(id),
                            object,
                            ..Proxy::default()
                        },
                    );
                    st.wanting.remove(&s.key);
                    continue;
                }
                need.push((since, s.key, i));
            }
        }
        // Newest first; among the same frame, the higher key (sent later).
        need.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let Some(create) = self.h.create else { return };
        for &(_, key, i) in need.iter().take(CREATES_PER_FRAME) {
            let s = &shown[i];
            MAKING_PROXY.store(true, Ordering::Relaxed);
            let made = call
                .call_script(create, &[s.x.into(), s.y.into(), Value::object(attack)])
                .ok()
                .filter(crate::world::classify::is_instance);
            MAKING_PROXY.store(false, Ordering::Relaxed);
            let Some(id) = made else { continue };
            let mut st = self.lock();
            st.wanting.remove(&key);
            if let Some(k) = id_key(&id) {
                st.proxy_ids.insert(k);
                st.draw_view.proxy_of.insert(k, (key, s.look.object));
            }
            st.proxies.insert(
                key,
                Proxy {
                    id: Some(id),
                    object: s.look.object,
                    ..Proxy::default()
                },
            );
        }
    }

    /// Hide an idle proxy (kept for the pool). False when its instance is
    /// gone or deactivated (then it is destroyed, not pooled).
    fn hide(&self, call: &Call<'_>, id: &Value) -> bool {
        call.with(id, |inst, _| {
            let _ = inst.set_builtin_f64("visible", 0.0);
        })
        .unwrap_or(0)
            > 0
    }

    /// Write a proxy's pose and look. False when its instance is gone.
    fn apply(&self, call: &Call<'_>, id: &Value, s: &Shown) -> bool {
        let (want_sprite, want_depth, drawable) = {
            let st = self.lock();
            let p = st.proxies.get(&s.key);
            (
                p.and_then(|p| p.sprite) != Some(s.look.sprite),
                p.and_then(|p| p.depth) != Some(s.look.depth),
                Self::drawable(&st, s),
            )
        };
        let n = call
            .with(id, |inst, _| {
                let set = |n: &str, v: f64| {
                    let _ = inst.set_builtin_f64(n, v);
                };
                set("x", s.x);
                set("y", s.y);
                set("image_angle", s.angle);
                set("image_xscale", s.xscale);
                set("image_yscale", s.yscale);
                set("image_alpha", s.alpha);
                set("image_blend", f64::from(s.look.blend));
                set("image_index", s.frame);
                set("image_speed", 0.0);
                set("visible", if drawable { 1.0 } else { 0.0 });
                if want_depth {
                    set("depth", f64::from(s.look.depth));
                }
            })
            .unwrap_or(0);
        if n == 0 {
            return false;
        }
        if want_sprite {
            // No sprite: none on the proxy either (the painter object has
            // one of its own, which its code's `draw_self` would show).
            if s.look.sprite < 0
                && let Some(b) = self.h.var_set
            {
                let _ =
                    call.call_builtin(b, &[id.clone(), "sprite_index".into(), Value::Real(-1.0)]);
            }
            let ok = s.look.sprite >= 0 && self.set_sprite(call, id, s.look.sprite);
            let mut st = self.lock();
            if let Some(p) = st.proxies.get_mut(&s.key) {
                p.sprite = Some(s.look.sprite);
                if !ok && s.look.sprite >= 0 {
                    log::debug!("co-op mirror: sprite {} not set", s.look.sprite);
                }
            }
        }
        if want_depth && let Some(p) = self.lock().proxies.get_mut(&s.key) {
            p.depth = Some(s.look.depth);
        }
        true
    }

    /// Whether a proxy is drawn: visible on the owner, and with a sprite or
    /// with Draw code of its own the viewer runs (trails have no sprite).
    fn drawable(st: &State, s: &Shown) -> bool {
        s.look.visible && (s.look.sprite >= 0 || st.draw_view.runs_code(s.look.object))
    }

    fn set_sprite(&self, call: &Call<'_>, id: &Value, sprite: i32) -> bool {
        let spr = sprite_ref(sprite);
        let ok = self
            .h
            .sprite_exists
            .and_then(|b| call.call_builtin(b, std::slice::from_ref(&spr)).ok())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        ok && self.exists(call, id)
            && self.h.var_set.is_some_and(|b| {
                call.call_builtin(b, &[id.clone(), "sprite_index".into(), spr])
                    .is_ok()
            })
    }

    fn exists(&self, call: &Call<'_>, id: &Value) -> bool {
        self.h.exists.is_some_and(|b| {
            call.call_builtin(b, std::slice::from_ref(id))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
    }

    /// Activate the proxies a freeze left deactivated.
    fn wake_proxies(&self, call: &Call<'_>) {
        let Some(a) = self.h.activate else { return };
        let ids = self.own_ids();
        for id in &ids {
            if !self.exists(call, id) {
                let _ = call.call_builtin(a, std::slice::from_ref(id));
            }
        }
    }

    /// The ending's partner ship placed again at the End Step (the world's
    /// `ending_end_step`): this frame's proxies drawn with the shift moved
    /// with it by `(dx, dy)`, so they stay on the drawn ship.
    pub fn move_shifted(&self, call: &Call<'_>, (dx, dy): (f64, f64)) {
        let ids = {
            let st = self.lock();
            if st.shifted_ids.0 != st.frame {
                return;
            }
            st.shifted_ids.1.clone()
        };
        for id in ids {
            let _ = call.with(&id, |inst, _| {
                if let (Some(x), Some(y)) = (inst.builtin_f64("x"), inst.builtin_f64("y")) {
                    let _ = inst.set_builtin_f64("x", x + dx);
                    let _ = inst.set_builtin_f64("y", y + dy);
                }
            });
        }
    }

    /// Owner side: the (owner, key) under which the partner draws a local
    /// instance; `None` when inactive, not sent, or not keyed yet.
    pub fn mirror_key_of(&self, inst: &Instance<'_>) -> Option<(PeerId, u16)> {
        let st = self.lock();
        if !st.active {
            return None;
        }
        Some((st.me?, st.sender.key_of(inst.key())?))
    }

    /// Viewer side: the proxy drawn this frame for the partner's
    /// `(owner, key)`, if any.
    pub fn proxy_for(&self, owner: PeerId, key: u16) -> Option<Value> {
        let st = self.lock();
        if !st.active || st.partner != Some(owner) || st.me == Some(owner) {
            return None;
        }
        st.proxies.get(&key).and_then(|p| p.id.clone())
    }

    /// Lookup, viewer side: the proxies drawn here this frame for the
    /// partner's instances of `objects` (the owner's object indices, as in
    /// `MirrorLook.object`): (owner, key, object, proxy). Read-only.
    pub fn proxies_of(&self, objects: &[u16]) -> Vec<(PeerId, u16, u16, Value)> {
        let st = self.lock();
        let Some(owner) = st.partner.filter(|p| st.active && st.me != Some(*p)) else {
            return Vec::new();
        };
        st.proxies
            .iter()
            .filter(|(_, p)| objects.contains(&p.object))
            .filter_map(|(k, p)| p.id.clone().map(|id| (owner, *k, p.object, id)))
            .collect()
    }

    /// The proxies' instances, idle ones included.
    pub fn own_ids(&self) -> Vec<Value> {
        let st = self.lock();
        st.proxies
            .values()
            .filter_map(|p| p.id.clone())
            .chain(st.pool.values().flatten().cloned())
            .collect()
    }

    /// Destroy one of the proxies, activating it first if a freeze
    /// deactivated it (deactivated instances are invisible to both calls).
    fn destroy_own(&self, call: &Call<'_>, id: Value) {
        if !self.exists(call, &id)
            && let Some(a) = self.h.activate
        {
            let _ = call.call_builtin(a, std::slice::from_ref(&id));
        }
        if self.exists(call, &id)
            && let Some(d) = self.h.destroy
        {
            let _ = call.call_builtin(d, &[id, Value::Bool(false)]);
        }
    }

    /// Destroy every proxy, idle ones included, and empty the tables that
    /// name them: how many there were and how many still existed.
    fn release_proxies(&self, call: &Call<'_>) -> (usize, usize) {
        let ids: Vec<Value> = {
            let mut st = self.lock();
            st.proxy_ids.clear();
            st.wanting.clear();
            st.draw_view.proxy_of.clear();
            st.pooled = 0;
            let pool: Vec<Value> = st.pool.drain().flat_map(|(_, v)| v).collect();
            st.proxies
                .drain()
                .filter_map(|(_, p)| p.id)
                .chain(pool)
                .collect()
        };
        let there = ids.iter().filter(|id| self.exists(call, id)).count();
        let n = ids.len();
        for id in ids {
            self.destroy_own(call, id);
        }
        (n, there)
    }

    fn deactivate(&self, call: &Call<'_>) {
        {
            let mut st = self.lock();
            if !st.active {
                return;
            }
            st.active = false;
            st.last_send_ms = None;
            st.receiver.clear();
            st.sender = Sender::default();
            st.lineage.clear();
            st.create_depth.clear();
            st.reflect_open.clear();
            st.reflects.clear();
            st.draw_view.clear();
            st.draw_owner = draw::OwnerDraw::default();
            st.leaders.clear();
            st.leader_seen.clear();
            st.frozen_seen = false;
            st.leaders_sent.clear();
            st.leaders_carried.clear();
            st.leader_at.clear();
            log::info!("co-op mirror: run inactive");
            st.partner = None;
        }
        self.release_proxies(call);
    }
}

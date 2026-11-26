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

//! What a down does to what follows the ship: it is gone until the respawn,
//! as in a solo death, while the hidden ship stays.
//!
//! * At the down, every instance of a loadout object or of [`SHIP_BOUND`]
//!   whose variables refer to the ship, or to an instance already taken, is
//!   deactivated. Enemy-side objects, pickups and shots play on.
//! * The constructs ([`DESTROYED`]) get `breakConstructs`'s writes and die
//!   by their own code. They do not come back at the respawn.
//! * The pause and upgrade screens reactivate the room, so the run tick
//!   deactivates again ([`Downed::hold`]). Around the game's own rebuilds
//!   what is gone comes back and the rule runs again after.
//! * What the downed ship's own code makes is ended at the End Step
//!   ([`Downed::settle`]).
//! * The revive reactivates everything and waits a frame before its
//!   `refreshLoadout`.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ndml_sdk::abi::OBJECT_REF_TAG;
use ndml_sdk::{Builtin, Call, Instance, ModContext, Script, Value, Variable};

use crate::world::state::{self, Reflect, is_instance_ref};

/// The object the ship's Destroy event zeroes the hull of
/// (`oPlayer_Destroy_0`: `with (oConstructDrone) hull = 0`).
const DIES_WITH_SHIP: &str = "oConstructDrone";
/// `all`, for `with`.
const ALL: f64 = -3.0;
/// Ancestors whose children are never taken: the enemy side (enemies,
/// asteroids, mines and their shots) and pickups.
const NEVER: [&str; 5] = [
    crate::world::classify::parent::SOLID,
    crate::world::classify::parent::COLLECTABLE,
    crate::world::classify::parent::ENEMY_SHOTS[0],
    crate::world::classify::parent::ENEMY_SHOTS[1],
    crate::world::classify::parent::ENEMY_SHOTS[2],
];
/// How deep an object's ancestry is followed.
const MAX_ANCESTRY: usize = 16;

/// How an object of [`SHIP_BOUND`] follows the ship: what the game's own code
/// tests before it destroys the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follows {
    /// `instance_exists(oPlayer)`: every instance follows this game's ship.
    Ship,
    /// `instance_exists(<variable>)`: an instance follows the ship when one of
    /// its variables names the ship or an instance already taken.
    Var(&'static str),
}

/// What is attached to the ship, follows it or fires from it. The game's own
/// code ends each when the ship (or what it follows) is gone.
pub const SHIP_BOUND: &[(&str, Follows)] = &[
    // Defences and auras drawn on the ship (`Step_2` follows `oPlayer`).
    ("oAegis", Follows::Ship),
    ("oArcBarrierGraphic", Follows::Ship),
    ("oCausticBarrier", Follows::Ship),
    ("oForceArmor", Follows::Ship),
    ("oSanctuaryEffect", Follows::Ship),
    ("oSkirmishBoost", Follows::Ship),
    ("oCalibrate", Follows::Ship),
    ("oCalibrateCenter", Follows::Ship),
    ("oLance", Follows::Ship),
    ("oDiscordOrb", Follows::Ship),
    // The ship's own glow, flash and markers.
    ("oPlayerGlow", Follows::Ship),
    ("oPlayerEffectWhite", Follows::Ship),
    ("oBlinkReticle", Follows::Ship),
    ("oColBlueIntro", Follows::Ship),
    ("oColWhiteIntro", Follows::Ship),
    ("oFinder", Follows::Ship),
    ("oPointer", Follows::Ship),
    ("oScope", Follows::Ship),
    // Charges a weapon makes while firing, on the ship.
    ("oThermalLanceCharged", Follows::Var("leader")),
    ("oThermalLanceChargeEffect", Follows::Var("leader")),
    ("oThermalLanceCurl", Follows::Var("leader")),
    ("oVortexCharge", Follows::Var("leader")),
    // The Blade's look on the ship (the swords are in `DESTROYED`).
    ("oBladeAttached", Follows::Var("leader")),
    // Shields and their indicators, on their target.
    ("oPlayerShield", Follows::Var("target")),
    ("oPlayerShieldEffect", Follows::Var("target")),
    ("oSanctuaryIndicator", Follows::Var("target")),
    ("oHelixController", Follows::Var("target")),
    ("oShockwaveController", Follows::Var("target")),
    ("oShieldAmpGlow", Follows::Var("target")),
    ("oShieldAmpIndicator", Follows::Var("target")),
    ("oShieldAmpPulse", Follows::Var("target")),
    ("oShieldBastion", Follows::Var("target")),
    ("oShieldBastionGlow", Follows::Var("target")),
    ("oShieldReflectIndicator", Follows::Var("target")),
    ("oShieldReflectPulse", Follows::Var("target")),
    ("oShieldShockwavePulse", Follows::Var("target")),
    ("oShieldSiphonIndicator", Follows::Var("target")),
    ("oShieldTemporalIndicator", Follows::Var("target")),
    ("oShieldTemporalPulse", Follows::Var("target")),
    ("oShieldWarpIndicator", Follows::Var("target")),
    ("oShieldWeaponized", Follows::Var("target")),
    // Effects and trails that follow a leader (taken when it is the ship).
    ("oPlayerVisibility", Follows::Var("leader")),
    ("oDyingStarEffect", Follows::Var("leader")),
    ("oArmorStack", Follows::Var("trail")),
    ("oArmorStackEffect", Follows::Var("leader")),
    ("oSiphonParticle", Follows::Var("leader")),
    ("oSpiralTrail", Follows::Var("leader")),
    ("oFollowingGraphic", Follows::Var("leader")),
    ("oFollowingGraphicBlendPos", Follows::Var("leader")),
    ("oFollowingGraphicDynamic", Follows::Var("leader")),
    ("oFollowingGraphicPassive", Follows::Var("leader")),
    ("oFollowingGraphicPulsating", Follows::Var("leader")),
    ("oFollowingGraphicPulsing", Follows::Var("leader")),
];

/// A test on a variable of the instance ([`Break::only`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Test {
    /// Greater than.
    Above(f64),
    /// Equal to.
    Is(f64),
}

/// Whether `x` passes `t`.
fn holds(t: Test, x: f64) -> bool {
    match t {
        Test::Above(v) => x > v,
        Test::Is(v) => (x - v).abs() < f64::EPSILON,
    }
}

/// How the down breaks the instances of one object: `breakConstructs`'s
/// own writes for it.
#[derive(Debug, Clone, Copy)]
pub struct Break {
    pub name: &'static str,
    /// Only the instances where every test holds.
    pub only: &'static [(&'static str, Test)],
    /// Written with `hull = -10`.
    pub also: &'static [(&'static str, f64)],
}

impl Break {
    const fn all(name: &'static str) -> Self {
        Self {
            name,
            only: &[],
            also: &[],
        }
    }
}

/// The constructs destroyed at a down, with `breakConstructs`'s own writes:
/// every mine, turret, ally and drone, the ship's own blades and swords, and
/// the swarms. The Bastion line (`oPowers.shield = 0`) is not written. Each
/// instance then dies by its own code. A drone gets `graceProtocolCooldown`
/// and `iFrames` so Grace Protocol does not save it. A blade gets
/// `wasBroken = 1`: no ricochet. An object taken out of this list is held
/// again.
pub const DESTROYED: [Break; 7] = [
    Break::all("oConstructMine"),
    Break::all("oConstructTurret"),
    Break {
        name: "oConstructDrone",
        only: &[],
        also: &[("graceProtocolCooldown", 10_000_000.0), ("iFrames", 0.0)],
    },
    Break::all("oConstructAlly"),
    Break {
        name: "oBlade",
        only: &[("chain", Test::Above(0.0)), ("shooter", Test::Is(1.0))],
        also: &[("wasBroken", 1.0)],
    },
    Break {
        name: "oSwordObject",
        only: &[("shooter", Test::Is(1.0))],
        also: &[],
    },
    Break::all("oConstructSwarm"),
];
/// What [`DESTROYED`] sets the hull to (`breakConstructs`).
const BROKEN_HULL: f64 = -10.0;

/// A Terminal Directive drone's dash: `droneKamikaze`, its flag, the target
/// it picks and `canDie`.
#[derive(Debug, Clone, Copy)]
struct Dash {
    script: Script,
    dashing: Variable,
    target: Variable,
    can_die: Variable,
}

impl Dash {
    fn resolve(ctx: &ModContext) -> Option<Self> {
        let v = |n: &str| ctx.variable(n).ok();
        Some(Self {
            script: ctx.script("gml_Script_droneKamikaze").ok()?,
            dashing: v("kamikazeActive")?,
            target: v("target")?,
            can_die: v("canDie")?,
        })
    }
}

/// Whether a dashing drone's `target` is the ship (`id`): `droneKamikaze`
/// falls back to `oPlayer.id` when no `oSolid` is left to dash at.
fn aims_at_ship(target: &Value, ship: Option<u32>) -> bool {
    ship.is_some() && id_of(target) == ship
}

/// Whether a `destroyConstruct` call (`hull` of its `self`, its second
/// argument: the build's Reconstitution) would drop an armour fragment at
/// the hidden ship.
fn salvage_at_hidden_ship(hidden: bool, hull: Option<f64>, salvage: Option<bool>) -> bool {
    hidden && hull.is_some_and(|h| h <= 0.0) && salvage == Some(true)
}

/// Whether objects with this ancestry (own name first) are destroyed at a
/// down ([`DESTROYED`]).
fn destroyed_by_chain(chain: &[&str]) -> bool {
    chain.iter().any(|n| destroyed(n))
}

/// Whether `name` is an entry of [`DESTROYED`].
fn destroyed(name: &str) -> bool {
    DESTROYED.iter().any(|b| b.name == name)
}

/// The object of `name` in [`SHIP_BOUND`].
fn bound(name: &str) -> Option<Follows> {
    SHIP_BOUND.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

/// What an object with this ancestry (own name first) does at a down. The
/// nearest listed name decides. An enemy-side or pickup ancestor means never.
fn follows_by_chain(chain: &[&str]) -> Option<Follows> {
    if never_by_chain(chain) {
        return None;
    }
    chain.iter().find_map(|n| bound(n))
}

/// An enemy-side or pickup ancestor anywhere in the ancestry.
fn never_by_chain(chain: &[&str]) -> bool {
    chain.iter().any(|n| NEVER.contains(n))
}

/// While down, whether an instance just made is the downed ship's: made by
/// the ship, by an instance the down took, or by one made this way. Never an
/// enemy-side object or a pickup.
fn ship_made(
    maker: Option<u32>,
    ship: Option<u32>,
    taken: &HashSet<u32>,
    made: &HashSet<u32>,
    never: bool,
) -> bool {
    let Some(m) = maker else { return false };
    !never && (ship == Some(m) || taken.contains(&m) || made.contains(&m))
}

/// The most children of other makers a frame keeps ([`Gone::children`]).
const MAX_CHILDREN: usize = 256;

/// Children whose maker is now the downed ship's (in `made_ids`), to the
/// last generation, move to `made`, ended with it.
fn adopt_children(
    children: &mut Vec<(Value, u32, u32)>,
    made: &mut Vec<(Value, u32)>,
    made_ids: &mut HashSet<u32>,
) {
    loop {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(children)
            .into_iter()
            .partition(|(_, _, m)| made_ids.contains(m));
        *children = rest;
        if mine.is_empty() {
            return;
        }
        for (id, o, _) in mine {
            if let Some(n) = id_of(&id) {
                made_ids.insert(n);
            }
            made.push((id, o));
        }
    }
}

/// What the down deactivated.
#[derive(Debug, Default)]
struct Gone {
    /// (instance id, object).
    ids: Vec<(Value, u32)>,
    /// The objects taken, and those a rebuild made while down: what follows
    /// the ship is taken again by them.
    kinds: HashSet<u32>,
    /// Times they were deactivated again after the game reactivated them.
    again: u32,
    /// The ship's instance id at the down.
    ship: Option<u32>,
    /// Made by the downed ship this frame ([`ship_made`]), ended at the
    /// controller's End Step ([`Downed::settle`]): (instance id, object).
    made: Vec<(Value, u32)>,
    /// Their ids, kept for the down: what they make is the ship's too.
    made_ids: HashSet<u32>,
    /// Made this frame by a maker not yet the ship's: (instance id, object,
    /// maker id). A Create event makes its children before the create
    /// returns, so [`adopt_children`] takes them later. Cleared at the End
    /// Step.
    children: Vec<(Value, u32, u32)>,
    /// Ended while down, by object (the "back" line).
    ended: HashMap<u32, u32>,
}

/// Hooks and state (see the module docs).
#[derive(Debug)]
pub struct Downed {
    reflect: Option<Reflect>,
    asset_get_index: Option<Builtin>,
    object_get_name: Option<Builtin>,
    deactivate: Option<Builtin>,
    activate: Option<Builtin>,
    /// `refreshLoadout` calls in progress.
    refresh_depth: AtomicU32,
    /// Objects of the instances `refreshLoadout` made (run time).
    loadout_made: Mutex<HashSet<u32>>,
    gone: Mutex<Option<Gone>>,
    /// The game's own rebuilds of what follows the ship in progress
    /// (`refreshLoadout`, `refreshDefenses`).
    rebuild_depth: AtomicU32,
    /// At the outermost rebuild's start while down: whether `with` saw the
    /// reactivated instances at once.
    rebuild_seen: AtomicBool,
    /// While down, the game made an instance of an object that follows the
    /// ship (construct assembly refills the drones); the next run tick
    /// applies the rule to it.
    fresh: AtomicBool,
    /// The ship is down and hidden: what its own code makes is ended
    /// ([`ship_made`]). Set at the down, cleared by [`Downed::release`] before
    /// the game's own death, the ending, the respawn or the run's end.
    hidden: AtomicBool,
    /// [`DIES_WITH_SHIP`]'s object index (looked up at the down).
    dies_obj: Mutex<Option<u32>>,
    object_get_parent: Option<Builtin>,
    /// Object index -> what it does at a down ([`follows_by_chain`]), asked
    /// once per object.
    follows: Mutex<HashMap<u32, Option<Follows>>>,
    /// Object index -> an enemy-side or pickup ancestor ([`never_by_chain`]),
    /// filled with [`Self::follows`].
    never: Mutex<HashMap<u32, bool>>,
    /// Object index -> destroyed at a down ([`destroyed_by_chain`]), filled
    /// with [`Self::follows`].
    broken: Mutex<HashMap<u32, bool>>,
    destroy: Option<Builtin>,
    hull: Option<Variable>,
    /// The variables [`DESTROYED`] tests and writes.
    break_vars: HashMap<&'static str, Variable>,
    /// A Terminal Directive drone's dash ([`Downed::hold`]).
    kamikaze: Option<Dash>,
}

impl Downed {
    #[must_use]
    pub fn resolve(ctx: &ModContext) -> Self {
        let b = |n: &str| ctx.builtin(n).ok();
        Self {
            reflect: Reflect::resolve(ctx),
            asset_get_index: b("asset_get_index"),
            object_get_name: b("object_get_name"),
            deactivate: b("instance_deactivate_object"),
            activate: b("instance_activate_object"),
            refresh_depth: AtomicU32::new(0),
            loadout_made: Mutex::new(HashSet::new()),
            gone: Mutex::new(None),
            rebuild_depth: AtomicU32::new(0),
            rebuild_seen: AtomicBool::new(false),
            fresh: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            dies_obj: Mutex::new(None),
            object_get_parent: b("object_get_parent"),
            follows: Mutex::new(HashMap::new()),
            never: Mutex::new(HashMap::new()),
            broken: Mutex::new(HashMap::new()),
            destroy: b("instance_destroy"),
            hull: ctx.variable("hull").ok(),
            break_vars: DESTROYED
                .iter()
                .flat_map(|b| {
                    b.only
                        .iter()
                        .map(|(v, _)| *v)
                        .chain(b.also.iter().map(|(v, _)| *v))
                })
                .filter_map(|v| ctx.variable(v).ok().map(|var| (v, var)))
                .collect(),
            kamikaze: Dash::resolve(ctx),
        }
    }

    /// Whether objects of `o` are attached to the ship ([`SHIP_BOUND`], by
    /// the object or an ancestor). Asks the game the first time only.
    fn follows(&self, call: &Call<'_>, o: u32) -> Option<Follows> {
        if let Some(f) = lock(&self.follows).get(&o) {
            return *f;
        }
        let (Some(get_name), Some(get_parent)) = (self.object_get_name, self.object_get_parent)
        else {
            return None;
        };
        let mut chain: Vec<String> = Vec::new();
        let mut cur = f64::from(o);
        for _ in 0..MAX_ANCESTRY {
            let Some(name) = call
                .call_builtin(get_name, &[cur.into()])
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
            else {
                break;
            };
            chain.push(name);
            // `object_get_parent` gives an object ref of -100 for no parent:
            // only an index the game can hold is kept.
            match call
                .call_builtin(get_parent, &[cur.into()])
                .ok()
                .as_ref()
                .and_then(crate::world::classify::object_index)
                .filter(|p| *p <= 0xFFFF)
            {
                Some(p) => cur = f64::from(p),
                None => break,
            }
        }
        let refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        let f = follows_by_chain(&refs);
        lock(&self.never).insert(o, never_by_chain(&refs));
        lock(&self.broken).insert(o, destroyed_by_chain(&refs));
        lock(&self.follows).insert(o, f);
        f
    }

    /// Whether objects of `o` are enemy-side or pickups (by ancestry).
    fn never(&self, call: &Call<'_>, o: u32) -> bool {
        if let Some(n) = lock(&self.never).get(&o) {
            return *n;
        }
        let _ = self.follows(call, o);
        lock(&self.never).get(&o).copied().unwrap_or(false)
    }

    /// Whether objects of `o` are destroyed at a down ([`DESTROYED`]).
    fn broken(&self, call: &Call<'_>, o: u32) -> bool {
        if let Some(b) = lock(&self.broken).get(&o) {
            return *b;
        }
        let _ = self.follows(call, o);
        lock(&self.broken).get(&o).copied().unwrap_or(false)
    }

    /// While down and hidden: every unbroken instance [`DESTROYED`] names gets
    /// `breakConstructs`'s writes, and its own code ends it. Runs at the down
    /// and each run tick. Returns what was broken, by object.
    fn break_constructs(&self, call: &Call<'_>) -> HashMap<u32, u32> {
        let mut per: HashMap<u32, u32> = HashMap::new();
        let (Some(get), Some(hull)) = (self.asset_get_index, self.hull) else {
            return per;
        };
        for b in &DESTROYED {
            let Ok(obj) = call.call_builtin(get, &[b.name.into()]) else {
                continue;
            };
            if crate::world::classify::object_index(&obj).is_none() {
                continue;
            }
            let _ = call.with(&obj, |inst, _| {
                let num = |v: &str| self.break_vars.get(v).and_then(|var| inst.get_f64(*var));
                if !inst.get_f64(hull).is_some_and(|h| h > BROKEN_HULL)
                    || !b
                        .only
                        .iter()
                        .all(|(v, t)| num(v).is_some_and(|x| holds(*t, x)))
                {
                    return;
                }
                for (v, x) in b.also {
                    if let Some(var) = self.break_vars.get(v) {
                        let _ = inst.set_f64(*var, *x);
                    }
                }
                if inst.set_f64(hull, BROKEN_HULL)
                    && let Some(o) = object_of(&inst)
                {
                    *per.entry(o).or_default() += 1;
                }
            });
        }
        per
    }

    /// While down: `droneKamikaze` run as each dashing Terminal Directive
    /// drone, once a run tick. Its Step runs it only with
    /// `oPowers.playerControl`, which the down holds at 0. A drone whose
    /// fallback target is the hidden ship ends where it is (`canDie = 1`).
    fn drive_kamikaze(&self, call: &Call<'_>) {
        let (Some(dash), Some(get)) = (self.kamikaze, self.asset_get_index) else {
            return;
        };
        let Ok(obj) = call.call_builtin(get, &[DIES_WITH_SHIP.into()]) else {
            return;
        };
        if crate::world::classify::object_index(&obj).is_none() {
            return;
        }
        let mut dashing: Vec<Value> = Vec::new();
        let _ = call.with(&obj, |inst, _| {
            if inst.get_f64(dash.dashing).is_some_and(|k| k != 0.0)
                && let Ok(id) = inst.builtin_value("id")
            {
                dashing.push(id);
            }
        });
        if dashing.is_empty() {
            return;
        }
        let ship = self.ship(call).map(|(id, _)| id);
        for id in dashing {
            let _ = call.with(&id, |inst, _| {
                if let Err(e) = inst.call_script(dash.script, &[Value::Undefined]) {
                    log_limited!(
                        log::Level::Warn,
                        "downed-kamikaze",
                        "co-op rules: down: droneKamikaze: {e}"
                    );
                } else if inst
                    .get_value(dash.target)
                    .is_some_and(|t| aims_at_ship(&t, ship))
                {
                    let _ = inst.set_f64(dash.can_die, 1.0);
                }
            });
        }
    }

    /// Before `destroyConstruct`, while down and hidden: the ship is
    /// deactivated for the call, so the game skips the Reconstitution armour
    /// drop (it drops only while `instance_exists(oPlayer)`). Returns what was
    /// deactivated, for [`Self::salvage_back`].
    pub fn salvage_off(&self, call: &Call<'_>) -> Vec<Value> {
        let hidden = self.hidden();
        if !hidden {
            return Vec::new();
        }
        let (Some(hull), Some(get), Some(deactivate)) =
            (self.hull, self.asset_get_index, self.deactivate)
        else {
            return Vec::new();
        };
        let salvage = call.arg_value(1).and_then(|v| v.as_bool());
        let h = call.instance().and_then(|i| i.get_f64(hull));
        if !salvage_at_hidden_ship(hidden, h, salvage) {
            return Vec::new();
        }
        let Ok(player) = call.call_builtin(get, &["oPlayer".into()]) else {
            return Vec::new();
        };
        let mut ships: Vec<Value> = Vec::new();
        let _ = call.with(&player, |inst, _| {
            if let Ok(id) = inst.builtin_value("id") {
                ships.push(id);
            }
        });
        let out: Vec<Value> = ships
            .into_iter()
            .filter(|id| {
                call.call_builtin(deactivate, std::slice::from_ref(id))
                    .is_ok()
            })
            .collect();
        if !out.is_empty() && active(call, &player) {
            log_limited!(
                log::Level::Warn,
                "downed-salvage",
                "co-op rules: down: the ship is still found after its deactivation; a construct's death may drop its armour"
            );
        }
        out
    }

    /// After `destroyConstruct`: the ship [`Self::salvage_off`] took out is
    /// back.
    pub fn salvage_back(&self, call: &Call<'_>, out: &[Value]) {
        if out.is_empty() {
            return;
        }
        let Some(activate) = self.activate else {
            return;
        };
        for id in out {
            if let Err(e) = call.call_builtin(activate, std::slice::from_ref(id)) {
                log::warn!("co-op rules: down: instance_activate_object({id:?}) (the ship): {e}");
            }
        }
    }

    /// Whether what the ship makes is ended now: down and hidden (one atomic
    /// load).
    #[must_use]
    pub fn hidden(&self) -> bool {
        self.hidden.load(Ordering::Relaxed)
    }

    /// The run ended (the menu): a down of that run is over with it. A game
    /// over otherwise leaves `gone` set, and the next run's first
    /// `refreshLoadout` would be taken for a rebuild while down.
    pub fn end_run(&self) {
        self.hidden.store(false, Ordering::Relaxed);
        let was = lock(&self.gone).take();
        self.fresh.store(false, Ordering::Relaxed);
        self.rebuild_seen.store(false, Ordering::Relaxed);
        *lock(&self.dies_obj) = None;
        if let Some(g) = was {
            log::info!(
                "co-op rules: down: the run ended with the ship down; {} instances of it forgotten",
                g.ids.len()
            );
        }
    }

    /// Whether a down holds what follows the ship.
    #[must_use]
    pub fn is_down(&self) -> bool {
        lock(&self.gone).is_some()
    }

    /// Whether what the downed ship's own code makes is ended now: down and
    /// not released.
    #[must_use]
    pub fn ends_ship_makes(&self) -> bool {
        self.hidden.load(Ordering::Relaxed) && self.is_down()
    }

    /// The ship stops being down and hidden (`why`, for the log): from here
    /// nothing it makes is ended. What it made while hidden is ended first.
    /// What the down holds stays held.
    pub fn release(&self, call: &Call<'_>, why: &str) {
        if !self.hidden.load(Ordering::Relaxed) {
            return;
        }
        self.settle(call);
        if self.unhide() {
            log::info!("co-op rules: down: {why}; what the ship makes is no longer ended");
        }
    }

    /// Clears [`Self::hidden`]; true when it was set.
    fn unhide(&self) -> bool {
        self.hidden.swap(false, Ordering::Relaxed)
    }

    /// `refreshLoadout` starts (hooked before it).
    pub fn refresh_enter(&self) {
        self.refresh_depth.fetch_add(1, Ordering::Relaxed);
    }

    /// `refreshLoadout` ends (hooked after it).
    pub fn refresh_exit(&self) {
        let _ = self
            .refresh_depth
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |d| d.checked_sub(1));
    }

    /// After `instance_create_depth`/`_layer`: inside `refreshLoadout`, the
    /// new instance's object is one the loadout makes.
    /// While down (outside the game's rebuilds, which [`Self::rebuild_exit`]
    /// covers), an instance of an object that follows the ship marks the
    /// next run tick to apply the rule again.
    pub fn after_create(&self, call: &Call<'_>) {
        let loadout = self.refresh_depth.load(Ordering::Relaxed) > 0;
        let gone = lock(&self.gone).is_some();
        let rebuild = self.rebuild_depth.load(Ordering::Relaxed) > 0;
        let down = !rebuild && gone;
        if !loadout && !gone {
            return;
        }
        let Some(id) = call.result() else { return };
        let mut obj = None;
        let _ = call.with(&id, |inst, _| obj = object_of(&inst));
        let Some(o) = obj else { return };
        if down && self.note_ship_made(call, &id, o) {
            return;
        }
        if loadout {
            lock(&self.loadout_made).insert(o);
        }
        if rebuild && let Some(g) = lock(&self.gone).as_mut() {
            g.kinds.insert(o);
        }
        if down {
            let known = lock(&self.loadout_made).contains(&o) || *lock(&self.dies_obj) == Some(o);
            if known || self.follows(call, o).is_some() {
                self.fresh.store(true, Ordering::Relaxed);
            }
        }
    }

    /// While down: an instance the downed ship made ([`ship_made`]) is noted
    /// to be ended at the End Step. True when it was.
    fn note_ship_made(&self, call: &Call<'_>, id: &Value, o: u32) -> bool {
        if !self.hidden.load(Ordering::Relaxed) {
            return false;
        }
        let Some(n) = id_of(id) else { return false };
        let maker = call
            .instance()
            .and_then(|i| i.builtin_value("id").ok())
            .as_ref()
            .and_then(id_of);
        let never = self.never(call, o);
        let mut g = lock(&self.gone);
        let Some(g) = g.as_mut() else { return false };
        let taken: HashSet<u32> = g.ids.iter().filter_map(|(id, _)| id_of(id)).collect();
        if !ship_made(maker, g.ship, &taken, &g.made_ids, never) {
            if let Some(m) = maker
                && !never
                && g.children.len() < MAX_CHILDREN
            {
                g.children.push((id.clone(), o, m));
            }
            return false;
        }
        g.made.push((id.clone(), o));
        g.made_ids.insert(n);
        adopt_children(&mut g.children, &mut g.made, &mut g.made_ids);
        true
    }

    /// While down, at the controller's End Step and at the run tick: what the
    /// downed ship made is ended with `instance_destroy(id, false)`, without
    /// its Destroy event.
    pub fn settle(&self, call: &Call<'_>) {
        let made = match lock(&self.gone).as_mut() {
            Some(g) => {
                g.children.clear();
                if g.made.is_empty() {
                    return;
                }
                std::mem::take(&mut g.made)
            }
            None => return,
        };
        let Some(destroy) = self.destroy else { return };
        let mut per: HashMap<u32, u32> = HashMap::new();
        for (id, o) in made {
            if active(call, &id) {
                match call.call_builtin(destroy, &[id.clone(), Value::Bool(false)]) {
                    Ok(_) => *per.entry(o).or_default() += 1,
                    Err(e) => log::warn!("co-op rules: down: instance_destroy({id:?}): {e}"),
                }
            }
        }
        if per.is_empty() {
            return;
        }
        if let Some(g) = lock(&self.gone).as_mut() {
            for (o, n) in per {
                *g.ended.entry(o).or_default() += n;
            }
        }
    }

    /// Before `refreshLoadout` and `refreshDefenses`: while down, what the down
    /// took comes back for the rebuild. A level-up's `refreshDefenses` replaces
    /// the chain from the ship's links; a deactivated old chain would stay and
    /// return as a second tail at the respawn.
    pub fn rebuild_enter(&self, call: &Call<'_>) {
        if self.rebuild_depth.fetch_add(1, Ordering::Relaxed) > 0 {
            return;
        }
        let ids: Vec<Value> = match lock(&self.gone).as_ref() {
            Some(g) => g.ids.iter().map(|(id, _)| id.clone()).collect(),
            None => return,
        };
        let Some(activate) = self.activate else {
            return;
        };
        for id in &ids {
            if let Err(e) = call.call_builtin(activate, std::slice::from_ref(id)) {
                log::warn!("co-op rules: down: instance_activate_object({id:?}): {e}");
            }
        }
        let seen = ids.iter().filter(|id| active(call, id)).count();
        self.rebuild_seen
            .store(seen == ids.len(), Ordering::Relaxed);
        log::info!(
            "co-op rules: down: the game rebuilds what follows the ship; {} reactivated for it, {seen} active at once",
            ids.len()
        );
    }

    /// The rebuild is over (hooked after it): while down, the rule again.
    pub fn rebuild_exit(&self, call: &Call<'_>) {
        let was = self
            .rebuild_depth
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |d| d.checked_sub(1))
            .unwrap_or(0);
        if was != 1 {
            return;
        }
        let Some(deactivate) = self.deactivate else {
            return;
        };
        let Some(old) = lock(&self.gone).take() else {
            return;
        };
        // What the rebuild destroyed is no longer found; when the
        // reactivation was not seen at once, everything is kept.
        let seen = self.rebuild_seen.load(Ordering::Relaxed);
        let mut gone = Gone {
            ids: Vec::new(),
            again: old.again,
            kinds: old.kinds,
            ship: old.ship,
            made: old.made,
            made_ids: old.made_ids,
            children: old.children,
            ended: old.ended,
        };
        let mut ended = 0;
        for (id, o) in old.ids {
            if !seen || active(call, &id) {
                gone.ids.push((id, o));
            } else {
                ended += 1;
            }
        }
        for (id, _) in &gone.ids {
            let _ = call.call_builtin(deactivate, std::slice::from_ref(id));
        }
        let kept = gone.ids.len();
        *lock(&self.gone) = Some(gone);
        // A rebuild while frozen cannot reach the deactivated ship: the next
        // run tick takes what it made.
        let new = if self.ship_reachable(call) {
            self.take_new(call)
        } else {
            self.fresh.store(true, Ordering::Relaxed);
            "after the freeze".to_owned()
        };
        log::info!(
            "co-op rules: down: after the rebuild {kept} deactivated again, {ended} ended by it, new: {new}"
        );
    }

    /// What follows the ship now: the loadout's instances its links reach,
    /// and the drones. `None` (logged) when the rule cannot be applied.
    fn reach(&self, call: &Call<'_>) -> Option<Vec<(Value, u32)>> {
        let r = self.reflect?;
        let Some((ship_id, ship_obj)) = self.ship(call) else {
            log::warn!("co-op rules: down: no ship found; what follows it is left as it is");
            return None;
        };
        let made = lock(&self.loadout_made).clone();
        if made.is_empty() {
            log::warn!(
                "co-op rules: down: no refreshLoadout seen this run; only what the game's code ties to the ship is taken"
            );
        }
        // Every instance; then the loadout's and the attached kinds' (by
        // object, asked outside the `with`).
        let mut all: Vec<(Value, u32, u32)> = Vec::new();
        let _ = call.with(&Value::Real(ALL), |inst, _| {
            if let (Some(o), Ok(id)) = (object_of(&inst), inst.builtin_value("id"))
                && o != ship_obj
                && let Some(n) = id_of(&id)
            {
                all.push((id, n, o));
            }
        });
        let kinds: HashSet<u32> = all.iter().map(|(_, _, o)| *o).collect();
        let kind_of: HashMap<u32, Option<Follows>> = kinds
            .into_iter()
            .map(|o| (o, self.follows(call, o)))
            .collect();
        // The deployed constructs are destroyed, never held.
        all.retain(|(_, _, o)| !self.broken(call, *o));
        // The ship's own taken whole; the rest by their links.
        let mut taken: HashSet<u32> = HashSet::from([ship_id]);
        let mut objs: Vec<(Value, u32, u32)> = Vec::new();
        for (id, n, o) in all {
            match kind_of.get(&o).copied().flatten() {
                Some(Follows::Ship) => {
                    taken.insert(n);
                    objs.push((id, n, o));
                }
                Some(Follows::Var(_)) => objs.push((id, n, o)),
                None if made.contains(&o) => objs.push((id, n, o)),
                None => {}
            }
        }
        let mut refs: Vec<(Value, u32, u32, HashSet<u32>)> = Vec::new();
        for (id, n, o) in objs {
            match state::raw_vars(call, &r, &id) {
                Ok(vars) => {
                    let to: HashSet<u32> = vars
                        .iter()
                        .filter_map(|(_, v)| refers(v, ship_obj, ship_id))
                        .collect();
                    refs.push((id, n, o, to));
                }
                Err((what, arg, e)) => {
                    log::warn!("co-op rules: down: {what}({id:?}, {arg}) on object {o}: {e}");
                }
            }
        }
        // Follow the links from the ship (and from what is taken whole).
        loop {
            let before = taken.len();
            for (_, n, _, to) in &refs {
                if !taken.contains(n) && to.iter().any(|t| taken.contains(t)) {
                    taken.insert(*n);
                }
            }
            if taken.len() == before {
                break;
            }
        }
        let mut out = Vec::new();
        for (id, n, o, _) in refs {
            if taken.contains(&n) {
                out.push((id, o));
            }
        }
        // The Destroy event's line about the drones (held when they are not
        // destroyed: [`DESTROYED`]).
        if !destroyed(DIES_WITH_SHIP)
            && let Some(obj) = self
                .asset_get_index
                .and_then(|b| call.call_builtin(b, &[DIES_WITH_SHIP.into()]).ok())
                .filter(|v| crate::world::classify::object_index(v).is_some())
        {
            *lock(&self.dies_obj) = crate::world::classify::object_index(&obj);
            let _ = call.with(&obj, |inst, _| {
                if let (Some(o), Ok(id)) = (object_of(&inst), inst.builtin_value("id")) {
                    out.push((id, o));
                }
            });
        }
        Some(out)
    }

    /// The ship is active (not deactivated by a freeze), so [`Self::down`]
    /// can follow its links.
    pub fn ship_reachable(&self, call: &Call<'_>) -> bool {
        self.ship(call).is_some()
    }

    /// The ship went down (`oController`'s run tick, outside every lock).
    pub fn down(&self, call: &Call<'_>) {
        let Some(deactivate) = self.deactivate else {
            return;
        };
        let Some(ids) = self.reach(call) else { return };
        let kinds = ids.iter().map(|(_, o)| *o).collect();
        let gone = Gone {
            ids,
            again: 0,
            kinds,
            ship: self.ship(call).map(|(id, _)| id),
            ..Gone::default()
        };
        let mut per: HashMap<u32, u32> = HashMap::new();
        let mut failed = 0;
        for (id, o) in &gone.ids {
            match call.call_builtin(deactivate, std::slice::from_ref(id)) {
                Ok(_) => *per.entry(*o).or_default() += 1,
                Err(e) => {
                    failed += 1;
                    log::warn!("co-op rules: down: instance_deactivate_object({id:?}): {e}");
                }
            }
        }
        log::info!(
            "co-op rules: down: gone until the respawn (following the ship's links and what the game ties to it; {DIES_WITH_SHIP} by the Destroy event's line): {}{}",
            self.list(call, &per),
            if failed > 0 {
                format!("; {failed} failed")
            } else {
                String::new()
            }
        );
        *lock(&self.gone) = Some(gone);
        self.hidden.store(true, Ordering::Relaxed);
        let broken = self.break_constructs(call);
        log::info!(
            "co-op rules: down: deployed constructs destroyed, the game's own way (hull {BROKEN_HULL}, as breakConstructs): {}",
            self.list(call, &broken)
        );
    }

    /// While down, every run tick: the game's pause and upgrade screen
    /// reactivate the whole room when they end; deactivate the taken ones again.
    pub fn hold(&self, call: &Call<'_>) {
        self.settle(call);
        self.drive_kamikaze(call);
        if self.hidden() {
            let _ = self.break_constructs(call);
        }
        let Some(deactivate) = self.deactivate else {
            return;
        };
        let ids: Vec<Value> = match lock(&self.gone).as_ref() {
            Some(g) => g.ids.iter().map(|(id, _)| id.clone()).collect(),
            None => return,
        };
        let mut again = 0;
        for id in ids {
            if active(call, &id) && call.call_builtin(deactivate, &[id]).is_ok() {
                again += 1;
            }
        }
        if again > 0
            && let Some(g) = lock(&self.gone).as_mut()
        {
            g.again += 1;
        }
        if self.fresh.load(Ordering::Relaxed) && self.ship_reachable(call) {
            self.fresh.store(false, Ordering::Relaxed);
            let new = self.take_new(call);
            if new != "none" {
                log::info!("co-op rules: down: made while down, deactivated too: {new}");
            }
        }
    }

    /// While down: the rule again; what it reaches that is not gone yet is
    /// deactivated and added. The objects found, for the log.
    fn take_new(&self, call: &Call<'_>) -> String {
        let Some(deactivate) = self.deactivate else {
            return "none".to_owned();
        };
        // Only more of what the down took, or of [`SHIP_BOUND`]: a loadout
        // object reached only through a new link stays.
        let (known, mut kinds): (HashSet<u32>, HashSet<u32>) = match lock(&self.gone).as_ref() {
            Some(g) => (
                g.ids.iter().filter_map(|(id, _)| id_of(id)).collect(),
                g.ids
                    .iter()
                    .map(|(_, o)| *o)
                    .chain(g.kinds.iter().copied())
                    .collect(),
            ),
            None => return "none".to_owned(),
        };
        let reached = self.reach(call);
        let Some(reached) = reached else {
            return "none".to_owned();
        };
        kinds.extend(*lock(&self.dies_obj));
        let mut per: HashMap<u32, u32> = HashMap::new();
        let mut add = Vec::new();
        for (id, o) in reached {
            if (kinds.contains(&o) || self.follows(call, o).is_some())
                && id_of(&id).is_some_and(|n| !known.contains(&n))
                && call
                    .call_builtin(deactivate, std::slice::from_ref(&id))
                    .is_ok()
            {
                *per.entry(o).or_default() += 1;
                add.push((id, o));
            }
        }
        if let Some(g) = lock(&self.gone).as_mut() {
            g.ids.extend(add);
        }
        self.list(call, &per)
    }

    /// The revive: everything comes back. True when something was
    /// reactivated: the revive then waits a frame, so that its
    /// `refreshLoadout` finds it active whenever an activation is seen.
    pub fn back(&self, call: &Call<'_>) -> bool {
        self.release(call, "the respawn");
        let Some(gone) = lock(&self.gone).take() else {
            return false;
        };
        let Some(activate) = self.activate else {
            return false;
        };
        let mut per: HashMap<u32, u32> = HashMap::new();
        for (id, o) in &gone.ids {
            match call.call_builtin(activate, std::slice::from_ref(id)) {
                Ok(_) => *per.entry(*o).or_default() += 1,
                Err(e) => log::warn!("co-op rules: back: instance_activate_object({id:?}): {e}"),
            }
        }
        log::info!(
            "co-op rules: back: reactivated, refreshLoadout next frame: {} (deactivated again after {} unfreezes; made by the downed ship and ended: {})",
            self.list(call, &per),
            gone.again,
            self.list(call, &gone.ended)
        );
        !gone.ids.is_empty()
    }

    /// After the revive's `refreshLoadout`: what follows the ship now (log).
    pub fn rebuilt(&self, call: &Call<'_>) {
        let (Some(r), Some((ship_id, ship_obj))) = (self.reflect, self.ship(call)) else {
            return;
        };
        let made = lock(&self.loadout_made).clone();
        let mut objs: Vec<(Value, u32)> = Vec::new();
        let _ = call.with(&Value::Real(ALL), |inst, _| {
            if let (Some(o), Ok(id)) = (object_of(&inst), inst.builtin_value("id"))
                && o != ship_obj
                && made.contains(&o)
            {
                objs.push((id, o));
            }
        });
        let mut per: HashMap<u32, u32> = HashMap::new();
        for (id, o) in objs {
            let linked = state::raw_vars(call, &r, &id)
                .unwrap_or_default()
                .iter()
                .any(|(_, v)| refers(v, ship_obj, ship_id) == Some(ship_id));
            if linked {
                *per.entry(o).or_default() += 1;
            }
        }
        log::info!(
            "co-op rules: back: after refreshLoadout, referring to the ship: {}",
            self.list(call, &per)
        );
    }

    /// False while the game is frozen (pause or upgrade screen): the ship is
    /// deactivated then, while `oPowers` steps on. True when that cannot be
    /// told.
    pub fn ship_active(&self, call: &Call<'_>) -> bool {
        let Some(obj) = self
            .asset_get_index
            .and_then(|b| call.call_builtin(b, &["oPlayer".into()]).ok())
        else {
            return true;
        };
        active(call, &obj)
    }

    /// The ship's instance id and object.
    fn ship(&self, call: &Call<'_>) -> Option<(u32, u32)> {
        let obj = call
            .call_builtin(self.asset_get_index?, &["oPlayer".into()])
            .ok()?;
        let mut found = None;
        let _ = call.with(&obj, |inst, _| {
            if found.is_none() {
                found = inst
                    .builtin_value("id")
                    .ok()
                    .as_ref()
                    .and_then(id_of)
                    .zip(object_of(&inst));
            }
        });
        found
    }

    fn list(&self, call: &Call<'_>, m: &HashMap<u32, u32>) -> String {
        if m.is_empty() {
            return "none".to_owned();
        }
        let mut v: Vec<String> = m
            .iter()
            .map(|(o, n)| {
                let name = self
                    .object_get_name
                    .and_then(|b| call.call_builtin(b, &[f64::from(*o).into()]).ok())
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| o.to_string());
                format!("{name} x{n}")
            })
            .collect();
        v.sort();
        v.join(", ")
    }
}

/// Whether `with` reaches the instance (it exists and is active).
fn active(call: &Call<'_>, id: &Value) -> bool {
    let mut found = false;
    let _ = call.with(id, |_, _| found = true);
    found
}

/// An instance's id from its `id` value (a ref or a number).
fn id_of(v: &Value) -> Option<u32> {
    match v {
        Value::Ref(p) if is_instance_ref(*p) => Some((p & 0xFFFF_FFFF) as u32),
        Value::Real(n) if *n >= 100_000.0 && n.fract() == 0.0 => Some(*n as u32),
        Value::Int(n) if *n >= 100_000 => u32::try_from(*n).ok(),
        _ => None,
    }
}

/// The instance `v` names, if any: an instance (by ref or id), or the ship
/// when it names the ship's object (which has that one instance).
fn refers(v: &Value, ship_obj: u32, ship_id: u32) -> Option<u32> {
    match v {
        Value::Ref(p) if *p & 0xFFFF_FFFF_0000_0000 == OBJECT_REF_TAG => {
            ((p & 0xFFFF_FFFF) as u32 == ship_obj).then_some(ship_id)
        }
        other => id_of(other),
    }
}

/// The instance's object index (a number or an object ref, by runner).
fn object_of(inst: &Instance<'_>) -> Option<u32> {
    inst.builtin_value("object_index")
        .ok()
        .as_ref()
        .and_then(crate::world::classify::object_index)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unresolved() -> Downed {
        Downed {
            reflect: None,
            asset_get_index: None,
            object_get_name: None,
            deactivate: None,
            activate: None,
            refresh_depth: AtomicU32::new(0),
            loadout_made: Mutex::new(HashSet::new()),
            gone: Mutex::new(None),
            rebuild_depth: AtomicU32::new(0),
            rebuild_seen: AtomicBool::new(false),
            fresh: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            dies_obj: Mutex::new(None),
            object_get_parent: None,
            follows: Mutex::new(HashMap::new()),
            never: Mutex::new(HashMap::new()),
            broken: Mutex::new(HashMap::new()),
            destroy: None,
            hull: None,
            break_vars: HashMap::new(),
            kamikaze: None,
        }
    }

    #[test]
    fn ship_bound_names_are_unique() {
        let mut seen = HashSet::new();
        for (n, _) in SHIP_BOUND {
            assert!(seen.insert(*n), "{n} twice");
        }
    }

    /// Aegis and Force Armor follow the ship by name, a Thermal Lance charge
    /// by `leader`; an Ally is a deployed construct (destroyed, not held).
    #[test]
    fn by_name_defences_and_charges_are_attached() {
        assert_eq!(follows_by_chain(&["oAegis"]), Some(Follows::Ship));
        assert_eq!(follows_by_chain(&["oForceArmor"]), Some(Follows::Ship));
        assert_eq!(
            follows_by_chain(&[
                "oConstructAlly",
                "oPlayerAdvancedConstruct",
                "oPlayerConstruct"
            ]),
            None
        );
        assert_eq!(
            follows_by_chain(&["oThermalLanceCharged", "oPlayerProjectile"]),
            Some(Follows::Var("leader"))
        );
        assert_eq!(follows_by_chain(&["oDart"]), None, "independent: plays on");
    }

    /// Constructs, children included, are destroyed at a down. The Bastion
    /// shield stays with the ship.
    #[test]
    fn deployed_constructs_are_destroyed_and_the_ships_own_are_not() {
        let adv = ["oPlayerAdvancedConstruct", "oPlayerConstruct"];
        for chain in [
            &["oConstructTurret", adv[0], adv[1]][..],
            &["oConstructAllyStandard", "oConstructAlly", adv[0], adv[1]][..],
            &["oConstructSuper", "oConstructAlly", adv[0], adv[1]][..],
            &["oConstructSwarm", "oPlayerConstruct"][..],
            &["oConstructMine", "oPlayerConstruct"][..],
            &["oConstructDrone", "oPlayerConstruct"][..],
            &["oSwordObject"][..],
            &["oBlade"][..],
            &["oBladeChild", "oBlade"][..],
        ] {
            assert!(destroyed_by_chain(chain), "{chain:?}");
            assert_eq!(follows_by_chain(chain), None, "{chain:?}: never held");
        }
        // The attached blade is the weapon's look on the ship: held.
        assert!(!destroyed_by_chain(&["oBladeAttached", "oEffect"]));
        assert!(!destroyed_by_chain(&["oShieldBastion", adv[0], adv[1]]));
        assert!(!destroyed_by_chain(&["oPlayerShield"]));
        // No held object is destroyed, and none destroyed is held.
        for (n, _) in SHIP_BOUND {
            assert!(!destroyed(n), "{n}");
        }
    }

    /// What a remade drone makes in its own Create (its Shielded Drones
    /// `oShield`), and their children, go with it. Another maker's do not.
    #[test]
    fn what_a_remade_construct_made_in_its_create_goes_with_it() {
        let id = Value::Real;
        let mut children = vec![
            (id(100_010.0), 501, 100_020), // the drone's shield
            (id(100_011.0), 9, 100_010),   // the shield's own child
            (id(100_012.0), 501, 100_099), // another maker's
        ];
        let mut made = vec![(id(100_020.0), 300)];
        let mut ids: HashSet<u32> = HashSet::from([100_020]);
        adopt_children(&mut children, &mut made, &mut ids);
        let objs: Vec<u32> = made.iter().map(|m| m.1).collect();
        assert_eq!(objs, [300, 501, 9]);
        assert_eq!(children.len(), 1, "another maker's stays");
        assert!(ids.contains(&100_011));
        // By `ship_made` alone, a child whose maker was not yet the ship's
        // at its own create is not taken.
        assert!(!ship_made(
            Some(100_020),
            Some(1),
            &HashSet::new(),
            &HashSet::new(),
            false
        ));
    }

    /// `breakConstructs`' tests: the ship's blades (`chain > 0`, `shooter ==
    /// 1`) and swords (`shooter == 1`), nothing of an ally or a mine.
    #[test]
    fn the_break_tests_are_the_games() {
        let only = |n: &str| DESTROYED.iter().find(|b| b.name == n).unwrap().only;
        let sword = only("oSwordObject");
        let pass = |tests: &[(&str, Test)], vals: &[(&str, f64)]| {
            tests.iter().all(|(v, t)| {
                vals.iter()
                    .find(|(n, _)| n == v)
                    .is_some_and(|(_, x)| holds(*t, *x))
            })
        };
        assert!(pass(sword, &[("shooter", 1.0)]), "the ship's sword");
        assert!(!pass(sword, &[("shooter", 3.0)]), "an ally's sword");
        assert!(!pass(sword, &[("shooter", 2.0)]), "a mine's sword");
        let blade = only("oBlade");
        assert!(pass(blade, &[("chain", 1.0), ("shooter", 1.0)]));
        assert!(!pass(blade, &[("chain", 0.0), ("shooter", 1.0)]));
        assert!(only("oConstructTurret").is_empty());
    }

    /// No armour fragment drops at a hidden ship. With the ship up a dying
    /// construct drops it.
    #[test]
    fn no_armour_drops_at_a_hidden_ship_only() {
        assert!(salvage_at_hidden_ship(true, Some(-10.0), Some(true)));
        assert!(salvage_at_hidden_ship(true, Some(0.0), Some(true)));
        assert!(
            !salvage_at_hidden_ship(false, Some(-10.0), Some(true)),
            "the ship up: it drops"
        );
        assert!(
            !salvage_at_hidden_ship(true, Some(5.0), Some(true)),
            "not dying"
        );
        assert!(
            !salvage_at_hidden_ship(true, Some(-10.0), Some(false)),
            "no Reconstitution"
        );
        assert!(!salvage_at_hidden_ship(true, Some(-10.0), None));
        assert!(!salvage_at_hidden_ship(true, None, Some(true)));
    }

    /// A dashing drone whose fallback target is the hidden ship (no `oSolid`
    /// left) ends where it is; one with an enemy dashes on.
    #[test]
    fn a_dash_at_the_hidden_ship_is_told() {
        let ship = Some(100_123);
        assert!(aims_at_ship(&Value::Real(100_123.0), ship));
        assert!(aims_at_ship(&Value::Int(100_123), ship));
        assert!(!aims_at_ship(&Value::Real(100_456.0), ship), "an enemy");
        assert!(!aims_at_ship(&Value::Real(100_123.0), None), "no ship");
        assert!(!aims_at_ship(&Value::Undefined, ship));
    }

    #[test]
    fn a_child_inherits_and_the_enemy_side_never_counts() {
        assert_eq!(
            follows_by_chain(&["oSomeNewShieldPulse", "oShieldAmpPulse", "oEffect"]),
            Some(Follows::Var("target"))
        );
        assert_eq!(follows_by_chain(&["oFollowingGraphic", "oSolid"]), None);
        assert_eq!(follows_by_chain(&["oPlayerGlow", "oEnemyBeam"]), None);
        assert_eq!(follows_by_chain(&["oEffect"]), None);
    }

    /// A drone remade while the ship is down, and the flash it makes inside
    /// `with`, are the downed ship's. What plays on its own is not.
    #[test]
    fn what_the_downed_ship_makes_is_its_own() {
        let ship = Some(100_000);
        let taken = HashSet::from([100_010]);
        let mut made = HashSet::new();
        // The ship's Step deploys a drone (self is the ship).
        assert!(ship_made(Some(100_000), ship, &taken, &made, false));
        made.insert(100_020);
        // The new drone makes its flash inside `with`.
        assert!(ship_made(Some(100_020), ship, &taken, &made, false));
        // A taken instance (a shield stepping after a pause) makes something.
        assert!(ship_made(Some(100_010), ship, &taken, &made, false));
        // An enemy, the controller, no `self`: not the ship's.
        assert!(!ship_made(Some(100_500), ship, &taken, &made, false));
        assert!(!ship_made(None, ship, &taken, &made, false));
        // Enemy-side or a pickup, whoever made it.
        assert!(!ship_made(Some(100_000), ship, &taken, &made, true));
        // No ship found at the down: only what the down took counts.
        assert!(!ship_made(
            Some(100_000),
            None,
            &taken,
            &HashSet::new(),
            false
        ));
    }

    #[test]
    fn the_enemy_side_and_pickups_are_never_the_ships() {
        assert!(never_by_chain(&[
            "oZ",
            crate::world::classify::parent::ENEMY_SHOTS[0]
        ]));
        assert!(never_by_chain(&[
            "oX",
            crate::world::classify::parent::COLLECTABLE
        ]));
        assert!(never_by_chain(&[
            "oY",
            crate::world::classify::parent::SOLID
        ]));
        assert!(!never_by_chain(&["oRainbowEffect"]));
        assert!(!never_by_chain(&["oConstructDrone", "oPlayerConstruct"]));
    }

    /// Once released, nothing the ship makes is ended, while what the down
    /// holds stays held. A run's end releases too.
    #[test]
    fn a_release_stops_ending_what_the_ship_makes() {
        let d = unresolved();
        assert!(!d.ends_ship_makes(), "up");
        *lock(&d.gone) = Some(Gone {
            ids: vec![(Value::Real(100_001.0), 640)],
            ship: Some(100_000),
            ..Gone::default()
        });
        d.hidden.store(true, Ordering::Relaxed);
        assert!(d.ends_ship_makes(), "down and hidden");
        assert!(d.unhide());
        assert!(!d.ends_ship_makes(), "released");
        assert!(d.is_down(), "what the down holds stays held");
        assert!(!d.unhide(), "once");
        d.hidden.store(true, Ordering::Relaxed);
        d.end_run();
        assert!(!d.ends_ship_makes() && !d.hidden.load(Ordering::Relaxed));
    }

    /// A game over leaves the ship down; the next run must not start
    /// down (its first `refreshLoadout` would be a rebuild while down).
    #[test]
    fn a_run_end_forgets_the_down() {
        let d = unresolved();
        *lock(&d.gone) = Some(Gone {
            ids: vec![(Value::Real(100_001.0), 640), (Value::Real(100_002.0), 641)],
            again: 2,
            ..Gone::default()
        });
        d.fresh.store(true, Ordering::Relaxed);
        *lock(&d.dies_obj) = Some(77);
        assert!(d.is_down());
        d.end_run();
        assert!(!d.is_down(), "the next run starts up");
        assert!(!d.fresh.load(Ordering::Relaxed));
        assert_eq!(*lock(&d.dies_obj), None);
        d.end_run();
        assert!(!d.is_down(), "again at the menu: nothing");
    }

    #[test]
    fn links_name_instances_and_the_ship_by_its_object() {
        let inst = Value::Ref(state::INSTANCE_REF_TAG | 0x0001_871B);
        assert_eq!(refers(&inst, 603, 100_094), Some(100_123));
        assert_eq!(refers(&Value::Real(100_094.0), 603, 100_094), Some(100_094));
        assert_eq!(
            refers(&Value::Ref(OBJECT_REF_TAG | 0x025B), 603, 100_094),
            Some(100_094)
        );
        assert_eq!(
            refers(&Value::Ref(OBJECT_REF_TAG | 0x025C), 603, 100_094),
            None
        );
        assert_eq!(refers(&Value::Real(12.0), 603, 100_094), None);
        assert_eq!(refers(&Value::Real(-4.0), 603, 100_094), None);
    }
}

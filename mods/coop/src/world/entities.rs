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

//! Remote entities: another player's things shown in this world as inert
//! `oPlayerAttack` instances (no events, never referenced by game code). The
//! only kind is the partner's ship, the avatar. Pure bookkeeping: the glue
//! carries out the [`Action`]s.
//!
//! An entity is created while the world runs (never while frozen), moved
//! every run tick, reactivated or recreated when gone, and destroyed when its
//! owner leaves. After [`MAX_FAILURES`] failed creates it is given up.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use ndml_net::interp::Vec2;
use ndml_net::msg::PeerId;
use ndml_sdk::Value;

/// What a remote entity stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// The owner's ship.
    Ship,
}

/// Identifies an entity: owner, kind and a slot within the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityKey {
    /// The player whose thing it is.
    pub owner: PeerId,
    /// What it stands for.
    pub kind: Kind,
    /// Slot within the kind.
    pub slot: u16,
}

impl EntityKey {
    /// The owner's ship.
    #[must_use]
    pub fn ship(owner: PeerId) -> Self {
        Self {
            owner,
            kind: Kind::Ship,
            slot: 0,
        }
    }
}

/// How an entity should look and where it should be this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Desired {
    /// The entity.
    pub key: EntityKey,
    /// Sprite index (`None` until known; the instance is then invisible).
    pub sprite: Option<u32>,
    /// `image_index`.
    pub frame: f64,
    /// Position.
    pub pos: Vec2,
    /// Velocity, px per second.
    pub vel: Vec2,
    /// `image_angle`.
    pub angle: f32,
    /// `image_xscale`.
    pub xscale: f64,
    /// `image_yscale`.
    pub yscale: f64,
    /// `image_blend`: the owner's co-op colour, or the ship's own blend.
    pub blend: u32,
    /// `image_alpha`.
    pub alpha: f64,
    /// `visible`.
    pub visible: bool,
    /// Enemies may be pointed at it.
    pub targetable: bool,
}

/// What the glue should do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `instance_create(x, y, oPlayerAttack)`, then [`RemoteEntities::created`].
    Create { key: EntityKey, x: f64, y: f64 },
    /// Write the pose and look, and `sprite` when it changed; then
    /// [`RemoteEntities::visited`].
    Update {
        key: EntityKey,
        id: Value,
        d: Desired,
        sprite: Option<u32>,
        first: bool,
    },
    /// `instance_activate_object(id)`: a freeze left it deactivated.
    Activate { id: Value },
    /// `instance_destroy(id, false)` after `instance_exists`.
    Destroy { id: Value },
}

/// Frames an entity may be missing while the world runs before it is recreated.
pub const RECREATE_AFTER: u32 = 60;
/// Failed creates before an entity is given up.
pub const MAX_FAILURES: u32 = 3;

#[derive(Debug, Clone, Default)]
struct Entity {
    id: Option<Value>,
    missing: u32,
    failures: u32,
    sprite: Option<u32>,
    last: Option<Desired>,
}

/// See the module docs.
#[derive(Debug, Clone, Default)]
pub struct RemoteEntities {
    map: BTreeMap<EntityKey, Entity>,
}

impl RemoteEntities {
    /// The actions for this frame. `running`: the world is not frozen.
    pub fn plan(&mut self, desired: &[Desired], running: bool) -> Vec<Action> {
        let mut out = Vec::new();
        let keep: Vec<EntityKey> = desired.iter().map(|d| d.key).collect();
        self.map.retain(|k, e| {
            let stay = keep.contains(k);
            if !stay && let Some(id) = e.id.take() {
                out.push(Action::Destroy { id });
            }
            stay
        });
        for d in desired {
            let e = self.map.entry(d.key).or_default();
            e.last = Some(*d);
            if e.failures >= MAX_FAILURES {
                continue;
            }
            match &e.id {
                None if running => out.push(Action::Create {
                    key: d.key,
                    x: f64::from(d.pos.x),
                    y: f64::from(d.pos.y),
                }),
                None => {}
                Some(id) => {
                    if e.missing > 0 {
                        out.push(Action::Activate { id: id.clone() });
                    }
                    let sprite = d.sprite.filter(|s| e.sprite != Some(*s));
                    out.push(Action::Update {
                        key: d.key,
                        id: id.clone(),
                        d: *d,
                        sprite,
                        first: e.sprite.is_none() && e.missing == 0,
                    });
                }
            }
        }
        // Gone for a while although the world runs: forget it, recreate.
        if running {
            for e in self.map.values_mut() {
                if e.missing > RECREATE_AFTER {
                    e.id = None;
                    e.missing = 0;
                    e.sprite = None;
                }
            }
        }
        out
    }

    /// The create for `key` returned `id` (`None`: it failed).
    pub fn created(&mut self, key: EntityKey, id: Option<Value>) {
        if let Some(e) = self.map.get_mut(&key) {
            match id {
                Some(id) => {
                    e.id = Some(id);
                    e.missing = 0;
                    e.sprite = None;
                }
                None => e.failures += 1,
            }
        }
    }

    /// An update ran: whether it found the instance, and the sprite applied.
    pub fn visited(&mut self, key: EntityKey, found: bool, sprite: Option<u32>) {
        if let Some(e) = self.map.get_mut(&key) {
            if found {
                e.missing = 0;
                if sprite.is_some() {
                    e.sprite = sprite;
                }
            } else {
                e.missing += 1;
            }
        }
    }

    /// A live, targetable entity of `owner` of this kind: its id and pose.
    #[must_use]
    pub fn target(&self, key: EntityKey) -> Option<(Value, Desired)> {
        let e = self.map.get(&key)?;
        let d = e.last?;
        (e.missing == 0 && d.targetable)
            .then(|| e.id.clone().map(|id| (id, d)))
            .flatten()
    }

    /// The instance of a live entity, targetable or not.
    #[must_use]
    pub fn id(&self, key: EntityKey) -> Option<Value> {
        self.map
            .get(&key)
            .filter(|e| e.missing == 0)
            .and_then(|e| e.id.clone())
    }

    /// `owner`'s ship entity moved by `(dx, dy)` after its pose was written:
    /// its id; the written pose moves with it.
    pub fn nudge(&mut self, owner: PeerId, dx: f32, dy: f32) -> Option<Value> {
        let e = self.map.get_mut(&EntityKey::ship(owner))?;
        let id = e.id.clone()?;
        if let Some(d) = e.last.as_mut() {
            d.pos.x += dx;
            d.pos.y += dy;
        }
        Some(id)
    }

    /// The instance of `owner`'s ship entity, if it has one.
    #[must_use]
    pub fn ship_id(&self, owner: PeerId) -> Option<Value> {
        self.map.get(&EntityKey::ship(owner))?.id.clone()
    }

    /// Every entity with an instance and a pose written: key, id, the pose
    /// last written.
    #[must_use]
    pub fn written(&self) -> Vec<(EntityKey, Value, Desired)> {
        self.map
            .iter()
            .filter_map(|(k, e)| Some((*k, e.id.clone()?, e.last?)))
            .collect()
    }

    /// The entity exists in the game (created and seen last frame).
    #[must_use]
    pub fn live(&self, key: EntityKey) -> bool {
        self.map
            .get(&key)
            .is_some_and(|e| e.id.is_some() && e.missing == 0)
    }

    /// Given up after failed creates.
    #[must_use]
    pub fn failed(&self, key: EntityKey) -> bool {
        self.map
            .get(&key)
            .is_some_and(|e| e.failures >= MAX_FAILURES)
    }

    /// Forget everything; the instances to destroy (if they still exist).
    pub fn clear(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.map)
            .into_values()
            .filter_map(|e| e.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: PeerId = PeerId(1);

    fn want(targetable: bool) -> Desired {
        Desired {
            key: EntityKey::ship(P),
            sprite: Some(824),
            frame: 0.0,
            pos: Vec2::new(10.0, 20.0),
            vel: Vec2::default(),
            angle: 90.0,
            xscale: 0.4,
            yscale: 0.4,
            blend: 0x00FF_FFFF,
            alpha: 1.0,
            visible: true,
            targetable,
        }
    }

    fn id() -> Value {
        Value::Real(100_001.0)
    }

    fn update(a: &[Action]) -> Option<(Option<u32>, bool)> {
        match a {
            [Action::Update { sprite, first, .. }] => Some((*sprite, *first)),
            _ => None,
        }
    }

    #[test]
    fn a_nudge_moves_the_written_pose_with_the_instance() {
        let mut r = RemoteEntities::default();
        let key = EntityKey::ship(P);
        assert_eq!(r.nudge(P, 1.0, 1.0), None, "no entity yet");
        let _ = r.plan(&[want(true)], true);
        r.created(key, Some(id()));
        let _ = r.plan(&[want(true)], true);
        r.visited(key, true, Some(824));
        assert_eq!(r.ship_id(P), Some(id()));
        assert_eq!(r.nudge(P, 3.0, -32.0), Some(id()));
        let (_, _, d) = r.written().pop().unwrap();
        assert_eq!(d.pos, Vec2::new(13.0, -12.0));
        assert_eq!(r.nudge(PeerId(2), 1.0, 1.0), None, "another peer's");
    }

    #[test]
    fn create_update_sprite_once_and_destroy() {
        let mut r = RemoteEntities::default();
        let key = EntityKey::ship(P);
        assert!(r.plan(&[want(true)], false).is_empty(), "not while frozen");
        let a = r.plan(&[want(true)], true);
        assert!(matches!(a[..], [Action::Create { .. }]));
        assert!(!r.live(key));
        r.created(key, Some(id()));
        let a = r.plan(&[want(true)], true);
        assert_eq!(update(&a), Some((Some(824), true)));
        r.visited(key, true, Some(824));
        let a = r.plan(&[want(true)], true);
        assert_eq!(update(&a), Some((None, false)), "sprite set once");
        assert!(r.live(key));
        assert_eq!(r.target(key).map(|t| t.0), Some(id()));
        r.plan(&[want(false)], true);
        assert!(r.target(key).is_none(), "cloaked: not a target");
        // Owner gone: destroyed.
        let a = r.plan(&[], true);
        assert_eq!(a, vec![Action::Destroy { id: id() }]);
        assert!(!r.live(key));
    }

    #[test]
    fn missing_entities_are_reactivated_then_recreated() {
        let mut r = RemoteEntities::default();
        let key = EntityKey::ship(P);
        r.plan(&[want(true)], true);
        r.created(key, Some(id()));
        r.visited(key, false, None);
        let a = r.plan(&[want(true)], true);
        assert!(matches!(a[0], Action::Activate { .. }));
        assert!(!r.live(key) && r.target(key).is_none());
        for _ in 0..=RECREATE_AFTER {
            r.visited(key, false, None);
        }
        r.plan(&[want(true)], true);
        let a = r.plan(&[want(true)], true);
        assert!(matches!(a[..], [Action::Create { .. }]), "{a:?}");
    }

    #[test]
    fn failures_give_up_and_clear_returns_ids() {
        let mut r = RemoteEntities::default();
        let key = EntityKey::ship(P);
        for _ in 0..MAX_FAILURES {
            r.plan(&[want(true)], true);
            r.created(key, None);
        }
        assert!(r.failed(key));
        assert!(r.plan(&[want(true)], true).is_empty());
        let mut r = RemoteEntities::default();
        r.plan(&[want(true)], true);
        r.created(key, Some(id()));
        assert_eq!(r.clear(), vec![id()]);
        assert!(r.clear().is_empty());
    }
}

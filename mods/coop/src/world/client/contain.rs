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

//! The client tables' side of the GML error guard (`crate::guard`): which
//! instances are the mod's, and taking a puppet out after one of its events
//! raised a GML error, to be made again from the host's spawn or left out.

#![forbid(unsafe_code)]

use ndml_net::msg::NetId;
use ndml_sdk::{Value, instance_number};

use super::{ClientWorld, State};

impl ClientWorld {
    /// Every instance the client made for the host or a puppet made: live
    /// puppets, their parts, the missiles they fired and what waits on the
    /// drop list. Enemy shots a puppet fires are not tracked, so not marked.
    #[must_use]
    pub fn guarded_instances(&self) -> Vec<Value> {
        let live = self.entries.values().filter_map(|e| match &e.state {
            State::Live { id, .. } => Some(id.clone()),
            _ => None,
        });
        let parts = self.parts.values().map(|p| p.id.clone());
        live.chain(parts)
            .chain(self.dropping.values().cloned())
            .collect()
    }

    /// The puppet that instance `instance` (an id number) is, or belongs to
    /// as a part.
    #[must_use]
    pub fn net_of_instance(&self, instance: i32) -> Option<NetId> {
        let is = |id: &Value| instance_number(id) == Some(instance);
        self.entries
            .values()
            .find(|e| matches!(&e.state, State::Live { id, .. } if is(id)))
            .map(|e| e.spawn.net_id)
            .or_else(|| self.parts.values().find(|p| is(&p.id)).map(|p| p.owner))
    }

    /// Take puppet `net` and its parts out (they go to the drop list) after a
    /// GML error; `again`: pending once more, so it is made again from the
    /// host's spawn and newest state; else skipped (its messages are then
    /// ignored). False if it is not live.
    pub fn redo_after_error(&mut self, net: NetId, again: bool) -> bool {
        let frame = self.frame;
        let Some(e) = self.entries.get_mut(&net) else {
            return false;
        };
        let after = if again {
            State::Pending
        } else {
            State::Skipped
        };
        let State::Live { key, id, .. } = std::mem::replace(&mut e.state, after) else {
            e.state = State::Skipped;
            return false;
        };
        e.since = frame;
        self.by_key.remove(&key);
        self.fresh.remove(&key);
        self.dropping.insert(key, id.clone());
        self.to_drop.push(id);
        let parts: Vec<usize> = self
            .parts
            .iter()
            .filter(|(_, p)| p.owner == net)
            .map(|(k, _)| *k)
            .collect();
        for k in parts {
            if let Some(p) = self.parts.remove(&k) {
                self.dropping.insert(k, p.id.clone());
                self.to_drop.push(p.id);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use ndml_net::msg::{EnemySpawn, Message};
    use ndml_sdk::Value;

    use super::ClientWorld;

    /// Net id 1 live as instance 100001 (key 11) with a part 100002 (key 12).
    fn live_world_with_part() -> (ClientWorld, Value, Value) {
        let mut c = ClientWorld::default();
        c.on_message(Message::EnemySpawn(EnemySpawn {
            net_id: 1,
            object: 0x325,
            x: 100.0,
            y: 200.0,
            angle: 10.0,
            hull: 50.0,
            hull_max: 100.0,
            flags: 0,
        }));
        c.player_stepped();
        assert!(c.begin_frame(None).running);
        let (puppet, part) = (Value::Real(100_001.0), Value::Real(100_002.0));
        c.spawned(1, 11, puppet.clone());
        c.record_part(1, 12, part.clone());
        (c, puppet, part)
    }

    #[test]
    fn redo_drops_and_requeues() {
        let (mut c, puppet, part) = live_world_with_part();
        assert_eq!(c.net_of_instance(100_001), Some(1));
        assert_eq!(
            c.net_of_instance(100_002),
            Some(1),
            "a part maps to its owner"
        );
        assert_eq!(c.guarded_instances().len(), 2);
        assert!(c.redo_after_error(1, true));
        // Both wait on the drop list (still marked) and the spawn is pending again.
        assert_eq!(c.net_of_instance(100_001), None);
        let marked = c.guarded_instances();
        assert!(marked.contains(&puppet) && marked.contains(&part));
        assert_eq!(c.pending(8).len(), 1);
        c.player_stepped();
        let f = c.begin_frame(None);
        assert_eq!(f.drop.len(), 2);
        // Made again under the same net id.
        c.spawned(1, 21, Value::Real(100_003.0));
        assert_eq!(c.net_of_instance(100_003), Some(1));
        // A second error: left out.
        assert!(c.redo_after_error(1, false));
        assert!(c.pending(8).is_empty());
        assert!(!c.redo_after_error(1, true), "no longer live");
    }
}

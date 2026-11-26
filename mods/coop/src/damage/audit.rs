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

//! An independent count of copies' hull changes (pure part; the hooks are in
//! [`super::glue`]). The count is off; [`Audit::register`] names
//! the bracket sources for [`super::cause`] either way.
//!
//! Every hull writer is bracketed; the change of each `oSolid` during a call,
//! minus what nested brackets explained, is attributed to that function
//! (the mod's own writes taken out, [`Audit::own_write`]). At each copy's
//! measurement ([`Audit::measure`]) the bracketed damage is compared with
//! the measured drop: damage the measurement missed is a **leak**, a drop no
//! bracket explains is **unattributed**. Both are counted above 0 (hull
//! below 0 is overkill).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

/// Changes below this are float noise.
const EPS: f64 = 1e-3;

/// Why damage the brackets saw was not in the measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leak {
    /// A heal in the same window cancelled it in the hull difference.
    Netted,
    /// The copy left the game before its next measurement.
    Gone,
    /// Unexplained.
    Unknown,
}

/// Where damage on an instance that is not a measured copy went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flush {
    /// A part (segment, jaw, barrier, ...) of a copy: never measured.
    Part,
    /// An enemy that exists only here (a copy's own missile, a stray).
    Local,
    /// A copy that left the game before its next measurement.
    Gone,
    /// A copy that was already dying here (its kill was claimed), or whose
    /// enemy the host had already killed or removed: overkill.
    AfterKill,
    /// A copy getting its first state: earlier hull changes are replaced by
    /// the host's hull; raises are dropped, lowerings counted apart.
    BeforeState,
}

/// Per source function.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    /// Hull-lowering changes on measured copies, and their sum.
    pub events: u64,
    pub damage: f64,
    /// Of `damage`, what the measurement contained.
    pub captured: f64,
    /// Of `damage`: netted, gone, unknown.
    pub leaked: [f64; 3],
    /// Overkill on copies already dying here.
    pub after_kill: f64,
    /// Hull raised on measured copies.
    pub heal: f64,
    /// Damage on parts and on local-only enemies (not shared by design).
    pub part: f64,
    pub local: f64,
    /// Hull lowered on a copy before its first state ([`Flush::BeforeState`]).
    pub before_state: f64,
    /// Instances that vanished during a call (deaths inside it).
    pub vanished: u64,
}

impl Row {
    fn leaked_total(&self) -> f64 {
        self.leaked.iter().sum()
    }
}

/// Totals over every source.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    /// Measurement points with something on either side.
    pub compares: u64,
    /// Of those, where measurement and brackets agreed within 0.01.
    pub agreed: u64,
    /// Sum of the measurement's drops at those points.
    pub measured: f64,
    /// Drops no bracket explains.
    pub unattributed: f64,
    pub unattributed_events: u64,
}

/// One measurement point compared (for the mismatch rows).
#[derive(Debug, Clone, PartialEq)]
pub struct Compare {
    pub measured: f64,
    pub damage: f64,
    pub heal: f64,
    pub leak: f64,
    pub unattributed: f64,
    /// (source index, damage) seen in the window.
    pub sources: Vec<(u16, f64)>,
}

impl Compare {
    /// Whether the two counts disagree.
    #[must_use]
    pub fn mismatch(&self) -> bool {
        self.leak > 0.01 || self.unattributed > 0.01
    }
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Audit {
    names: Vec<String>,
    rows: Vec<Row>,
    /// Per open bracket: per instance, change already explained by nested
    /// calls (raw, and of the part above 0).
    stack: Vec<HashMap<usize, (f64, f64)>>,
    /// Per instance: (source, change) since its last measurement.
    pending: HashMap<usize, Vec<(u16, f64)>>,
    totals: Totals,
    /// Object of each changed instance (the glue reads it).
    objects: HashMap<usize, u32>,
    /// Damage not measured, by (how, object): parts, local-only, gone.
    by_object: BTreeMap<(&'static str, u32), f64>,
}

impl Audit {
    /// An empty audit.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A source function; returns its index.
    pub fn register(&mut self, name: &str) -> u16 {
        let i = u16::try_from(self.names.len()).unwrap_or(u16::MAX);
        self.names.push(name.to_owned());
        self.rows.push(Row::default());
        i
    }

    /// Source name.
    #[must_use]
    pub fn name(&self, src: u16) -> &str {
        self.names.get(usize::from(src)).map_or("?", String::as_str)
    }

    /// A bracketed call begins (its "before" map is the hook's own state).
    pub fn enter(&mut self) {
        self.stack.push(HashMap::new());
    }

    /// A bracketed call of `src` ended. Returns the instances it changed
    /// (the glue tells [`Self::set_object`] their objects).
    pub fn leave(
        &mut self,
        src: u16,
        before: &HashMap<usize, f64>,
        after: &HashMap<usize, f64>,
    ) -> Vec<usize> {
        let child = self.stack.pop().unwrap_or_default();
        let mut changed = Vec::new();
        let s = usize::from(src);
        for (&key, &b) in before {
            let Some(&a) = after.get(&key) else {
                if let Some(r) = self.rows.get_mut(s) {
                    r.vanished += 1;
                }
                continue;
            };
            if !(a.is_finite() && b.is_finite()) {
                continue;
            }
            let total = a - b;
            let above = a.max(0.0) - b.max(0.0);
            let (child_raw, child_above) = child.get(&key).copied().unwrap_or((0.0, 0.0));
            let excl = above - child_above;
            if excl.abs() > EPS {
                self.pending.entry(key).or_default().push((src, excl));
            }
            if (total - child_raw).abs() > EPS {
                changed.push(key);
            }
            if (total.abs() > EPS || above.abs() > EPS)
                && let Some(parent) = self.stack.last_mut()
            {
                let e = parent.entry(key).or_insert((0.0, 0.0));
                e.0 += total;
                e.1 += above;
            }
        }
        changed
    }

    /// The object of an instance a call changed.
    pub fn set_object(&mut self, key: usize, object: u32) {
        self.objects.insert(key, object);
    }

    /// The mod changed an instance's hull from `old` to `new`.
    pub fn own_write(&mut self, key: usize, old: f64, new: f64) {
        let (delta, above) = (new - old, new.max(0.0) - old.max(0.0));
        if (delta.abs() > EPS || above.abs() > EPS)
            && let Some(top) = self.stack.last_mut()
        {
            let e = top.entry(key).or_insert((0.0, 0.0));
            e.0 += delta;
            e.1 += above;
        }
    }

    /// A copy's measurement point: `measured` is the generic measurement's
    /// drop for the same window. `dying`: the copy's kill was already claimed
    /// (what is seen now is overkill).
    pub fn measure(&mut self, key: usize, measured: f64, dying: bool) -> Option<Compare> {
        let pend = self.pending.remove(&key).unwrap_or_default();
        let damage: f64 = pend.iter().filter(|p| p.1 < 0.0).map(|p| -p.1).sum();
        let heal: f64 = pend.iter().filter(|p| p.1 > 0.0).map(|p| p.1).sum();
        if damage <= EPS && measured <= EPS && heal <= EPS {
            return None;
        }
        let mut sources: Vec<(u16, f64)> = Vec::new();
        for &(src, d) in &pend {
            let r = &mut self.rows[usize::from(src)];
            if d < 0.0 {
                if dying {
                    r.after_kill += -d;
                } else {
                    r.events += 1;
                    r.damage += -d;
                }
                match sources.iter_mut().find(|s| s.0 == src) {
                    Some(s) => s.1 += -d,
                    None => sources.push((src, -d)),
                }
            } else {
                r.heal += d;
            }
        }
        let captured = measured.min(damage);
        let leak = (damage - captured).max(0.0);
        let unattributed = (measured - (damage - heal).max(0.0)).max(0.0);
        if !dying && damage > EPS {
            let reason = if heal > EPS {
                Leak::Netted
            } else {
                Leak::Unknown
            };
            for &(src, d) in &sources {
                let share = d / damage;
                let r = &mut self.rows[usize::from(src)];
                r.captured += captured * share;
                r.leaked[reason as usize] += leak * share;
            }
        }
        let t = &mut self.totals;
        t.compares += 1;
        t.measured += measured;
        if unattributed > 0.01 {
            t.unattributed += unattributed;
            t.unattributed_events += 1;
        }
        let c = Compare {
            measured,
            damage,
            heal,
            leak: if dying { 0.0 } else { leak },
            unattributed,
            sources,
        };
        if !c.mismatch() {
            t.agreed += 1;
        }
        Some(c)
    }

    /// Instances with changes not measured yet.
    #[must_use]
    pub fn pending_keys(&self) -> Vec<usize> {
        self.pending.keys().copied().collect()
    }

    /// Close an instance's window without a measurement. Returns the
    /// damage (hull lowered) that was pending, by source, and the object.
    pub fn flush(&mut self, key: usize, how: Flush) -> (Vec<(u16, f64)>, u32) {
        let object = self.objects.get(&key).copied().unwrap_or(u32::MAX);
        let mut lowered = Vec::new();
        for (src, d) in self.pending.remove(&key).unwrap_or_default() {
            if d < 0.0 {
                lowered.push((src, -d));
                let label = match how {
                    Flush::Part => "parts",
                    Flush::Local => "local-only",
                    Flush::Gone => "gone",
                    Flush::AfterKill => "overkill",
                    Flush::BeforeState => "before its first state",
                };
                *self.by_object.entry((label, object)).or_insert(0.0) += -d;
            }
            let r = &mut self.rows[usize::from(src)];
            if d >= 0.0 {
                if how == Flush::Gone || how == Flush::AfterKill {
                    r.heal += d;
                }
                continue;
            }
            let d = -d;
            match how {
                Flush::Part => r.part += d,
                Flush::Local => r.local += d,
                Flush::AfterKill => r.after_kill += d,
                Flush::BeforeState => r.before_state += d,
                Flush::Gone => {
                    r.events += 1;
                    r.damage += d;
                    r.leaked[Leak::Gone as usize] += d;
                }
            }
        }
        (lowered, object)
    }

    /// Damage counted `gone` (by source, on `object`) whose enemy the host
    /// then reported killed or removed: overkill, not a leak.
    pub fn gone_was_overkill(&mut self, parts: &[(u16, f64)], object: u32) {
        for &(src, d) in parts {
            if let Some(r) = self.rows.get_mut(usize::from(src)) {
                r.events = r.events.saturating_sub(1);
                r.damage -= d;
                r.leaked[Leak::Gone as usize] -= d;
                r.after_kill += d;
            }
            if let Some(g) = self.by_object.get_mut(&("gone", object)) {
                *g -= d;
            }
            *self.by_object.entry(("overkill", object)).or_insert(0.0) += d;
        }
    }

    /// Totals so far.
    #[must_use]
    pub fn totals(&self) -> Totals {
        self.totals
    }

    /// Rows with activity, by name.
    #[must_use]
    pub fn rows(&self) -> Vec<(&str, &Row)> {
        self.names
            .iter()
            .zip(&self.rows)
            .filter(|(_, r)| **r != Row::default())
            .map(|(n, r)| (n.as_str(), r))
            .collect()
    }

    /// The summary line.
    #[must_use]
    pub fn line(&self) -> String {
        let (mut ev, mut dmg, mut cap, mut leak, mut ak, mut part, mut local, mut pre) =
            (0u64, 0.0, 0.0, [0.0; 3], 0.0, 0.0, 0.0, 0.0);
        for r in &self.rows {
            ev += r.events;
            dmg += r.damage;
            cap += r.captured;
            for (l, x) in leak.iter_mut().zip(r.leaked) {
                *l += x;
            }
            ak += r.after_kill;
            part += r.part;
            local += r.local;
            pre += r.before_state;
        }
        let lt: f64 = leak.iter().sum();
        let t = self.totals;
        format!(
            "damage audit: copies {dmg:.1} in {ev} events, captured {cap:.1}, leaked {lt:.1} ({:.3} %; netted {:.1}, gone {:.1}, unknown {:.1}), overkill {ak:.1}, unattributed {:.1} in {} events; parts {part:.1}, local-only {local:.1}, before a first state {pre:.1}; points {} agreed {}",
            if dmg > 0.0 { 100.0 * lt / dmg } else { 0.0 },
            leak[0],
            leak[1],
            leak[2],
            t.unattributed,
            t.unattributed_events,
            t.compares,
            t.agreed
        )
    }

    /// Unmeasured damage by kind and object (`u32::MAX` = unknown object).
    #[must_use]
    pub fn by_object(&self) -> Vec<String> {
        self.by_object
            .iter()
            .map(|((how, obj), d)| format!("damage audit obj {obj} {how}: {d:.1}"))
            .collect()
    }

    /// One line per active source.
    #[must_use]
    pub fn table(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|(n, r)| {
                let mut s = format!(
                    "damage audit src {n}: events {} damage {:.1} captured {:.1} leaked {:.1}",
                    r.events,
                    r.damage,
                    r.captured,
                    r.leaked_total()
                );
                if r.leaked_total() > 0.0 {
                    let _ = write!(
                        s,
                        " (netted {:.1} gone {:.1} unknown {:.1})",
                        r.leaked[0], r.leaked[1], r.leaked[2]
                    );
                }
                for (label, v) in [
                    ("overkill", r.after_kill),
                    ("heal", r.heal),
                    ("parts", r.part),
                    ("local", r.local),
                ] {
                    if v > 0.0 {
                        let _ = write!(s, " {label} {v:.1}");
                    }
                }
                if r.vanished > 0 {
                    let _ = write!(s, " vanished {}", r.vanished);
                }
                s
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: &[(usize, f64)]) -> HashMap<usize, f64> {
        v.iter().copied().collect()
    }

    #[test]
    fn exclusive_attribution_through_nested_calls() {
        let mut a = Audit::new();
        let ev = a.register("oX_Collision_oSolid");
        let hit = a.register("projectileHitsSolid");
        // Outer event: 100 -> 70; inside it the hit script took 25.
        a.enter();
        a.enter();
        let _ = a.leave(hit, &m(&[(1, 100.0)]), &m(&[(1, 75.0)]));
        let _ = a.leave(ev, &m(&[(1, 100.0)]), &m(&[(1, 70.0)]));
        let c = a.measure(1, 30.0, false).unwrap();
        assert!(!c.mismatch(), "{c:?}");
        let rows = a.rows();
        let get = |n: &str| rows.iter().find(|r| r.0 == n).unwrap().1.clone();
        assert!((get("projectileHitsSolid").damage - 25.0).abs() < 1e-9);
        assert!((get("oX_Collision_oSolid").damage - 5.0).abs() < 1e-9);
        assert!((get("projectileHitsSolid").captured - 25.0).abs() < 1e-9);
    }

    #[test]
    fn own_writes_inside_a_bracket_are_not_causes() {
        let mut a = Audit::new();
        let step = a.register("oEnemySegment_Step_2");
        a.enter();
        // The mod wrote the host's hull (+40) inside the bracket.
        a.own_write(1, 60.0, 100.0);
        let _ = a.leave(step, &m(&[(1, 60.0)]), &m(&[(1, 100.0)]));
        assert!(a.measure(1, 0.0, false).is_none());
    }

    #[test]
    fn overkill_below_zero_set_back_to_zero_is_no_leak() {
        // A burst takes a copy from 7.5 to -2110, a write sets it to 0,
        // another burst takes it to -397: 7.5 counted, all measured.
        let mut a = Audit::new();
        let hit = a.register("burstHitsSolid");
        let touch = a.register("oPlayerConstruct_Collision_oEnemyExplosive");
        a.enter();
        let _ = a.leave(hit, &m(&[(1, 7.5)]), &m(&[(1, -2110.0)]));
        a.enter();
        let _ = a.leave(touch, &m(&[(1, -2110.0)]), &m(&[(1, 0.0)]));
        a.enter();
        let _ = a.leave(hit, &m(&[(1, 0.0)]), &m(&[(1, -397.0)]));
        let c = a.measure(1, 7.5, false).unwrap();
        assert!((c.damage - 7.5).abs() < 1e-9, "{c:?}");
        assert!(
            c.leak < 1e-9 && c.unattributed < 1e-9 && c.heal < 1e-9,
            "{c:?}"
        );
    }

    #[test]
    fn a_set_up_raise_before_the_first_state_does_not_net_later_damage() {
        // Set-up raised a new copy's hull from 0 to 2700, the first state
        // wrote 675, a burst took 471: the raise does not net it away.
        let mut a = Audit::new();
        let init = a.register("enemyInit");
        let hit = a.register("burstHitsSolid");
        a.enter();
        let _ = a.leave(init, &m(&[(1, 0.0)]), &m(&[(1, 2700.0)]));
        let (lowered, _) = a.flush(1, Flush::BeforeState);
        assert!(lowered.is_empty());
        a.enter();
        let _ = a.leave(hit, &m(&[(1, 675.0)]), &m(&[(1, 204.0)]));
        let c = a.measure(1, 471.0, false).unwrap();
        assert!(
            c.unattributed < 1e-9 && c.leak < 1e-9 && c.heal < 1e-9,
            "{c:?}"
        );
        assert!(a.totals().unattributed < 1e-9);
    }

    #[test]
    fn leaks_and_unattributed() {
        let mut a = Audit::new();
        let hit = a.register("burstHitsSolid");
        let heal = a.register("enemyHealingAura");
        // 20 damage and a 5 heal in one window: the measurement sees 15.
        a.enter();
        let _ = a.leave(hit, &m(&[(1, 100.0)]), &m(&[(1, 80.0)]));
        a.enter();
        let _ = a.leave(heal, &m(&[(1, 80.0)]), &m(&[(1, 85.0)]));
        let c = a.measure(1, 15.0, false).unwrap();
        assert!((c.leak - 5.0).abs() < 1e-9 && c.unattributed < 1e-9);
        let r = a
            .rows()
            .into_iter()
            .find(|r| r.0 == "burstHitsSolid")
            .unwrap();
        assert!((r.1.leaked[Leak::Netted as usize] - 5.0).abs() < 1e-9);
        // A drop no bracket saw.
        let c = a.measure(2, 7.0, false).unwrap();
        assert!((c.unattributed - 7.0).abs() < 1e-9 && c.mismatch());
        assert_eq!(a.totals().unattributed_events, 1);
        // Gone before its measurement; parts; local.
        a.enter();
        let _ = a.leave(
            hit,
            &m(&[(3, 50.0), (4, 9.0), (5, 9.0)]),
            &m(&[(3, 40.0), (4, 8.0), (5, 7.0)]),
        );
        let mut keys = a.pending_keys();
        keys.sort_unstable();
        assert_eq!(keys, vec![3, 4, 5]);
        a.flush(3, Flush::Gone);
        a.flush(4, Flush::Part);
        a.flush(5, Flush::Local);
        let r = a
            .rows()
            .into_iter()
            .find(|r| r.0 == "burstHitsSolid")
            .unwrap()
            .1
            .clone();
        assert!((r.leaked[Leak::Gone as usize] - 10.0).abs() < 1e-9);
        assert!((r.part - 1.0).abs() < 1e-9 && (r.local - 2.0).abs() < 1e-9);
        assert!(a.line().contains("leaked"));
        assert!(!a.table().is_empty());
    }

    #[test]
    fn vanished_instances_and_overkill() {
        let mut a = Audit::new();
        let kill = a.register("fullKill");
        a.enter();
        let _ = a.leave(kill, &m(&[(1, 100.0)]), &m(&[]));
        assert_eq!(a.rows()[0].1.vanished, 1);
        a.enter();
        let _ = a.leave(kill, &m(&[(2, 5.0)]), &m(&[(2, -20.0)]));
        // Both counts above 0: 5 (the caller clips the measurement).
        let c = a.measure(2, 5.0, true).unwrap();
        assert!(!c.mismatch());
        assert!((a.rows()[0].1.after_kill - 5.0).abs() < 1e-9);
    }
}

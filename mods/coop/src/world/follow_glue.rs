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

//! Client: references a copy's own code rewrites every frame (a Constructor
//! drone's `healingTarget`), which the host's change repairs cannot hold.
//!
//! The host's latest value of every instance reference is kept; a few copies
//! a frame are checked, and an (object, variable) found rewritten while the
//! host's target exists here is followed from then on: the host's value is
//! written at the run step on every copy of that object.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use ndml_net::msg::{NetId, StateValue};
use ndml_sdk::{Call, Value};

use super::super::state;
use super::WorldHooks;

/// Copies checked per frame for a rewritten reference.
const CHECKS_PER_FRAME: usize = 24;

/// One copy's references as the host last sent them.
#[derive(Debug)]
struct Refs {
    object: u16,
    /// Variable -> the host's target (`None`: `noone`).
    vars: Vec<(String, Option<NetId>)>,
}

#[derive(Debug, Default)]
pub(super) struct FollowState {
    copies: HashMap<NetId, Refs>,
    /// (object, variable) the copies' own code was seen to rewrite.
    followed: HashSet<(u16, String)>,
    /// Round robin over `copies` for the check.
    order: Vec<NetId>,
    cursor: usize,
}

/// An instance id as a number (refs or numbers), for comparing references.
fn id_number(v: &Value) -> Option<f64> {
    match v {
        Value::Ref(p) => Some(f64::from((*p & 0xFFFF_FFFF) as u32)),
        other => other.as_f64(),
    }
}

/// Keep the host's references a state carried in `kept`: an instance by its
/// net id; `noone` only for a variable already kept; any other value ends it.
fn record(kept: &mut Vec<(String, Option<NetId>)>, vars: &[(String, StateValue)]) {
    for (k, v) in vars {
        let value = match v.clone().expanded() {
            StateValue::Instance(n) => Some(n),
            StateValue::Real(n) if (n + 4.0).abs() < f64::EPSILON => None,
            _ => {
                kept.retain(|(n, _)| n != k);
                continue;
            }
        };
        match kept.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = value,
            None if value.is_some() => kept.push((k.clone(), value)),
            None => {}
        }
    }
}

impl WorldHooks {
    /// Client: a state was applied to the copy of `net`: keep the host's
    /// references it carried.
    pub(super) fn follow_applied(&self, net: NetId, object: u16, vars: &[(String, StateValue)]) {
        let mut st = self.lock();
        let f = &mut st.follow;
        let e = f.copies.entry(net).or_insert_with(|| Refs {
            object,
            vars: Vec::new(),
        });
        record(&mut e.vars, vars);
    }

    /// Client, at the run step (not in a freeze): check a few copies for a
    /// rewritten reference, and write the host's value on every followed one.
    pub(super) fn follow_frame(&self, call: &Call<'_>) {
        let Some(r) = self.h.reflect else { return };
        let (check, followed) = {
            let mut st = self.lock();
            let st = &mut *st;
            let client = &st.client;
            let f = &mut st.follow;
            f.copies.retain(|n, _| client.is_known(*n));
            if f.cursor >= f.order.len() {
                f.order = f.copies.keys().copied().collect();
                f.order.sort_unstable();
                f.cursor = 0;
            }
            let end = (f.cursor + CHECKS_PER_FRAME).min(f.order.len());
            let check: Vec<NetId> = f.order[f.cursor..end].to_vec();
            f.cursor = end;
            let followed: Vec<NetId> = if f.followed.is_empty() {
                Vec::new()
            } else {
                f.copies
                    .iter()
                    .filter(|(_, c)| {
                        c.vars
                            .iter()
                            .any(|(k, _)| f.followed.contains(&(c.object, k.clone())))
                    })
                    .map(|(n, _)| *n)
                    .collect()
            };
            (check, followed)
        };
        for (net, check_all) in check
            .iter()
            .map(|n| (*n, true))
            .chain(followed.iter().map(|n| (*n, false)))
        {
            let Some(id) = self.lock().client.id_of(net) else {
                continue;
            };
            if !self.exists(call, &id) {
                continue;
            }
            let (object, vars) = {
                let st = self.lock();
                let Some(c) = st.follow.copies.get(&net) else {
                    continue;
                };
                let vars: Vec<(String, Option<NetId>, bool)> = c
                    .vars
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            *v,
                            st.follow.followed.contains(&(c.object, k.clone())),
                        )
                    })
                    .collect();
                (c.object, vars)
            };
            for (var, host, is_followed) in vars {
                if !check_all && !is_followed {
                    continue;
                }
                // Only a target made here can be compared or written.
                let Some(want) = host.and_then(|n| self.lock().client.id_of(n)) else {
                    continue;
                };
                let have = state::get_var(call, &r, &id, &var);
                if have.as_ref().and_then(id_number) == id_number(&want) {
                    continue;
                }
                if !is_followed {
                    self.lock().follow.followed.insert((object, var.clone()));
                }
                let _ = state::set_var(call, &r, &id, &var, &want);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_references_are_kept_until_they_stop_being_references() {
        let mut kept = Vec::new();
        let v = |k: &str, x: StateValue| (k.to_owned(), x);
        // noone first: not a reference yet (a counter could be -4).
        record(
            &mut kept,
            &[
                v("healingTarget", StateValue::Whole(-4)),
                v("leader", StateValue::Instance(7)),
            ],
        );
        assert_eq!(kept, [("leader".to_owned(), Some(7))]);
        record(&mut kept, &[v("healingTarget", StateValue::Instance(9))]);
        record(&mut kept, &[v("leader", StateValue::Whole(-4))]);
        assert_eq!(
            kept,
            [
                ("leader".to_owned(), None),
                ("healingTarget".to_owned(), Some(9))
            ]
        );
        record(&mut kept, &[v("healingTarget", StateValue::Real(3.5))]);
        assert_eq!(kept, [("leader".to_owned(), None)]);
    }
}

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

//! The owner's side of the mirror's draw values ([`MirrorDraw::Edits`]). A
//! value goes when it changed since the key's previous tick, and all of a
//! key's values every [`FULL_EVERY`] ticks (spread over the keys), so a lost
//! change heals. A changed array goes as an [`ArrayEdit`] when that is
//! smaller; numbers go in their smallest exact encoding.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::mirror::{ArrayDelta, KeyVars, MirrorDraw, array_hash};
use crate::msg::{PeerId, StateValue};

/// A key's full set goes every this many of its ticks.
pub const FULL_EVERY: u32 = 30;

/// How far an edit may shift the array at its front.
const EDIT_MAX_SHIFT: usize = 16;

/// An array as an edit of the one the viewer holds: `head`, then the old
/// array from `drop` on, with `patches` applied, then `tail`. `check` is the
/// low byte of the old array's [`array_hash`]; a mismatch drops the slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArrayEdit {
    pub slot: u8,
    pub check: u8,
    /// Length of the new array.
    pub len: u16,
    pub drop: u16,
    pub head: Vec<StateValue>,
    pub patches: Vec<(u16, StateValue)>,
    pub tail: Vec<StateValue>,
}

/// The low byte of an array's hash.
fn check_of(items: &[StateValue]) -> u8 {
    array_hash(items).to_le_bytes()[0]
}

fn encoded_len<T: Serialize + ?Sized>(v: &T) -> usize {
    postcard::to_allocvec(v).map_or(usize::MAX, |b| b.len())
}

impl ArrayEdit {
    /// The edit from `old` to `new` with the fewest changes, when it encodes
    /// smaller than `new`.
    #[must_use]
    pub fn between(slot: u8, old: &[StateValue], new: &[StateValue]) -> Option<Self> {
        let max = i64::try_from(EDIT_MAX_SHIFT).unwrap_or(0);
        // Shift 0 first, then 1, -1, 2, -2, ...: on equal cost the smaller.
        let shifts = std::iter::once(0).chain((1..=max).flat_map(|s| [s, -s]));
        let mut best: Option<(usize, usize, usize)> = None;
        for shift in shifts {
            let (drop, head) = (
                usize::try_from(shift.max(0)).unwrap_or(0),
                usize::try_from((-shift).max(0)).unwrap_or(0),
            );
            if drop > old.len() || head > new.len() {
                continue;
            }
            let kept = (old.len() - drop).min(new.len() - head);
            let changed = (0..kept)
                .filter(|&i| old[drop + i] != new[head + i])
                .count();
            let cost = head + changed + (new.len() - head - kept);
            if best.is_none_or(|(c, _, _)| cost < c) {
                best = Some((cost, drop, head));
            }
        }
        let (_, drop, head) = best?;
        let kept = (old.len() - drop).min(new.len() - head);
        let edit = Self {
            slot,
            check: check_of(old),
            len: u16::try_from(new.len()).ok()?,
            drop: u16::try_from(drop).ok()?,
            head: new[..head].to_vec(),
            patches: (head..head + kept)
                .filter(|&i| old[drop + i - head] != new[i])
                .map(|i| Some((u16::try_from(i).ok()?, new[i].clone())))
                .collect::<Option<_>>()?,
            tail: new[head + kept..].to_vec(),
        };
        // The slot and the value's own tag against the edit's fields.
        (encoded_len(&edit) < encoded_len(new) + 2).then_some(edit)
    }

    /// The new array, if `old` is the one the edit was made from.
    #[must_use]
    pub fn apply(&self, old: &[StateValue]) -> Option<Vec<StateValue>> {
        let (len, drop) = (usize::from(self.len), usize::from(self.drop));
        if check_of(old) != self.check || drop > old.len() {
            return None;
        }
        let kept = len.checked_sub(self.head.len() + self.tail.len())?;
        let from = old.get(drop..drop + kept)?;
        let mut v = Vec::with_capacity(len);
        v.extend(self.head.iter().cloned());
        v.extend(from.iter().cloned());
        for (i, val) in &self.patches {
            *v.get_mut(usize::from(*i))? = val.clone();
        }
        v.extend(self.tail.iter().cloned());
        (v.len() == len).then_some(v)
    }
}

/// An [`ArrayDelta`] as an edit (its hash checked by the low byte).
impl From<ArrayDelta> for ArrayEdit {
    fn from(d: ArrayDelta) -> Self {
        Self {
            slot: d.slot,
            check: d.from_hash.to_le_bytes()[0],
            len: d.len,
            drop: d.drop,
            head: Vec::new(),
            patches: d.patches,
            tail: d.tail,
        }
    }
}

/// A draw-values message's (peer, owner time, keys with their time since
/// last sent and array changes as edits).
///
/// # Errors
/// Not draw values: the message itself.
pub fn values_of(m: MirrorDraw) -> Result<Keyed, MirrorDraw> {
    match m {
        MirrorDraw::Vars {
            peer,
            time_ms,
            keys,
            ..
        } => Ok((
            peer,
            time_ms,
            keys.into_iter()
                .map(|k| (None, k.map_deltas(ArrayEdit::from)))
                .collect(),
        )),
        MirrorDraw::Ticks {
            peer,
            time_ms,
            keys,
            ..
        } => Ok((
            peer,
            time_ms,
            keys.into_iter()
                .map(|(a, k)| (Some(a), k.map_deltas(ArrayEdit::from)))
                .collect(),
        )),
        MirrorDraw::Edits {
            peer,
            time_ms,
            keys,
            ..
        } => Ok((
            peer,
            time_ms,
            keys.into_iter().map(|(a, k)| (Some(a), k)).collect(),
        )),
        other => Err(other),
    }
}

/// [`values_of`]'s result: (peer, owner time, keys with their time since).
pub type Keyed = (PeerId, u32, Vec<(Option<u32>, KeyVars<ArrayEdit>)>);

/// The owner's draw values as sent: per key, its last values and send time.
#[derive(Debug, Default)]
pub struct DrawSender {
    tick: u32,
    last: HashMap<u16, Vec<Option<StateValue>>>,
    sent_at: HashMap<u16, u32>,
}

impl DrawSender {
    /// The messages for one tick at owner time `now_ms`: every live key's
    /// values in slot order (`None`: unreadable); missing keys are forgotten.
    pub fn tick(
        &mut self,
        peer: PeerId,
        now_ms: u32,
        read: Vec<(u16, Vec<Option<StateValue>>)>,
    ) -> Vec<MirrorDraw> {
        self.tick = self.tick.wrapping_add(1);
        let alive: std::collections::HashSet<u16> = read.iter().map(|(k, _)| *k).collect();
        self.last.retain(|k, _| alive.contains(k));
        self.sent_at.retain(|k, _| alive.contains(k));
        let mut keys = Vec::new();
        for (key, vals) in read {
            let vals: Vec<Option<StateValue>> = vals
                .into_iter()
                .map(|v| v.map(StateValue::compact))
                .collect();
            let full = self.tick.wrapping_add(u32::from(key)) % FULL_EVERY == 0;
            let last = self.last.entry(key).or_default();
            let mut changed = Vec::new();
            let mut edits = Vec::new();
            for (i, v) in vals.iter().enumerate() {
                let (Some(v), Ok(slot)) = (v, u8::try_from(i)) else {
                    continue;
                };
                let before = last.get(i).and_then(Option::as_ref);
                if !full && before == Some(v) {
                    continue;
                }
                let edit = match (full, before, v) {
                    (false, Some(StateValue::Array(old)), StateValue::Array(new)) => {
                        ArrayEdit::between(slot, old, new)
                    }
                    _ => None,
                };
                match edit {
                    Some(e) => edits.push(e),
                    None => changed.push((slot, v.clone())),
                }
            }
            *last = vals;
            if changed.is_empty() && edits.is_empty() {
                continue;
            }
            // How long since this key was last sent (0: never).
            let after = self
                .sent_at
                .insert(key, now_ms)
                .map_or(0, |t| now_ms.saturating_sub(t).max(1));
            keys.push((after, KeyVars::whole(key, changed, edits)));
        }
        let (parts, _) = MirrorDraw::split_edits(peer, now_ms, keys);
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::Message;

    fn reals(v: &[f64]) -> Vec<StateValue> {
        v.iter().map(|x| StateValue::Real(*x).compact()).collect()
    }

    #[test]
    fn an_array_grown_at_either_end_goes_as_its_edit() {
        // A trail kept newest first: a node in front, two nodes moved.
        let old = reals(&[155.0, 112.5, 67.5, 22.75, -21.5]);
        let new = reals(&[233.25, 195.125, 112.5, 67.5, 22.75, -20.0]);
        let e = ArrayEdit::between(2, &old, &new).unwrap();
        assert_eq!(
            (e.drop, e.head.len(), e.patches.len(), e.tail.len()),
            (0, 1, 2, 0)
        );
        assert_eq!(e.apply(&old).unwrap(), new);
        // Kept oldest first: the oldest gone, one appended, one moved.
        let old: Vec<StateValue> = (0..40)
            .map(|i| StateValue::Real(f64::from(i) * 1.37))
            .collect();
        let mut new: Vec<StateValue> = old[1..].to_vec();
        new.push(StateValue::Real(-3.3));
        new[20] = StateValue::Real(-1.1);
        let e = ArrayEdit::between(0, &old, &new).unwrap();
        assert_eq!(
            (e.drop, e.head.len(), e.patches.len(), e.tail.len()),
            (1, 0, 1, 1)
        );
        assert_eq!(e.apply(&old).unwrap(), new);
        // Not from the array held: refused.
        assert!(e.apply(&new).is_none());
        // Every element changed: the array goes whole.
        let other: Vec<StateValue> = (0..40)
            .map(|i| StateValue::Real(f64::from(i) + 0.1))
            .collect();
        assert!(ArrayEdit::between(0, &old, &other).is_none());
        // An `ArrayDelta` applies the same way.
        let d = ArrayDelta::between(0, &old, &new).unwrap();
        assert_eq!(ArrayEdit::from(d).apply(&old).unwrap(), new);
    }

    #[test]
    fn changed_values_go_compact_and_a_full_set_comes_round() {
        let mut s = DrawSender::default();
        let vals = |t: f64| {
            vec![
                Some(StateValue::Real(t)),
                Some(StateValue::Real(7.0)),
                Some(StateValue::Array(reals(&[t, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))),
            ]
        };
        let sent = |m: &[MirrorDraw]| -> Vec<(u32, KeyVars<ArrayEdit>)> {
            m.iter()
                .flat_map(|p| match p {
                    MirrorDraw::Edits { keys, .. } => keys.clone(),
                    _ => Vec::new(),
                })
                .collect()
        };
        // First tick: everything, compact.
        let first = sent(&s.tick(PeerId(1), 1016, vec![(5, vals(1.0))]));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, 0);
        assert_eq!(first[0].1.vals.len(), 3);
        assert_eq!(first[0].1.vals[1].1, StateValue::Whole(7));
        let mut fulls = 0;
        for t in 2..=(FULL_EVERY + 1) {
            let at = 1000 + 16 * t;
            let k = sent(&s.tick(PeerId(1), at, vec![(5, vals(f64::from(t) + 0.5))]));
            assert_eq!(k.len(), 1);
            assert_eq!(k[0].0, 16);
            if k[0].1.vals.len() == 3 {
                fulls += 1;
            } else {
                // The number that changed, and the array as its edit.
                assert_eq!(k[0].1.vals.len(), 1);
                assert_eq!(
                    k[0].1.vals[0].1,
                    StateValue::Real32(f64::from(t) as f32 + 0.5)
                );
                assert_eq!(k[0].1.deltas.len(), 1);
            }
        }
        assert_eq!(fulls, 1);
        // Nothing changed: nothing sent; a key gone and back starts whole.
        let same = vals(f64::from(FULL_EVERY + 1) + 0.5);
        let mut s2 = DrawSender::default();
        s2.tick(PeerId(1), 0, vec![(3, same.clone())]);
        assert!(sent(&s2.tick(PeerId(1), 16, vec![(3, same.clone())])).is_empty());
        s2.tick(PeerId(1), 32, Vec::new());
        let back = sent(&s2.tick(PeerId(1), 48, vec![(3, same)]));
        assert_eq!((back[0].0, back[0].1.vals.len()), (0, 3));
        for p in s.tick(PeerId(1), 5000, vec![(5, vals(9.0))]) {
            let m = Message::MirrorDraw(p);
            assert_eq!(Message::decode(&m.encode().unwrap()).unwrap(), m);
            assert_eq!(m.stream(), None);
        }
    }
}

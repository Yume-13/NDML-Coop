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

//! What enemies and shared shots chase when the host's `chooseTarget` picks
//! a player's thing (a construct, a mine, a decoy). The joining game's copy
//! is told the thing as a [`ThingRef`] and chases the proxy drawn for it.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ndml_net::shots::{AimChange, AimSubject, ThingRef};

/// Host: what each subject was last said to chase.
#[derive(Debug, Default)]
pub struct HostAims {
    sent: HashMap<AimSubject, ThingRef>,
    /// Changes to send.
    pub out: Vec<AimChange>,
}

impl HostAims {
    /// What `subject` chases now (`None`: no player's thing); queues and
    /// returns whether it changed.
    pub fn note(&mut self, subject: AimSubject, thing: Option<ThingRef>, now_ms: u32) -> bool {
        let before = self.sent.get(&subject).copied();
        if before == thing {
            return false;
        }
        if let Some(t) = thing {
            self.sent.insert(subject, t);
        } else {
            self.sent.remove(&subject);
        }
        self.out.push(AimChange {
            subject,
            time_ms: now_ms,
            thing,
        });
        true
    }

    /// A subject that is gone: nothing more to say about it.
    pub fn forget(&mut self, subject: AimSubject) {
        self.sent.remove(&subject);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Joining side: what each subject chases, on the host's clock.
#[derive(Debug, Default)]
pub struct ClientAims {
    /// Per subject: the latest change's time, the thing after it, the one before.
    by: HashMap<AimSubject, (u32, Option<ThingRef>, Option<ThingRef>)>,
}

impl ClientAims {
    /// A change from the host.
    pub fn change(&mut self, c: AimChange) {
        let e = self.by.entry(c.subject).or_insert((0, None, None));
        // Changes arrive in order (reliable); an older one is late news.
        if c.time_ms < e.0 {
            return;
        }
        *e = (c.time_ms, c.thing, e.1);
    }

    /// What `subject` chases at the shown time `shown_ms` (host clock).
    #[must_use]
    pub fn at(&self, subject: AimSubject, shown_ms: u32) -> Option<ThingRef> {
        let &(t, now, before) = self.by.get(&subject)?;
        if shown_ms >= t { now } else { before }
    }

    /// A subject that is gone here.
    pub fn forget(&mut self, subject: AimSubject) {
        self.by.remove(&subject);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::msg::PeerId;

    const T: ThingRef = ThingRef {
        owner: PeerId(0),
        key: 7,
    };

    #[test]
    fn the_host_says_each_change_once() {
        let mut h = HostAims::default();
        let s = AimSubject::Enemy(3);
        assert!(!h.note(s, None, 10), "nothing to nothing says nothing");
        assert!(h.note(s, Some(T), 20));
        assert!(!h.note(s, Some(T), 30));
        assert!(h.note(s, None, 40));
        assert_eq!(h.out.len(), 2);
        assert_eq!(h.out[1].thing, None);
    }

    #[test]
    fn the_joining_side_follows_the_shown_time() {
        let mut c = ClientAims::default();
        let s = AimSubject::Shot(9);
        assert_eq!(c.at(s, 100), None);
        c.change(AimChange {
            subject: s,
            time_ms: 200,
            thing: Some(T),
        });
        assert_eq!(c.at(s, 150), None, "not yet shown");
        assert_eq!(c.at(s, 200), Some(T));
        c.change(AimChange {
            subject: s,
            time_ms: 300,
            thing: None,
        });
        assert_eq!(c.at(s, 250), Some(T));
        assert_eq!(c.at(s, 300), None);
        c.change(AimChange {
            subject: s,
            time_ms: 250,
            thing: Some(T),
        });
        assert_eq!(c.at(s, 400), None, "an older change is late news");
    }
}

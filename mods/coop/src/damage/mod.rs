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

//! Shared damage. The joining game measures how much its copies' hull
//! dropped from causes on its player's side ([`cause`]; every other change
//! is put back) and reports the cumulative amount ([`ndml_net::damage`]);
//! the host subtracts it from the real enemy, whose own death path runs.
//!
//! * [`ClientDamage`]: the measurement, the reports and kill prediction: a
//!   copy shown at 0 dies by its own code or the host's word, never by the mod.
//!   The host answers every claim within [`REFUSE_AFTER_FRAMES`] (`Despawn`
//!   or `KillRefused`); a copy refused at 0 is not claimed again until the
//!   host's hull rises.
//! * [`HostDamage`]: the host's ledger and kill verdicts.
//! * [`audit`]: an independent count of every hull write (off).
//! * [`glue`]: hooks and game calls.

#![forbid(unsafe_code)]

pub mod audit;
pub mod cause;
pub mod fx;
pub mod glue;
pub mod status_cause;

use std::collections::HashMap;

use ndml_net::damage::{EPS, Ledger, Reporter};
use ndml_net::msg::{DamageReport, DespawnCause, Message, NetId, PeerId, hull_frac_carry};

/// A copy at `hull <= 0` for this many frames (the host's with hull) is
/// "stuck" on the client.
pub const STUCK_FRAMES: u64 = 30;
/// The host's enemy that a peer's damage took to `hull <= 0` and that sits
/// there this many running frames, neither dead nor refused, is "stuck".
pub const HOST_STUCK_FRAMES: u64 = REFUSE_AFTER_FRAMES + STUCK_FRAMES;
/// The host refuses a confirmed claim when its enemy still lives this many
/// running frames after the apply that took it to 0 (2.5 s: longer than any
/// staggered death that ends on its own).
pub const REFUSE_AFTER_FRAMES: u64 = 150;
/// A copy dead here (or the host's enemy dead there) longer than this is
/// "dead on one side": the bound plus two trips at 120 ms with loss.
pub const ONE_SIDE_FRAMES: u64 = REFUSE_AFTER_FRAMES + 45;
/// A kill claim with no answer within this many frames is given up.
pub const ANSWER_FRAMES: u64 = 240;
/// A copy the host has despawned that is still here this many frames later
/// is removed.
pub const GHOST_FRAMES: u64 = 60;

/// How long a despawn of an enemy with no copy here is kept for a copy
/// still to be made (within the render delay, at most 500 ms).
const UNMADE_KEEP_FRAMES: u64 = 600;

/// After a resync, how many running frames its `EnemySpawn`s may take
/// before what they did not name is taken as gone.
const RESYNC_WAIT_FRAMES: u64 = 120;

/// A copy held back after a resync stays held this many frames after the
/// host confirmed its kill (past the world's `DIE_WAIT_FRAMES`, 120).
const HELD_AFTER_CONFIRM_FRAMES: u64 = 180;

/// A refused claim's copy that leaves this many frames or less after the
/// refusal is the refusal's old instance.
const REFUSED_LEFT_FRAMES: u64 = 300;

/// How long the host's word that an enemy is gone is kept with no live copy
/// of it here (a copy can still be made after its despawn).
const HOST_GONE_KEEP_FRAMES: u64 = 300;

/// Whether the audit keeps the host's word that an enemy is gone (it came
/// at frame `at`): while a copy of it is live here, and for
/// [`HOST_GONE_KEEP_FRAMES`] in any case.
#[must_use]
pub(crate) fn host_gone_kept(live: bool, at: u64, frame: u64) -> bool {
    live || frame.saturating_sub(at) < HOST_GONE_KEEP_FRAMES
}

/// What a resync still waits for (see [`ClientDamage`]'s resync).
#[derive(Debug, Default)]
struct ResyncWait {
    /// Net ids kept here (a total, a claim) that no spawn of it named yet.
    left: std::collections::HashSet<NetId>,
    /// Its spawns still to come.
    spawns: u16,
    /// The frame it gives up waiting.
    until: u64,
}

/// The shared damage switches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DamageSettings {
    /// The independent count (client; off).
    pub audit: bool,
}

impl DamageSettings {
    /// The kill-effects counts (contested kills, [`fx::HostKills`]): off.
    #[must_use]
    pub const fn kill_fx_on() -> bool {
        false
    }

    /// The status channel ([`ndml_net::status`]): off.
    #[must_use]
    pub const fn status_on() -> bool {
        false
    }
}

/// What the client's tick writes to a copy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickOut {
    /// Hull (and `hullPrevious`) to write.
    pub write: f32,
    /// Hull this copy lost to local causes since the last write.
    pub drop: f32,
    /// The copy is to die here (its death path is let through).
    pub dying: bool,
    /// The host despawned the enemy and the copy is still here
    /// [`GHOST_FRAMES`] later: remove it (the host's word).
    pub remove: bool,
    /// The drop came after the enemy was already dead (dying here before
    /// this tick, or despawned by the host): overkill, not reported.
    pub overkill: bool,
}

/// Client counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClientCounts {
    /// Measurement events (frames where a copy lost hull) and their sum.
    pub drops: u64,
    pub reported: f64,
    /// Copies of an object that regains hull whose view reached 0: not
    /// predicted dead here, left to the host's word.
    pub heal_held: u64,
    /// Copies whose view reached 0 here.
    pub claims: u64,
    /// Of those, copies that died here through the game's path.
    pub deaths: u64,
    /// The host confirmed (`Despawn` of a copy that died here, killer this peer).
    pub confirmed: u64,
    /// The host's `Despawn` named someone else for a copy that died here.
    pub confirmed_other: u64,
    /// The host refused a claim (`KillRefused`), and copies it sent back.
    pub refused: u64,
    pub respawned: u64,
    /// The host killed an enemy damaged here but not claimed.
    pub host_kills_damaged: u64,
    /// Frames from the claim (the killing hit's measurement) to the death:
    /// 0, 1, 2, 3 or more.
    pub lag: [u64; 4],
    /// Copies alive at `hull <= 0` for [`STUCK_FRAMES`] while the host's
    /// own instance still had hull.
    pub stuck: u64,
    /// Claims the host never answered within [`ANSWER_FRAMES`].
    pub unanswered: u64,
    /// Despawned copies still here after [`GHOST_FRAMES`] (removed then).
    pub ghosts: u64,
    /// Claims whose copy was still alive when the host's despawn came.
    pub overtaken: u64,
    /// Claims refused while the copy was still alive here (its own code
    /// keeps it at 0; the host's word: it lives).
    pub refused_alive: u64,
    /// Claims answered (or given up) more than [`ONE_SIDE_FRAMES`] after the
    /// copy died here: dead on this side only for longer than the bound.
    pub one_side: u64,
    /// How the claims on copies that died here left the waiting table.
    pub left: DeadLeft,
    /// Of `host_kills_damaged`, the despawns that named this peer as the killer.
    pub host_kills_ours: u64,
    /// How a "no claim until the hull rises" mark (a refusal at 0) ended.
    pub no_claim_ended: NoClaimEnds,
}

/// The ways a "no claim until the host's hull rises above 0" mark ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoClaimEnds {
    /// A tick brought the host's hull above 0.
    pub hull_rise: u64,
    /// The host despawned the enemy.
    pub despawn: u64,
    /// A resync.
    pub resync: u64,
    /// The run's end or a role change.
    pub cleared: u64,
}

/// Which way a no-claim mark ended (see [`NoClaimEnds`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoClaimEnd {
    HullRise,
    Despawn,
    Resync,
    Cleared,
}

/// The ways a claim on a copy that died here leaves the table of claims
/// waiting for the host's word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeadLeft {
    /// The host's despawn named this peer.
    pub confirmed: u64,
    /// The host's despawn named someone else, or no one.
    pub confirmed_other: u64,
    /// The host refused it (the copy is sent again).
    pub refused: u64,
    /// The host sent the enemy again with no refusal first.
    pub respawned: u64,
    /// A resync's spawns did not name it (gone on the host in the gap).
    pub resync_gone: u64,
    /// No word within [`ANSWER_FRAMES`].
    pub given_up: u64,
    /// Forgotten at a run's end or a role change.
    pub cleared: u64,
    /// A second death here of the same net id replaced it.
    pub overwritten: u64,
}

/// Which way a claim left (see [`DeadLeft`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Left {
    Confirmed,
    ConfirmedOther,
    Refused,
    Respawned,
    ResyncGone,
    GivenUp,
    Cleared,
    Overwritten,
}

/// Frames from a claim to its end, by who ended it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimTimes {
    /// The copy's own code killed it here.
    pub own: Vec<u32>,
    /// The host's word: its despawn or its refusal.
    pub word: Vec<u32>,
    /// A copy that died here and was refused: frames from its death here to
    /// the copy made again (the player sees an enemy it killed come back).
    pub remade: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
struct Dying {
    key: usize,
    since: u64,
}

/// The client's side: see the module docs.
#[derive(Debug, Default)]
pub struct ClientDamage {
    rep: Reporter,
    /// What the last tick wrote (the measurement baseline).
    written: HashMap<NetId, f32>,
    dying: HashMap<NetId, Dying>,
    /// Died here, waiting for the host's word (frame of death).
    dead: HashMap<NetId, u64>,
    /// Copies at `hull <= 0` without a claim: since which frame.
    zero_since: HashMap<NetId, u64>,
    /// Enemies the host has despawned whose copies are still here (timeline
    /// deaths): measured, not reported (overkill).
    host_dead: HashMap<NetId, u64>,
    /// The host's despawns of enemies with no copy here yet (frame): a copy
    /// made after its despawn starts host-dead and is never claimed.
    despawned_unmade: HashMap<NetId, u64>,
    /// Claims the host refused on a copy that died here, until the enemy
    /// is sent again: the frame it died here.
    refused: HashMap<NetId, u64>,
    /// Every refusal of the latest claim, dying here or dead here: the
    /// frame it came, for [`REFUSED_LEFT_FRAMES`] ([`Self::refused_lately`]).
    refused_at: HashMap<NetId, u64>,
    /// Refused at 0 (the host keeps it alive at 0): no claim until the
    /// host's hull rises above 0. The frame of the refusal.
    no_claim: HashMap<NetId, u64>,
    /// After a resync, until its spawns are in: see [`Self::resync`].
    resync_wait: Option<ResyncWait>,
    /// Copies whose view reached 0 held for the host's word (`host_word`;
    /// counted once each).
    heal_held: std::collections::HashSet<NetId>,
    /// Enemies whose claim the host refused above 0: their next kill shows
    /// at the host's word, never predicted here again, so one enemy cannot
    /// die twice on the joining screen.
    host_word: std::collections::HashSet<NetId>,
    /// Kills claimed here before a resync that sent the enemy again: no copy
    /// while the claim is open; `Some(frame)`: confirmed, held until then.
    held: HashMap<NetId, Option<u64>>,
    frame: u64,
    counts: ClientCounts,
    times: ClaimTimes,
}

impl ClientDamage {
    /// A fresh client side.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a frame. Returns the claims given up this frame (dead here or
    /// still dying here: either way the host never answered).
    pub fn begin_frame(&mut self) -> Vec<NetId> {
        self.frame += 1;
        let frame = self.frame;
        self.despawned_unmade
            .retain(|_, at| frame.saturating_sub(*at) < UNMADE_KEEP_FRAMES);
        self.held.retain(|_, until| until.is_none_or(|u| frame < u));
        self.refused_at
            .retain(|_, at| frame.saturating_sub(*at) < REFUSED_LEFT_FRAMES);
        if self.resync_wait.as_ref().is_some_and(|w| frame >= w.until) {
            self.finish_resync();
        }
        let mut late: Vec<NetId> = self
            .dead
            .iter()
            .filter(|(_, at)| frame.saturating_sub(**at) >= ANSWER_FRAMES)
            .map(|(&n, _)| n)
            .collect();
        for n in &late {
            if self.dead.remove(n).is_some() {
                self.left_dead(Left::GivenUp);
            }
            self.counts.one_side += 1;
        }
        let dying: Vec<NetId> = self
            .dying
            .iter()
            .filter(|(_, d)| frame.saturating_sub(d.since) >= ANSWER_FRAMES)
            .map(|(&n, _)| n)
            .collect();
        for n in &dying {
            // Back to the host's view; its claim is over.
            self.dying.remove(n);
            self.rep.unclaim(*n);
        }
        late.extend(dying);
        for n in &late {
            self.rep.forget(*n);
            self.held.remove(n);
            self.counts.unanswered += 1;
        }
        late
    }

    /// The host sent its world again (`WorldResync`, `enemies` spawns to
    /// follow). Totals and open claims stay (the host's ledger keeps what it
    /// applied) and claims are sent again; what the spawns do not name is
    /// forgotten ([`Self::finish_resync`]).
    fn resync(&mut self, enemies: u16) {
        let frame = self.frame;
        for (n, _) in std::mem::take(&mut self.dying) {
            if self.dead.insert(n, frame).is_some() {
                self.left_dead(Left::Overwritten);
            }
        }
        for at in self.dead.values_mut() {
            *at = frame;
        }
        self.rep.resync();
        self.host_dead.clear();
        self.refused.clear();
        self.refused_at.clear();
        self.end_no_claims(NoClaimEnd::Resync);
        self.written.clear();
        self.zero_since.clear();
        self.held.clear();
        let mut left: std::collections::HashSet<NetId> = self.rep.net_ids().into_iter().collect();
        left.extend(self.dead.keys().copied());
        self.resync_wait = Some(ResyncWait {
            left,
            spawns: enemies,
            until: frame + RESYNC_WAIT_FRAMES,
        });
        if enemies == 0 {
            self.finish_resync();
        }
    }

    /// The resync's spawns are in (or the wait ran out): what they did not
    /// name is gone on the host; its totals go and a claim on it is settled.
    fn finish_resync(&mut self) {
        let Some(w) = self.resync_wait.take() else {
            return;
        };
        for n in w.left {
            self.rep.forget(n);
            self.held.remove(&n);
            if let Some(at) = self.dead.remove(&n) {
                self.answered_dead(at);
                self.counts.confirmed_other += 1;
                self.left_dead(Left::ResyncGone);
            }
        }
    }

    /// Enemies whose copy must not be made now: kills claimed here before a
    /// resync that sent the enemy again (claim open, or confirmed lately).
    #[must_use]
    pub fn held_spawns(&self) -> Vec<NetId> {
        self.held.keys().copied().collect()
    }

    /// A claim on `net_id` ended `frames` after it by the copy's own code
    /// (`own`) or the host's word.
    fn ended(&mut self, frames: u64, own: bool) {
        let f = u32::try_from(frames).unwrap_or(u32::MAX);
        if own {
            self.times.own.push(f);
        } else {
            self.times.word.push(f);
        }
    }

    /// A claim on a copy dead here left the waiting table `how`: counted.
    fn left_dead(&mut self, how: Left) {
        let l = &mut self.counts.left;
        let c = match how {
            Left::Confirmed => &mut l.confirmed,
            Left::ConfirmedOther => &mut l.confirmed_other,
            Left::Refused => &mut l.refused,
            Left::Respawned => &mut l.respawned,
            Left::ResyncGone => &mut l.resync_gone,
            Left::GivenUp => &mut l.given_up,
            Left::Cleared => &mut l.cleared,
            Left::Overwritten => &mut l.overwritten,
        };
        *c += 1;
    }

    /// The no-claim mark on `net_id` (if any) ends `how`: counted.
    fn end_no_claim(&mut self, net_id: NetId, how: NoClaimEnd) {
        if self.no_claim.remove(&net_id).is_none() {
            return;
        }
        let e = &mut self.counts.no_claim_ended;
        *match how {
            NoClaimEnd::HullRise => &mut e.hull_rise,
            NoClaimEnd::Despawn => &mut e.despawn,
            NoClaimEnd::Resync => &mut e.resync,
            NoClaimEnd::Cleared => &mut e.cleared,
        } += 1;
    }

    /// The host killed an enemy damaged here but not claimed: counted, and
    /// apart when the despawn names this peer (credited, never claimed).
    fn host_kill_of_damaged(&mut self, cause: DespawnCause, me: PeerId) {
        self.counts.host_kills_damaged += 1;
        let ours = matches!(cause, DespawnCause::Killed { by: Some(p) } if p == me);
        self.counts.host_kills_ours += u64::from(ours);
    }

    /// Every no-claim mark ends `how` (a resync, the run's end).
    fn end_no_claims(&mut self, how: NoClaimEnd) {
        let nets: Vec<NetId> = self.no_claim.keys().copied().collect();
        for n in nets {
            self.end_no_claim(n, how);
        }
    }

    /// The copy died here at `at`: answered now; dead on one side too long?
    fn answered_dead(&mut self, at: u64) {
        if self.frame.saturating_sub(at) > ONE_SIDE_FRAMES {
            self.counts.one_side += 1;
        }
    }

    /// A copy's End Step tick: `host_hull` from the host, `now_hull` read
    /// from the copy before any write this frame.
    pub fn tick(
        &mut self,
        net_id: NetId,
        key: usize,
        host_hull: f32,
        now_hull: Option<f32>,
    ) -> TickOut {
        let mut drop = 0.0;
        if let (Some(&w), Some(h)) = (self.written.get(&net_id), now_hull)
            && h.is_finite()
            && h < w - EPS
        {
            drop = w - h;
            if self.host_dead.contains_key(&net_id) {
                // The host's enemy is already gone: overkill on a copy that
                // is waiting for its timeline death.
                self.written.insert(net_id, h);
                return self.host_dead_out(net_id, h, drop);
            }
            self.rep.add(net_id, drop, host_hull);
            self.counts.drops += 1;
            self.counts.reported += f64::from(drop);
        }
        if self.host_dead.contains_key(&net_id) {
            let write = now_hull.unwrap_or(host_hull);
            self.written.insert(net_id, write);
            return self.host_dead_out(net_id, write, drop);
        }
        if host_hull > 0.0 {
            self.end_no_claim(net_id, NoClaimEnd::HullRise);
        }
        let view = self.rep.view(net_id, host_hull);
        if self.dying.contains_key(&net_id) {
            // Never raise a dying copy's hull again; it dies by its own code
            // or the host's word (never by the mod).
            let write = now_hull.unwrap_or(view).min(view).min(0.0);
            self.written.insert(net_id, write);
            return TickOut {
                write,
                drop,
                dying: true,
                remove: false,
                overkill: true,
            };
        }
        let predicted =
            view <= 0.0 && self.rep.unacked(net_id) > EPS && !self.no_claim.contains_key(&net_id);
        if predicted && self.host_word.contains(&net_id) {
            // An enemy whose claim the host refused above 0 (`host_word`):
            // it dies by the host's word, one round trip later, the copy
            // keeping the host's hull meanwhile; shown dead here once, never
            // twice.
            if self.heal_held.insert(net_id) {
                self.counts.heal_held += 1;
            }
            let write = host_hull.max(view);
            self.written.insert(net_id, write);
            self.zero_since.remove(&net_id);
            return TickOut {
                write,
                drop,
                dying: false,
                remove: false,
                overkill: false,
            };
        }
        let dying = predicted;
        if dying {
            self.dying.insert(
                net_id,
                Dying {
                    key,
                    since: self.frame,
                },
            );
            self.rep.claim_kill(net_id);
            self.counts.claims += 1;
            self.zero_since.remove(&net_id);
        } else if view <= 0.0 && host_hull > 0.0 {
            let since = *self.zero_since.entry(net_id).or_insert(self.frame);
            if self.frame - since == STUCK_FRAMES {
                self.counts.stuck += 1;
            }
        } else {
            self.zero_since.remove(&net_id);
        }
        self.written.insert(net_id, view);
        TickOut {
            write: view,
            drop,
            dying,
            remove: false,
            overkill: false,
        }
    }

    /// A despawn ends the enemy's holds for the host's word.
    fn note_despawn(&mut self, msg: &Message) {
        if let Message::Despawn { net_id, .. } = msg {
            self.heal_held.remove(net_id);
            self.host_word.remove(net_id);
        }
    }

    fn host_dead_out(&mut self, net_id: NetId, write: f32, drop: f32) -> TickOut {
        let since = self.host_dead.get(&net_id).copied().unwrap_or(self.frame);
        let age = self.frame.saturating_sub(since);
        let ghost = age >= GHOST_FRAMES;
        if age == GHOST_FRAMES {
            self.counts.ghosts += 1;
        }
        TickOut {
            write,
            drop,
            dying: true,
            remove: ghost,
            overkill: true,
        }
    }

    /// What the last tick wrote to the copy of `net_id` (the measurement's
    /// baseline).
    #[must_use]
    pub fn written(&self, net_id: NetId) -> Option<f32> {
        self.written.get(&net_id).copied()
    }

    /// A copy was just created with `hull` written: the baseline for its
    /// first measurement.
    pub fn baseline(&mut self, net_id: NetId, hull: f32) {
        self.written.insert(net_id, hull);
        if self.despawned_unmade.remove(&net_id).is_some() {
            self.host_dead.insert(net_id, self.frame);
        }
    }

    /// Whether the copy at `key` is dying here (its death path runs).
    #[must_use]
    pub fn is_dying(&self, key: usize) -> bool {
        self.dying.values().any(|d| d.key == key)
    }

    /// Whether the host refused this peer's claim on `net_id` in the last
    /// `REFUSED_LEFT_FRAMES`: a copy of it leaving now is the refusal's
    /// old instance.
    #[must_use]
    pub fn refused_lately(&self, net_id: NetId) -> bool {
        self.refused_at.contains_key(&net_id)
    }

    /// Net ids whose kill claim is not settled yet (claimed and unanswered,
    /// or refused and not sent again): their copy may come back.
    #[must_use]
    pub fn open_claims(&self) -> Vec<NetId> {
        self.dead
            .keys()
            .chain(self.refused.keys())
            .copied()
            .collect()
    }

    /// The copy of `net_id` is gone from the game. Returns whether it was
    /// dying here.
    pub fn gone(&mut self, net_id: NetId) -> bool {
        self.written.remove(&net_id);
        self.zero_since.remove(&net_id);
        self.host_dead.remove(&net_id);
        self.heal_held.remove(&net_id);
        let Some(d) = self.dying.remove(&net_id) else {
            return false;
        };
        let lag = self.frame.saturating_sub(d.since);
        self.counts.lag[usize::try_from(lag.min(3)).unwrap_or(3)] += 1;
        self.counts.deaths += 1;
        self.ended(lag, true);
        if self.dead.insert(net_id, self.frame).is_some() {
            self.left_dead(Left::Overwritten);
        }
        true
    }

    /// Net id of the dying copy at `key`.
    #[must_use]
    pub fn dying_net_id(&self, key: usize) -> Option<NetId> {
        self.dying
            .iter()
            .find_map(|(&n, d)| (d.key == key).then_some(n))
    }

    /// A host message about damage or enemy lifetimes.
    pub fn on_message(&mut self, me: PeerId, msg: &Message) {
        self.note_despawn(msg);
        match msg {
            Message::DamageAck(a) => self.rep.on_ack(me, a),
            // A refusal names its claim; one of an older claim than the
            // latest on the enemy is old news.
            Message::KillRefused {
                net_id,
                peer,
                hull,
                claim,
            } if *peer == me && *claim == self.rep.last_claim(*net_id) => {
                self.counts.refused += 1;
                self.refused_at.insert(*net_id, self.frame);
                // Held since a resync: the host's word is that it lives.
                self.held.remove(net_id);
                if let Some(at) = self.dead.remove(net_id) {
                    // Died here: the host sends it again.
                    self.answered_dead(at);
                    self.refused.insert(*net_id, at);
                    self.left_dead(Left::Refused);
                }
                let alive_here = if let Some(d) = self.dying.remove(net_id) {
                    // Still alive here (its own code keeps it at 0, as the
                    // host's): it stays; nothing to send again.
                    self.counts.refused_alive += 1;
                    let f = self.frame.saturating_sub(d.since);
                    self.ended(f, false);
                    true
                } else {
                    false
                };
                self.after_refusal(*net_id, *hull);
                // The host keeps what it applied: the total stays, the claim
                // is over.
                self.rep.unclaim(*net_id);
                // A copy still alive here keeps its baseline, so hits since
                // the last tick are still measured.
                if !alive_here {
                    self.written.remove(net_id);
                }
            }
            Message::EnemySpawn(s) => {
                self.host_dead.remove(&s.net_id);
                self.despawned_unmade.remove(&s.net_id);
                let mut of_resync = false;
                if let Some(w) = &mut self.resync_wait {
                    of_resync = true;
                    w.left.remove(&s.net_id);
                    w.spawns = w.spawns.saturating_sub(1);
                }
                if of_resync && self.dead.contains_key(&s.net_id) {
                    // The host still has an enemy claimed here: the claim
                    // (sent again) stays open, and holds its copy.
                    self.held.insert(s.net_id, None);
                } else if let Some(at) = self.refused.remove(&s.net_id) {
                    self.counts.respawned += 1;
                    let f = u32::try_from(self.frame.saturating_sub(at)).unwrap_or(u32::MAX);
                    self.times.remade.push(f);
                } else if self.dead.remove(&s.net_id).is_some() {
                    self.counts.respawned += 1;
                    self.left_dead(Left::Respawned);
                }
                if self.resync_wait.as_ref().is_some_and(|w| w.spawns == 0) {
                    self.finish_resync();
                }
            }
            Message::Despawn { net_id, cause } => {
                if let Some(h) = self.held.get_mut(net_id) {
                    // Confirmed (or gone): still no copy, until the world has
                    // let go of the spawn it would make to die at once.
                    *h = Some(self.frame + HELD_AFTER_CONFIRM_FRAMES);
                }
                if let Some(at) = self.dead.remove(net_id) {
                    self.answered_dead(at);
                    match cause {
                        DespawnCause::Killed { by: Some(p) } if *p == me => {
                            self.counts.confirmed += 1;
                            self.left_dead(Left::Confirmed);
                        }
                        _ => {
                            self.counts.confirmed_other += 1;
                            self.left_dead(Left::ConfirmedOther);
                        }
                    }
                } else if self.rep.total(*net_id) > EPS
                    && matches!(cause, DespawnCause::Killed { .. })
                {
                    self.host_kill_of_damaged(*cause, me);
                }
                self.rep.forget(*net_id);
                self.end_no_claim(*net_id, NoClaimEnd::Despawn);
                if let Some(d) = self.dying.remove(net_id) {
                    // Claimed here, but the host's despawn came first.
                    let f = self.frame.saturating_sub(d.since);
                    self.ended(f, false);
                    self.counts.overtaken += 1;
                    if self.counts.overtaken <= 20 {
                        log::info!(
                            "shared damage: claimed {net_id} still alive here when the host's despawn ({cause:?}) came, {} frames after the claim",
                            self.frame.saturating_sub(d.since)
                        );
                    }
                }
                self.zero_since.remove(net_id);
                // The copy may stay until its timeline death: keep measuring
                // (as overkill) until it leaves.
                if self.written.contains_key(net_id) {
                    self.host_dead.insert(*net_id, self.frame);
                } else {
                    // No copy yet: it may still be made (a timeline birth).
                    self.despawned_unmade.insert(*net_id, self.frame);
                }
            }
            Message::WorldResync { enemies } => self.resync(*enemies),
            _ => {}
        }
    }

    /// Reports due this frame.
    pub fn take_reports(&mut self, me: PeerId) -> Vec<Message> {
        self.rep.take_reports(me, self.frame)
    }

    /// Counters so far.
    #[must_use]
    pub fn counts(&self) -> ClientCounts {
        self.counts
    }

    /// Frames from claims to their end so far.
    #[must_use]
    pub fn times(&self) -> &ClaimTimes {
        &self.times
    }

    /// Kills claimed here the host has not answered yet.
    #[must_use]
    pub fn awaiting(&self) -> usize {
        self.dead.len()
    }

    /// Forget every enemy (run end, a role change); counters stay. A resync
    /// keeps totals and claims (`Self::resync`).
    pub fn clear(&mut self) {
        self.rep.clear();
        self.host_dead.clear();
        self.refused.clear();
        self.refused_at.clear();
        self.end_no_claims(NoClaimEnd::Cleared);
        self.written.clear();
        self.dying.clear();
        for _ in std::mem::take(&mut self.dead) {
            self.left_dead(Left::Cleared);
        }
        self.zero_since.clear();
        self.resync_wait = None;
        self.held.clear();
        self.heal_held.clear();
        self.host_word.clear();
    }

    /// After a refusal: one at 0 is not claimed again until the host's hull
    /// rises; one above 0 dies here only at the host's word (`host_word`).
    fn after_refusal(&mut self, net_id: NetId, hull: f32) {
        if hull <= 0.0 {
            self.no_claim.insert(net_id, self.frame);
        } else {
            self.host_word.insert(net_id);
        }
    }
}

/// Reports held back folded to one entry per reporter and enemy (totals are
/// cumulative: the largest wins, and any claim stands).
#[must_use]
pub fn compact(reports: Vec<DamageReport>) -> Vec<DamageReport> {
    let mut by: Vec<(PeerId, Vec<ndml_net::msg::DamageEntry>)> = Vec::new();
    for r in reports {
        let at = if let Some(i) = by.iter().position(|(p, _)| *p == r.by) {
            i
        } else {
            by.push((r.by, Vec::new()));
            by.len() - 1
        };
        let list = &mut by[at].1;
        for e in r.entries {
            match list.iter_mut().find(|x| x.net_id == e.net_id) {
                Some(x) => {
                    x.total = x.total.max(e.total);
                    x.claim = x.claim.max(e.claim);
                }
                None => list.push(e),
            }
        }
    }
    by.into_iter()
        .map(|(by, entries)| DamageReport { by, entries })
        .collect()
}

/// One report entry for the host's glue to apply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Apply {
    pub net_id: NetId,
    pub by: PeerId,
    pub delta: f32,
    /// The number of a claim the host has not judged yet (0: none).
    pub claim: u16,
}

/// The host's verdict on a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Hull is at or below 0: the game's own path kills it.
    Confirm,
    /// Hull left: refuse, send the copy back.
    Refuse,
}

/// Host counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostCounts {
    /// Report entries with something new, and the hull applied.
    pub applies: u64,
    pub applied: f64,
    /// Applies whose read-back differed from the amount by more than 0.01.
    pub mismatched: u64,
    /// Applies to an enemy that no longer exists here.
    pub missing: u64,
    pub claims: u64,
    pub refused: u64,
    /// Enemies a peer damaged that died here whose killer was a peer (its
    /// damage took the hull to 0).
    pub deaths_by_peer: u64,
    /// For deaths by a peer: Σ applied / Σ max hull (1.0 = the reports alone
    /// used up exactly the enemy's hull; >1 overkill).
    pub peer_applied_at_death: f64,
    pub peer_hull_max_at_death: f64,
    /// Enemies a peer's damage took to `hull <= 0` that sat there for
    /// [`HOST_STUCK_FRAMES`], neither dead nor refused at 0.
    pub stuck: u64,
    /// Confirmed claims refused after [`REFUSE_AFTER_FRAMES`] because the
    /// enemy still lived (the claim rule).
    pub refused_late: u64,
    /// New claims (a higher claim number) on an enemy refused at 0 with no
    /// hull rise since.
    pub reclaims: u64,
    /// Refused enemies that died within 2 s of the refusal (a copy sent
    /// again only to die again).
    pub refused_then_killed: u64,
    /// Claims whose enemy the reports left above 0 by no more than the
    /// carrying error ([`HostDamage::carried_short`]): written to 0, kills.
    pub carried: u64,
    /// Refusals with the enemy above 0: by more than a crumb (the claimant
    /// saw less hull than there was), and by under [`CRUMB_REST`] (only
    /// rounding leaves that; none once [`HostDamage::carried_short`] holds).
    pub refused_more: u64,
    pub refused_crumb: u64,
    /// Claims honoured past a repair the claimer could not have seen
    /// ([`HostDamage::honour`]): the hull the repairs left, written to 0
    /// (the lost repair, its own class in the read-back, never a mismatch).
    pub honoured: u64,
    pub lost_repair: f64,
    /// Repairs the host saw (`recordHealing` on an enemy): those the rule
    /// counts, and those it never does ([`repair_counts`]).
    pub repairs_counted: u64,
    pub repairs_uncounted: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct HostAcct {
    applied: f64,
    hull_max: f32,
    zero_since: Option<u64>,
    /// A peer's apply took the hull from above 0 to 0 or below (cleared
    /// when the hull rises above 0 again).
    peer_zeroed: bool,
}

/// An enemy the host's watch has just counted stuck (logged by its net).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stuck {
    pub net_id: NetId,
    pub object: u16,
    pub hull: f32,
    /// Peer damage applied to it so far.
    pub applied: f64,
}

/// The host's side: see the module docs.
#[derive(Debug, Default)]
pub struct HostDamage {
    ledger: Ledger,
    accts: HashMap<NetId, HostAcct>,
    /// Confirmed claims whose enemy has not died yet: claimant, frame,
    /// claim number.
    pending: HashMap<NetId, (PeerId, u64, u16)>,
    /// Refused at `hull <= 0`, no rise above 0 since.
    refused_zero: std::collections::HashSet<NetId>,
    /// Refusals: frame and object, for "refused then killed".
    refused_at: HashMap<NetId, (u64, u16)>,
    /// Object of each enemy a report touched.
    objects: HashMap<NetId, u16>,
    /// "Refused then killed within 2 s", by object.
    refused_killed_by: HashMap<u16, u64>,
    /// Refusals at `hull <= 0` (alive at 0 by the game's own code), by object.
    refused_zero_by: HashMap<u16, u64>,
    /// The frame of each enemy's first tick here (its set-up runs then).
    born: HashMap<NetId, u64>,
    /// Repairs on each enemy in the last [`HONOUR_CAP_FRAMES`].
    repairs: HashMap<NetId, Vec<Repair>>,
    /// An honoured claim's rest, for its read-back ([`Self::applied`]).
    honoured_rest: HashMap<NetId, f64>,
    frame: u64,
    counts: HostCounts,
}

/// A repair the host saw: frame, amount, whether the rule counts it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Repair {
    frame: u64,
    amount: f64,
    counted: bool,
}

/// The honour rule: a claim that would be refused above 0 is honoured when
/// the hull left is no more than the host's repairs on that enemy within the
/// claimer's round trip ([`honour_window`]), plus the carrying error. The
/// window is never more than this many running frames.
pub const HONOUR_CAP_FRAMES: u64 = 15;

/// The honour window in running frames for a claimer whose round trip is
/// `rtt` (the link's smoothed RTT; `None` or zero: no repair counts):
/// `min(ceil(60 * rtt + SNAPSHOT_EVERY) - 1, HONOUR_CAP_FRAMES)`, with
/// [`crate::world::host::SNAPSHOT_EVERY`] the most a snapshot waits for its
/// send.
#[must_use]
pub fn honour_window(rtt: Option<std::time::Duration>) -> Option<u64> {
    let rtt = rtt.filter(|r| !r.is_zero())?;
    #[allow(clippy::cast_precision_loss)] // a small frame count
    let x = rtt.as_secs_f64() * 60.0 + crate::world::host::SNAPSHOT_EVERY as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole frames, small
    let w = (x.ceil() as u64).saturating_sub(1);
    Some(w.min(HONOUR_CAP_FRAMES))
}

/// The largest single repair the rule counts: a Constructor drone's
/// (`oConstructorDrone_Alarm_1`: `hull + 25`, capped at `hullMax`).
pub const HONOUR_REPAIR_MAX: f64 = 25.0;

/// The largest share of the enemy's max hull a counted repair may be: a
/// larger one is part of the fight.
pub const HONOUR_REPAIR_SHARE: f64 = 0.25;

/// Frames after an enemy's first tick here in which no repair counts: its
/// set-up (`scaleEnemyStats`, rarity, the spawner's multipliers) is not a
/// repair, and a healer can reach it then.
pub const HONOUR_SETTLE_FRAMES: u64 = 30;

/// Whether one repair of `amount` on an enemy of `hull_max` counts toward
/// the honour rule (before the set-up and type tests).
#[must_use]
pub fn repair_counts(amount: f64, hull_max: f64) -> bool {
    amount > 0.0 && amount <= HONOUR_REPAIR_MAX && amount <= HONOUR_REPAIR_SHARE * hull_max
}

/// An honoured claim, for its log line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Honour {
    /// The hull the claim left, written to 0.
    pub rest: f64,
    /// The counted repairs in the window and their sum.
    pub repairs: usize,
    pub repaired: f64,
    /// The window that applied, in running frames ([`honour_window`]).
    pub window: u64,
}

/// The repairs on an enemy in the window, for a refusal's log line:
/// counted (number, sum) and not counted (number).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RepairsSeen {
    pub counted: usize,
    pub sum: f64,
    pub uncounted: usize,
}

fn by_count(m: &HashMap<u16, u64>) -> Vec<(u16, u64)> {
    let mut v: Vec<(u16, u64)> = m.iter().map(|(&o, &n)| (o, n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// Frames in 2 s (the "refused then killed" window).
const REFUSED_KILLED_FRAMES: u64 = 120;

/// A refused enemy's rest under this is a crumb: less than any hit takes,
/// left only by how numbers were carried or rounded.
pub const CRUMB_REST: f64 = 0.01;

/// The most hull a claim can leave on its enemy that the claimant could not
/// see: half a step of the snapshot's hull fraction plus the reporter's
/// noise floor ([`HostDamage::carried_short`]). 0.0022 for a max of 160.
#[must_use]
pub fn carry_bound(hull_max: f32) -> f64 {
    f64::from(hull_frac_carry(hull_max)) + f64::from(EPS)
}

impl HostDamage {
    /// A fresh host side.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A peer's report: what to apply.
    pub fn on_report(&mut self, r: &DamageReport) -> Vec<Apply> {
        r.entries
            .iter()
            .filter_map(|e| {
                let a = self.ledger.report(r.by, e);
                (a.delta > 0.0 || a.claim > 0).then_some(Apply {
                    net_id: e.net_id,
                    by: r.by,
                    delta: a.delta,
                    claim: a.claim,
                })
            })
            .collect()
    }

    /// A claim `a` left its enemy at `hull_after` (applied, before judging):
    /// whether that rest is only what carrying the hull to the claimant
    /// lost, so the glue writes 0 and the kill stands (counted). The bound
    /// is [`carry_bound`]: the snapshot's 16-bit hull fraction is off by up
    /// to [`hull_frac_carry`], and the reporter drops changes under [`EPS`].
    pub fn carried_short(&mut self, a: &Apply, hull_after: f64, hull_max: f32) -> bool {
        let short = a.claim > 0 && hull_after > 0.0 && hull_after <= carry_bound(hull_max);
        self.counts.carried += u64::from(short);
        short
    }

    /// The host's game repaired `net_id` by `amount` (`recordHealing` with
    /// the enemy as self): kept for [`HONOUR_CAP_FRAMES`], counted by
    /// the rule when [`repair_counts`], past the set-up and not `excluded`.
    pub fn repaired(&mut self, net_id: NetId, amount: f64, hull_max: f64, excluded: bool) {
        let frame = self.frame;
        let settled = self
            .born
            .get(&net_id)
            .is_some_and(|b| frame.saturating_sub(*b) >= HONOUR_SETTLE_FRAMES);
        let counted = settled && !excluded && repair_counts(amount, hull_max);
        if counted {
            self.counts.repairs_counted += 1;
        } else {
            self.counts.repairs_uncounted += 1;
        }
        let v = self.repairs.entry(net_id).or_default();
        v.retain(|r| frame.saturating_sub(r.frame) <= HONOUR_CAP_FRAMES);
        v.push(Repair {
            frame,
            amount,
            counted,
        });
    }

    /// The repairs on `net_id` within `window` frames ([`honour_window`];
    /// `None`, no round-trip measure: those within the cap, none counted).
    #[must_use]
    pub fn repairs_seen(&self, net_id: NetId, window: Option<u64>) -> RepairsSeen {
        let mut out = RepairsSeen::default();
        for r in self.repairs.get(&net_id).into_iter().flatten() {
            if self.frame.saturating_sub(r.frame) > window.unwrap_or(HONOUR_CAP_FRAMES) {
                continue;
            }
            if r.counted && window.is_some() {
                out.counted += 1;
                out.sum += r.amount;
            } else {
                out.uncounted += 1;
            }
        }
        out
    }

    /// A claim `a` left its enemy at `hull_after` (applied, before judging;
    /// not [`Self::carried_short`]): whether the honour rule makes it a kill
    /// within `window` ([`honour_window`] of the claimer's round trip). The
    /// glue then writes 0: the enemy dies by the game's own path as the
    /// claimer's confirmed kill.
    pub fn honour(
        &mut self,
        a: &Apply,
        hull_after: f64,
        hull_max: f32,
        window: Option<u64>,
    ) -> Option<Honour> {
        if a.claim == 0 || hull_after <= 0.0 {
            return None;
        }
        let window = window?;
        let seen = self.repairs_seen(a.net_id, Some(window));
        if seen.counted == 0 || hull_after > seen.sum + carry_bound(hull_max) {
            return None;
        }
        self.counts.honoured += 1;
        self.honoured_rest.insert(a.net_id, hull_after);
        Some(Honour {
            rest: hull_after,
            repairs: seen.counted,
            repaired: seen.sum,
            window,
        })
    }

    /// The glue applied `a`: hull read before and after (`None`: the enemy
    /// is gone here), its max hull and object.
    pub fn applied(&mut self, a: &Apply, read: Option<(f64, f64)>, hull_max: f32, object: u16) {
        self.objects.insert(a.net_id, object);
        if a.delta <= 0.0 {
            return;
        }
        self.counts.applies += 1;
        self.counts.applied += f64::from(a.delta);
        let Some((before, after)) = read else {
            self.counts.missing += 1;
            return;
        };
        let lost = before - after;
        // A claim the glue wrote to 0 ([`Self::carried_short`]) lost its rest
        // as well: no mismatch (over 0.01 for a max above about 1,200).
        let extra = lost - f64::from(a.delta);
        let carried = a.claim > 0 && after <= 0.0 && extra > 0.0 && extra <= carry_bound(hull_max);
        // An honoured claim's rest, written to 0 ([`Self::honour`]): the
        // lost repair, counted as its own class.
        let honoured = self
            .honoured_rest
            .remove(&a.net_id)
            .filter(|r| a.claim > 0 && after <= 0.0 && (extra - r).abs() <= 0.01);
        if let Some(r) = honoured {
            self.counts.lost_repair += r;
        } else if !carried && extra.abs() > 0.01 {
            self.counts.mismatched += 1;
        }
        let acct = self.accts.entry(a.net_id).or_default();
        acct.applied += f64::from(a.delta);
        acct.hull_max = hull_max;
        if before > 0.0 && after <= 0.0 {
            acct.peer_zeroed = true;
        }
    }

    /// Judge a claim given the hull after everything was applied. A
    /// confirmed claim is answered by the enemy's death, or refused by
    /// [`Self::overdue`] if it still lives after the bound.
    pub fn judge(&mut self, a: &Apply, hull_after: Option<f64>) -> Verdict {
        self.counts.claims += 1;
        if self.refused_zero.contains(&a.net_id) {
            self.counts.reclaims += 1;
        }
        self.ledger.judged(a.by, a.net_id, a.claim);
        if hull_after.is_some_and(|h| h > 0.0) {
            self.counts.refused += 1;
            self.refused(a.net_id, hull_after.unwrap_or(0.0));
            Verdict::Refuse
        } else {
            if hull_after.is_some() {
                self.pending
                    .entry(a.net_id)
                    .or_insert((a.by, self.frame, a.claim));
            }
            Verdict::Confirm
        }
    }

    /// Sounds (`crate::sound`): the peer whose claim on `net_id` is confirmed
    /// and still open; that peer's game plays the death sounds.
    #[must_use]
    pub fn confirmed_claim(&self, net_id: NetId) -> Option<PeerId> {
        self.pending.get(&net_id).map(|(p, _, _)| *p)
    }

    fn refused(&mut self, net_id: NetId, hull: f64) {
        let object = self.objects.get(&net_id).copied().unwrap_or(0);
        if hull <= 0.0 {
            self.refused_zero.insert(net_id);
            *self.refused_zero_by.entry(object).or_default() += 1;
        } else if hull < CRUMB_REST {
            self.counts.refused_crumb += 1;
        } else {
            self.counts.refused_more += 1;
        }
        self.refused_at.insert(net_id, (self.frame, object));
    }

    /// Confirmed claims whose enemy still lives [`REFUSE_AFTER_FRAMES`]
    /// after: refused now (`hull_of`: the enemy's hull, `None` when gone).
    /// Returns (net id, claimant, hull, claim number) of each.
    pub fn overdue(
        &mut self,
        hull_of: impl Fn(NetId) -> Option<f32>,
    ) -> Vec<(NetId, PeerId, f32, u16)> {
        let frame = self.frame;
        let due: Vec<(NetId, PeerId, u16)> = self
            .pending
            .iter()
            .filter(|(_, (_, at, _))| frame.saturating_sub(*at) >= REFUSE_AFTER_FRAMES)
            .map(|(&n, &(p, _, c))| (n, p, c))
            .collect();
        let mut out = Vec::new();
        for (n, p, c) in due {
            self.pending.remove(&n);
            if let Some(h) = hull_of(n) {
                self.counts.refused += 1;
                self.counts.refused_late += 1;
                self.refused(n, f64::from(h));
                out.push((n, p, h, c));
            }
        }
        out
    }

    /// Acks for this frame (`hull_of`: an enemy's hull now). `running`: the
    /// world runs; the stuck watch's clock counts only those frames.
    pub fn acks(&mut self, hull_of: impl Fn(NetId) -> Option<f32>, running: bool) -> Vec<Message> {
        if running {
            self.frame += 1;
        }
        self.ledger.acks(hull_of)
    }

    /// An enemy's hull this frame (its tick): watch for "stuck at 0".
    /// Returns the enemy when this tick counts it stuck.
    pub fn watch(&mut self, net_id: NetId, hull: f32) -> Option<Stuck> {
        let frame = self.frame;
        self.born.entry(net_id).or_insert(frame);
        if hull > 0.0 {
            self.refused_zero.remove(&net_id);
        }
        let a = self.accts.get_mut(&net_id)?;
        if hull > 0.0 {
            a.zero_since = None;
            a.peer_zeroed = false;
            return None;
        }
        let since = *a.zero_since.get_or_insert(frame);
        if frame - since == HOST_STUCK_FRAMES
            && a.peer_zeroed
            && !self.refused_zero.contains(&net_id)
        {
            self.counts.stuck += 1;
            return Some(Stuck {
                net_id,
                object: self.objects.get(&net_id).copied().unwrap_or(0),
                hull,
                applied: a.applied,
            });
        }
        None
    }

    /// The enemy is gone here (`killed`: through its death). `by_peer`:
    /// the despawn names a peer as the killer.
    pub fn gone(&mut self, net_id: NetId, killed: bool, by_peer: bool) {
        self.ledger.forget(net_id);
        self.pending.remove(&net_id);
        self.refused_zero.remove(&net_id);
        self.objects.remove(&net_id);
        self.born.remove(&net_id);
        self.repairs.remove(&net_id);
        self.honoured_rest.remove(&net_id);
        if let Some((at, object)) = self.refused_at.remove(&net_id)
            && killed
            && self.frame.saturating_sub(at) <= REFUSED_KILLED_FRAMES
        {
            self.counts.refused_then_killed += 1;
            *self.refused_killed_by.entry(object).or_default() += 1;
        }
        let Some(a) = self.accts.remove(&net_id) else {
            return;
        };
        if killed && by_peer {
            self.counts.deaths_by_peer += 1;
            self.counts.peer_applied_at_death += a.applied;
            self.counts.peer_hull_max_at_death += f64::from(a.hull_max);
        }
    }

    /// Counters so far.
    #[must_use]
    pub fn counts(&self) -> HostCounts {
        self.counts
    }

    /// "Refused then killed within 2 s" by object, most first.
    #[must_use]
    pub fn refused_killed_by(&self) -> Vec<(u16, u64)> {
        by_count(&self.refused_killed_by)
    }

    /// Refusals at `hull <= 0` by object, most first.
    #[must_use]
    pub fn refused_zero_by(&self) -> Vec<(u16, u64)> {
        by_count(&self.refused_zero_by)
    }

    /// Forget every enemy (run end); counters stay.
    pub fn clear(&mut self) {
        self.ledger.clear();
        self.accts.clear();
        self.pending.clear();
        self.refused_zero.clear();
        self.refused_at.clear();
        self.objects.clear();
        self.born.clear();
        self.repairs.clear();
        self.honoured_rest.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::msg::{AckEntry, DamageAck, DamageEntry};

    const ME: PeerId = PeerId(1);

    /// Every status tick is a bracketed hull writer.
    #[test]
    fn status_sites_are_hull_writers() {
        for s in super::glue::STATUS_SITES {
            assert!(super::glue::AUDIT_SITES.contains(s), "{s}");
        }
    }

    fn ack(net_id: NetId, applied: f32, hull: f32, frame: u32) -> Message {
        Message::DamageAck(DamageAck {
            peer: ME,
            frame,
            entries: vec![AckEntry {
                net_id,
                applied,
                hull,
                judged: 0,
            }],
        })
    }

    #[test]
    fn measures_drops_between_own_writes_and_writes_the_view() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        // First tick: no baseline, nothing measured; host hull written.
        let t = c.tick(3, 30, 100.0, Some(100.0));
        assert!((t.write - 100.0).abs() < 1e-6 && t.drop.abs() < 1e-6);
        // Local hits took 30; the host has not applied them: write 70.
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 100.0, Some(70.0));
        assert!((t.drop - 30.0).abs() < 1e-6);
        assert!((t.write - 70.0).abs() < 1e-6 && !t.dying);
        // A heal or a host write in between is not damage.
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 100.0, Some(80.0));
        assert!(t.drop.abs() < 1e-6);
        assert_eq!(c.counts().drops, 1);
        let r = c.take_reports(ME);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn a_copy_dies_here_when_own_damage_uses_up_the_hull() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 50.0, Some(50.0));
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 50.0, Some(-5.0));
        assert!(t.dying && t.write <= 0.0, "{t:?}");
        assert!(c.is_dying(30) && c.dying_net_id(30) == Some(3));
        // Its own Step runs the death path next frame.
        let _ = c.begin_frame();
        assert!(c.gone(3));
        assert_eq!(c.counts().lag, [0, 1, 0, 0]);
        assert!(c.take_reports(ME).iter().any(|m| matches!(
            m,
            Message::DamageReport(r) if r.entries[0].claim > 0
        )));
        // The host confirms.
        c.on_message(
            ME,
            &Message::Despawn {
                net_id: 3,
                cause: DespawnCause::Killed { by: Some(ME) },
            },
        );
        let n = c.counts();
        assert_eq!((n.claims, n.deaths, n.confirmed), (1, 1, 1));
    }

    #[test]
    fn damage_before_the_first_tick_counts_from_the_spawn_hull() {
        let mut c = ClientDamage::new();
        c.baseline(8, 40.0);
        let _ = c.begin_frame();
        let t = c.tick(8, 80, 40.0, Some(15.0));
        assert!((t.drop - 25.0).abs() < 1e-6 && (t.write - 15.0).abs() < 1e-6);
    }

    #[test]
    fn a_copy_made_after_its_despawn_is_never_claimed() {
        // The despawn came before the copy was made: no claim on it.
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        c.on_message(
            ME,
            &Message::Despawn {
                net_id: 9,
                cause: DespawnCause::Killed { by: None },
            },
        );
        let _ = c.begin_frame();
        c.baseline(9, 30.0);
        let t = c.tick(9, 90, 30.0, Some(-50.0));
        assert!(t.overkill, "overkill on a host-dead copy");
        assert!(!c.gone(9), "not a death of a claimed copy");
        for _ in 0..=ANSWER_FRAMES {
            assert!(c.begin_frame().is_empty());
        }
        let n = c.counts();
        assert_eq!((n.claims, n.unanswered, n.one_side, n.deaths), (0, 0, 0, 0));
        // The same net sent again later is a live enemy once more.
        let s = ndml_net::msg::EnemySpawn::new(9, 1, 0.0, 0.0, 0.0, 30.0);
        c.on_message(ME, &Message::EnemySpawn(s));
        c.baseline(9, 30.0);
        let t = c.tick(9, 90, 30.0, Some(10.0));
        assert!(!t.overkill && (t.drop - 20.0).abs() < 1e-6);
    }

    #[test]
    fn a_copy_the_host_keeps_at_zero_is_not_stuck() {
        let mut c = ClientDamage::new();
        // The host's own instance sits at 0 (a Star Eater leg): not stuck.
        for _ in 0..=STUCK_FRAMES + 1 {
            let _ = c.begin_frame();
            let _ = c.tick(5, 50, 0.0, Some(0.0));
        }
        assert_eq!(c.counts.stuck, 0);
    }

    #[test]
    fn no_prediction_without_own_damage_in_flight() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 0.0, Some(0.0));
        assert!(!t.dying, "the host's own 0 is the host's to act on");
    }

    #[test]
    fn a_hit_in_the_frame_of_a_refusal_on_a_copy_alive_here_is_reported() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 20.0, Some(20.0));
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 20.0, Some(0.0));
        assert!(t.dying && (t.drop - 20.0).abs() < 1e-6);
        // Hit again (5) before the next tick; the host's refusal comes
        // first: it still has the enemy at 15.
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 3,
                peer: ME,
                hull: 15.0,
                claim: 1,
            },
        );
        assert_eq!(c.counts().refused_alive, 1);
        assert!(c.refused_lately(3));
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 15.0, Some(-5.0));
        assert!((t.drop - 5.0).abs() < 1e-6, "the hit is measured: {t:?}");
        assert!((c.rep.total(3) - 25.0).abs() < 1e-6, "and reported");
    }

    #[test]
    fn a_no_claim_mark_counts_how_it_ends_and_host_kills_naming_this_peer_are_counted() {
        let mut c = ClientDamage::new();
        // Claimed, refused at 0 while alive here: no claim until a rise.
        let refuse_at_zero = |c: &mut ClientDamage, net: NetId| {
            let _ = c.begin_frame();
            let _ = c.tick(net, usize::from(net) * 10, 20.0, Some(20.0));
            let _ = c.begin_frame();
            assert!(c.tick(net, usize::from(net) * 10, 20.0, Some(0.0)).dying);
            let claim = c.rep.last_claim(net);
            c.on_message(
                ME,
                &Message::KillRefused {
                    net_id: net,
                    peer: ME,
                    hull: -3.0,
                    claim,
                },
            );
        };
        refuse_at_zero(&mut c, 3);
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, -3.0, Some(-3.0));
        assert_eq!(c.counts().no_claim_ended, NoClaimEnds::default(), "at 0");
        let _ = c.tick(3, 30, 4.0, Some(-3.0));
        assert_eq!(c.counts().no_claim_ended.hull_rise, 1);
        refuse_at_zero(&mut c, 4);
        c.on_message(
            ME,
            &Message::Despawn {
                net_id: 4,
                cause: DespawnCause::Killed { by: Some(ME) },
            },
        );
        let n = c.counts();
        assert_eq!(n.no_claim_ended.despawn, 1);
        assert_eq!((n.host_kills_damaged, n.host_kills_ours), (1, 1));
        refuse_at_zero(&mut c, 5);
        c.on_message(ME, &Message::WorldResync { enemies: 0 });
        assert_eq!(c.counts().no_claim_ended.resync, 1);
        refuse_at_zero(&mut c, 6);
        c.clear();
        assert_eq!(c.counts().no_claim_ended.cleared, 1);
        // A host kill of an enemy damaged here that names no one.
        let _ = c.begin_frame();
        let _ = c.tick(7, 70, 20.0, Some(20.0));
        let _ = c.begin_frame();
        let _ = c.tick(7, 70, 20.0, Some(15.0));
        c.on_message(
            ME,
            &Message::Despawn {
                net_id: 7,
                cause: DespawnCause::Killed { by: None },
            },
        );
        let n = c.counts();
        assert_eq!((n.host_kills_damaged, n.host_kills_ours), (2, 1));
    }

    #[test]
    fn a_refusal_is_recent_for_a_bounded_time() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let _ = c.tick(4, 40, 10.0, Some(10.0));
        let _ = c.begin_frame();
        assert!(c.tick(4, 40, 10.0, Some(0.0)).dying);
        assert!(c.gone(4), "died here");
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 4,
                peer: ME,
                hull: 6.0,
                claim: 1,
            },
        );
        assert!(c.refused_lately(4) && c.open_claims() == vec![4]);
        for _ in 0..REFUSED_LEFT_FRAMES {
            let _ = c.begin_frame();
        }
        assert!(!c.refused_lately(4));
        assert!(!c.refused_lately(5), "never refused");
    }

    #[test]
    fn the_hosts_word_outlives_a_copy_not_made_yet() {
        // The despawn came at frame 10, before the copy was made: kept.
        assert!(host_gone_kept(false, 10, 11));
        assert!(host_gone_kept(false, 10, 10 + HOST_GONE_KEEP_FRAMES - 1));
        // Bounded with no copy; kept as long as a copy is live.
        assert!(!host_gone_kept(false, 10, 10 + HOST_GONE_KEEP_FRAMES));
        assert!(host_gone_kept(true, 10, 10 + 10 * HOST_GONE_KEEP_FRAMES));
    }

    #[test]
    fn a_dying_copy_dies_only_by_its_own_code_or_the_hosts_word() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 20.0, Some(20.0));
        let _ = c.begin_frame();
        assert!(c.tick(3, 30, 20.0, Some(0.0)).dying);
        // Its own code does not kill it (an Omega Station wall): it waits
        // at 0, never removed by the mod.
        for _ in 0..REFUSE_AFTER_FRAMES {
            let _ = c.begin_frame();
            let t = c.tick(3, 30, 0.0, Some(0.0));
            assert!(t.dying && !t.remove && t.write <= 0.0, "{t:?}");
        }
        // The host's word: refused at 0. It stays, and is not claimed again
        // while the host's hull stays at 0, however much it is hit.
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 3,
                peer: ME,
                hull: 0.0,
                claim: 1,
            },
        );
        let n = c.counts();
        assert_eq!((n.refused, n.refused_alive, n.deaths), (1, 1, 0));
        assert!(c.open_claims().is_empty(), "nothing to send again");
        assert_eq!(
            c.times().word,
            vec![u32::try_from(REFUSE_AFTER_FRAMES).unwrap()]
        );
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 0.0, Some(0.0));
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 0.0, Some(-4.0));
        assert!(!t.dying && t.write <= 0.0, "no claim without a hull rise");
        assert_eq!(c.counts().claims, 1);
        // The host's hull rises (healed): a claim is possible again.
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 5.0, Some(5.0));
        let _ = c.begin_frame();
        assert!(c.tick(3, 30, 5.0, Some(-1.0)).dying);
        assert_eq!(c.counts().claims, 2);
    }

    #[test]
    fn an_enemy_refused_above_zero_dies_here_only_at_the_hosts_word() {
        // After a refusal above 0 an enemy's kill waits for the host. One
        // refused at 0 keeps the no-claim rule. Another enemy is predicted.
        let mut c = ClientDamage::new();
        for (net, object) in [(2, 300), (3, 300)] {
            c.on_message(
                ME,
                &Message::EnemySpawn(ndml_net::msg::EnemySpawn::new(
                    net, object, 0.0, 0.0, 0.0, 600.0,
                )),
            );
            let _ = c.begin_frame();
            let _ = c.tick(net, 40 + usize::from(net), 100.0, Some(100.0));
        }
        let _ = c.begin_frame();
        assert!(c.tick(2, 42, 100.0, Some(-5.0)).dying, "the first claim");
        assert!(c.gone(2));
        let claim = c.rep.last_claim(2);
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 2,
                peer: ME,
                hull: 20.0,
                claim,
            },
        );
        // Made again from the host's word, at its hull, and hit again.
        c.baseline(2, 20.0);
        let _ = c.begin_frame();
        let _ = c.tick(2, 46, 20.0, Some(20.0));
        let _ = c.begin_frame();
        let t = c.tick(2, 46, 20.0, Some(-3.0));
        assert!(!t.dying && (t.write - 20.0).abs() < 1e-3, "{t:?}");
        let _ = c.begin_frame();
        assert!(c.tick(3, 43, 100.0, Some(-5.0)).dying, "another enemy");
    }

    #[test]
    fn the_first_repair_of_a_type_can_race_the_joining_players_kill_and_comes_back_once() {
        // An enemy repaired 25 on the host while this player's kill was in
        // flight: claimed here at 0, refused at 20. No type waits for the
        // host's word before a refusal, so the first claim goes; the refusal
        // that follows is plain and once (`host_word`).
        let mut c = ClientDamage::new();
        c.clear();
        c.on_message(
            ME,
            &Message::EnemySpawn(ndml_net::msg::EnemySpawn::new(2, 301, 0.0, 0.0, 0.0, 600.0)),
        );
        let _ = c.begin_frame();
        let _ = c.tick(2, 40, 100.0, Some(100.0));
        let _ = c.begin_frame();
        assert!(
            c.tick(2, 40, 100.0, Some(-5.0)).dying,
            "claimed: no rise seen yet"
        );
        assert!(c.gone(2));
        let claim = c.rep.last_claim(2);
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 2,
                peer: ME,
                hull: 20.0,
                claim,
            },
        );
        c.baseline(2, 20.0);
        let _ = c.begin_frame();
        let _ = c.tick(2, 41, 20.0, Some(20.0));
        let _ = c.begin_frame();
        assert!(!c.tick(2, 41, 20.0, Some(-3.0)).dying, "once");
        assert_eq!((c.counts().claims, c.counts().heal_held), (1, 1));
    }

    #[test]
    fn an_enemy_that_regains_hull_is_predicted_like_any_other() {
        // An enemy its drones repair 25 at a time dies here at once like
        // any enemy; nothing waits for the host's word.
        let mut c = ClientDamage::new();
        c.on_message(
            ME,
            &Message::EnemySpawn(ndml_net::msg::EnemySpawn::new(
                21, 367, 0.0, 0.0, 0.0, 2376.0,
            )),
        );
        for h in [100.0, 2376.0, 50.0, 50.0, 75.0] {
            let _ = c.begin_frame();
            let _ = c.tick(21, 210, h, None);
        }
        let _ = c.begin_frame();
        assert!(c.tick(21, 210, 75.0, Some(-5.0)).dying);
        assert_eq!((c.counts().heal_held, c.counts().claims), (0, 1));
    }

    fn spawn_msg(net_id: NetId, hull_max: f32) -> Message {
        Message::EnemySpawn(ndml_net::msg::EnemySpawn::new(
            net_id, 1, 0.0, 0.0, 0.0, hull_max,
        ))
    }

    fn claims_in(m: &[Message]) -> Vec<(NetId, u16)> {
        m.iter()
            .flat_map(|m| match m {
                Message::DamageReport(r) => r.entries.clone(),
                _ => Vec::new(),
            })
            .filter(|e| e.claim > 0)
            .map(|e| (e.net_id, e.claim))
            .collect()
    }

    /// A copy shot to 0 here and dead by its own code; its claim unanswered.
    fn died_here_unanswered(c: &mut ClientDamage, net: NetId) {
        let _ = c.begin_frame();
        let _ = c.tick(net, usize::from(net) * 10, 20.0, Some(20.0));
        let _ = c.begin_frame();
        assert!(c.tick(net, usize::from(net) * 10, 20.0, Some(0.0)).dying);
        let _ = c.begin_frame();
        assert!(c.gone(net));
        // Its report never reaches the host (the link went).
        let _ = c.take_reports(ME);
    }

    #[test]
    fn every_way_a_claim_leaves_the_table_is_counted() {
        let mut c = ClientDamage::new();
        for n in 1..=6 {
            died_here_unanswered(&mut c, n);
        }
        let killed = |by| Message::Despawn {
            net_id: 0,
            cause: DespawnCause::Killed { by },
        };
        let to = |m: Message, net: NetId| match m {
            Message::Despawn { cause, .. } => Message::Despawn { net_id: net, cause },
            other => other,
        };
        c.on_message(ME, &to(killed(Some(ME)), 1));
        c.on_message(ME, &to(killed(None), 2));
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 3,
                peer: ME,
                hull: 5.0,
                claim: 1,
            },
        );
        c.on_message(ME, &spawn_msg(4, 20.0));
        let l = c.counts().left;
        assert_eq!(
            (l.confirmed, l.confirmed_other, l.refused, l.respawned),
            (1, 1, 1, 1)
        );
        // 5 and 6 wait; the run ends: both cleared, nothing else moves.
        assert_eq!(c.awaiting(), 2);
        c.clear();
        let l = c.counts().left;
        assert_eq!(l.cleared, 2);
        assert_eq!((l.given_up, l.resync_gone, l.overwritten), (0, 0, 0));
        // One unanswered past the bound: given up.
        died_here_unanswered(&mut c, 7);
        for _ in 0..ANSWER_FRAMES {
            let _ = c.begin_frame();
        }
        assert_eq!(c.counts().left.given_up, 1);
        // One the resync does not name: gone in the gap.
        died_here_unanswered(&mut c, 8);
        c.on_message(ME, &Message::WorldResync { enemies: 0 });
        assert_eq!(c.counts().left.resync_gone, 1);
        assert_eq!(c.awaiting(), 0);
    }

    #[test]
    fn a_kill_claimed_before_a_resync_is_not_lost() {
        // Killed here before the link went; the host sends it again in the
        // resync: no copy while the claim is open.
        let mut c = ClientDamage::new();
        died_here_unanswered(&mut c, 3);
        c.on_message(ME, &Message::WorldResync { enemies: 1 });
        c.on_message(ME, &spawn_msg(3, 20.0));
        assert_eq!(c.held_spawns(), vec![3], "no copy while the claim is open");
        assert_eq!(c.open_claims(), vec![3]);
        let _ = c.begin_frame();
        assert_eq!(
            claims_in(&c.take_reports(ME)),
            vec![(3, 1)],
            "the same claim, sent again"
        );
        // The host confirms: still no copy (the world would make a spawn
        // killed before its copy to die at once: a second death).
        c.on_message(
            ME,
            &Message::Despawn {
                net_id: 3,
                cause: DespawnCause::Killed { by: Some(ME) },
            },
        );
        assert_eq!(c.counts().confirmed, 1);
        assert_eq!(c.held_spawns(), vec![3]);
        for _ in 0..HELD_AFTER_CONFIRM_FRAMES {
            let _ = c.begin_frame();
        }
        assert!(c.held_spawns().is_empty(), "let go once the world has");
        assert_eq!(c.counts().respawned, 0);
    }

    #[test]
    fn a_kill_the_host_confirmed_during_the_gap_does_not_die_twice() {
        // The late report raced the resync: the host killed the enemy
        // before resending its world, so the resync does not name it.
        let mut c = ClientDamage::new();
        died_here_unanswered(&mut c, 3);
        c.on_message(ME, &Message::WorldResync { enemies: 1 });
        c.on_message(ME, &spawn_msg(8, 50.0));
        assert!(c.held_spawns().is_empty());
        assert!(c.open_claims().is_empty(), "gone on the host: settled");
        assert_eq!(c.counts().confirmed_other, 1);
        assert!(
            claims_in(&c.take_reports(ME)).is_empty(),
            "nothing sent for it"
        );
        // ... or the resync named it and the confirm came after: held, not
        // made, so its Despawn makes no second death.
        let mut c = ClientDamage::new();
        died_here_unanswered(&mut c, 4);
        c.on_message(ME, &Message::WorldResync { enemies: 1 });
        c.on_message(ME, &spawn_msg(4, 20.0));
        c.on_message(
            ME,
            &Message::Despawn {
                net_id: 4,
                cause: DespawnCause::Killed { by: Some(ME) },
            },
        );
        assert_eq!(c.held_spawns(), vec![4]);
    }

    #[test]
    fn a_claim_refused_after_a_resync_makes_the_copy() {
        let mut c = ClientDamage::new();
        died_here_unanswered(&mut c, 3);
        c.on_message(ME, &Message::WorldResync { enemies: 1 });
        c.on_message(ME, &spawn_msg(3, 20.0));
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 3,
                peer: ME,
                hull: 6.0,
                claim: 1,
            },
        );
        assert!(c.held_spawns().is_empty(), "the host's word: it lives");
        // A claim given up unanswered lets go too.
        let mut c = ClientDamage::new();
        died_here_unanswered(&mut c, 5);
        c.on_message(ME, &Message::WorldResync { enemies: 1 });
        c.on_message(ME, &spawn_msg(5, 20.0));
        for _ in 0..=ANSWER_FRAMES {
            let _ = c.begin_frame();
        }
        assert!(c.held_spawns().is_empty());
        assert_eq!(c.counts().unanswered, 1);
    }

    #[test]
    fn a_resync_keeps_totals_for_enemies_it_sends_again_only() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        c.baseline(6, 500.0);
        c.baseline(7, 100.0);
        let _ = c.tick(6, 60, 500.0, Some(200.0)); // 300 on the boss
        let _ = c.tick(7, 70, 100.0, Some(60.0)); // 40 on another
        let mut l = Ledger::new();
        let mut hull6 = 500.0_f32;
        for m in c.take_reports(ME) {
            if let Message::DamageReport(r) = m {
                for e in &r.entries {
                    if e.net_id == 6 {
                        hull6 -= l.report(ME, e).delta;
                    }
                }
            }
        }
        assert!((hull6 - 200.0).abs() < 1e-3);
        // A long cut; 7 died on the host meanwhile; the resync names 6 only.
        c.on_message(ME, &Message::WorldResync { enemies: 1 });
        c.on_message(ME, &spawn_msg(6, 500.0));
        let _ = c.begin_frame();
        let again = c.take_reports(ME);
        let nets: Vec<NetId> = again
            .iter()
            .flat_map(|m| match m {
                Message::DamageReport(r) => r.entries.iter().map(|e| e.net_id).collect(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(nets, vec![6], "7 forgotten, 6 kept");
        // The first hit after the return counts at once.
        c.baseline(6, 200.0);
        let _ = c.begin_frame();
        let _ = c.tick(6, 60, 200.0, Some(190.0));
        for m in c.take_reports(ME) {
            if let Message::DamageReport(r) = m {
                for e in &r.entries {
                    hull6 -= l.report(ME, e).delta;
                }
            }
        }
        assert!((hull6 - 190.0).abs() < 1e-3, "{hull6}");
    }

    #[test]
    fn a_resync_whose_spawns_never_complete_lets_go_in_time() {
        let mut c = ClientDamage::new();
        died_here_unanswered(&mut c, 3);
        c.on_message(ME, &Message::WorldResync { enemies: 2 });
        // One spawn of two arrives, not naming 3.
        c.on_message(ME, &spawn_msg(8, 10.0));
        assert_eq!(c.open_claims(), vec![3], "still waiting");
        for _ in 0..RESYNC_WAIT_FRAMES {
            let _ = c.begin_frame();
        }
        assert!(c.open_claims().is_empty());
    }

    #[test]
    fn a_refusal_after_a_death_here_brings_the_copy_back() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 20.0, Some(20.0));
        let _ = c.begin_frame();
        assert!(c.tick(3, 30, 20.0, Some(0.0)).dying);
        let _ = c.begin_frame();
        assert!(c.gone(3), "its own code killed it");
        assert_eq!(c.times().own, vec![1]);
        assert_eq!(c.open_claims(), vec![3], "claimed, not answered");
        // The host had healed it: refused, then the spawn comes back.
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 3,
                peer: ME,
                hull: 5.0,
                claim: 1,
            },
        );
        assert_eq!(c.counts().refused, 1);
        assert_eq!(c.awaiting(), 0);
        assert_eq!(c.open_claims(), vec![3], "refused, not sent again yet");
        let s = ndml_net::msg::EnemySpawn::new(3, 1, 0.0, 0.0, 0.0, 20.0);
        c.on_message(ME, &Message::EnemySpawn(s));
        assert_eq!(c.counts().respawned, 1);
        assert_eq!(
            c.times().remade,
            vec![0],
            "made again in the frame of the refusal"
        );
        assert!(c.open_claims().is_empty(), "settled");
    }

    #[test]
    fn a_refused_claim_keeps_the_total_and_a_lingering_despawned_copy_goes() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let _ = c.tick(5, 50, 30.0, Some(30.0));
        let _ = c.begin_frame();
        assert!(c.tick(5, 50, 30.0, Some(0.0)).dying);
        assert!(c.gone(5));
        c.on_message(
            ME,
            &Message::KillRefused {
                net_id: 5,
                peer: ME,
                hull: 6.0,
                claim: 1,
            },
        );
        // The host keeps the 30 it applied: the next report says 30 + more.
        c.baseline(5, 6.0);
        let _ = c.begin_frame();
        let _ = c.tick(5, 50, 6.0, Some(1.0));
        let total = c.take_reports(ME).iter().find_map(|m| match m {
            Message::DamageReport(r) => r.entries.first().map(|e| e.total),
            _ => None,
        });
        assert!(total.is_some_and(|t| (t - 35.0).abs() < 1e-3), "{total:?}");
        // The host despawns it and the copy stays: removed after GHOST_FRAMES.
        c.on_message(
            ME,
            &Message::Despawn {
                net_id: 5,
                cause: DespawnCause::Killed { by: None },
            },
        );
        let mut gone = false;
        for _ in 0..=GHOST_FRAMES {
            let _ = c.begin_frame();
            gone |= c.tick(5, 50, 6.0, Some(1.0)).remove;
        }
        assert!(gone);
        assert_eq!(c.counts().ghosts, 1);
    }

    #[test]
    fn the_view_follows_the_hosts_acks() {
        let mut c = ClientDamage::new();
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 100.0, Some(100.0));
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 100.0, Some(60.0));
        // More local damage (20) before the host's ack: it applied the first
        // 40 and took 10 more itself (hull 50), so the copy shows 50 - 20.
        let _ = c.begin_frame();
        let _ = c.tick(3, 30, 100.0, Some(40.0));
        c.on_message(ME, &ack(3, 40.0, 50.0, 1));
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 100.0, Some(40.0));
        assert!((t.write - 30.0).abs() < 1e-6, "{t:?}");
        // Everything applied: the snapshot's hull stands.
        c.on_message(ME, &ack(3, 60.0, 30.0, 2));
        let _ = c.begin_frame();
        let t = c.tick(3, 30, 28.0, Some(30.0));
        assert!((t.write - 28.0).abs() < 1e-6, "{t:?}");
    }

    #[test]
    fn host_applies_judges_and_counts() {
        let mut h = HostDamage::new();
        let r = DamageReport {
            by: ME,
            entries: vec![DamageEntry {
                net_id: 9,
                total: 60.0,
                claim: 0,
            }],
        };
        let a = h.on_report(&r);
        assert_eq!(a.len(), 1);
        h.applied(&a[0], Some((100.0, 40.0)), 100.0, 7);
        assert!(h.on_report(&r).is_empty(), "nothing new");
        let r = DamageReport {
            by: ME,
            entries: vec![DamageEntry {
                net_id: 9,
                total: 110.0,
                claim: 1,
            }],
        };
        let a = h.on_report(&r);
        assert!(a[0].claim == 1 && (a[0].delta - 50.0).abs() < 1e-6);
        h.applied(&a[0], Some((40.0, -10.0)), 100.0, 7);
        assert_eq!(h.judge(&a[0], Some(-10.0)), Verdict::Confirm);
        h.gone(9, true, true);
        let n = h.counts();
        assert_eq!((n.applies, n.mismatched, n.deaths_by_peer), (2, 0, 1));
        assert!((n.peer_applied_at_death / n.peer_hull_max_at_death - 1.1).abs() < 1e-6);
        // A heal first: refused.
        let a = h.on_report(&DamageReport {
            by: ME,
            entries: vec![DamageEntry {
                net_id: 10,
                total: 5.0,
                claim: 1,
            }],
        });
        h.applied(&a[0], Some((8.0, 3.0)), 8.0, 7);
        assert_eq!(h.judge(&a[0], Some(3.0)), Verdict::Refuse);
    }

    #[test]
    fn a_claims_final_state_decides_whose_death_sound_it_is() {
        let claim = |h: &mut HostDamage, net_id: NetId| {
            let a = h.on_report(&DamageReport {
                by: ME,
                entries: vec![DamageEntry {
                    net_id,
                    total: 50.0,
                    claim: 1,
                }],
            });
            h.applied(&a[0], Some((40.0, -10.0)), 100.0, 7);
            a
        };
        let mut h = HostDamage::new();
        // Confirmed: the claimant's to play until its enemy is gone.
        let a = claim(&mut h, 1);
        assert_eq!(h.judge(&a[0], Some(-10.0)), Verdict::Confirm);
        assert_eq!(h.confirmed_claim(1), Some(ME));
        h.gone(1, true, true);
        assert_eq!(h.confirmed_claim(1), None);
        // Refused: the host's.
        let a = claim(&mut h, 2);
        assert_eq!(h.judge(&a[0], Some(3.0)), Verdict::Refuse);
        assert_eq!(h.confirmed_claim(2), None);
        // Overdue (still alive after the bound; the claimant gives up later
        // still): the host's.
        let a = claim(&mut h, 3);
        assert_eq!(h.judge(&a[0], Some(0.0)), Verdict::Confirm);
        for _ in 0..REFUSE_AFTER_FRAMES {
            let _ = h.acks(|_| Some(0.0), true);
        }
        assert_eq!(h.overdue(|_| Some(0.0)).len(), 1);
        assert_eq!(h.confirmed_claim(3), None);
        // A report with no claim (the claimant predicted nothing): the host's.
        let a = h.on_report(&DamageReport {
            by: ME,
            entries: vec![DamageEntry {
                net_id: 4,
                total: 200.0,
                claim: 0,
            }],
        });
        h.applied(&a[0], Some((100.0, -100.0)), 100.0, 7);
        assert_eq!(h.confirmed_claim(4), None);
    }

    #[test]
    fn an_enemy_refused_at_zero_is_not_stuck_and_one_neither_dead_nor_refused_is() {
        let mut h = HostDamage::new();
        for (net, total) in [(6, 20.0), (7, 20.0)] {
            let a = h.on_report(&DamageReport {
                by: ME,
                entries: vec![DamageEntry {
                    net_id: net,
                    total,
                    claim: u16::from(net == 6),
                }],
            });
            h.applied(&a[0], Some((20.0, 0.0)), 20.0, 786);
            if net == 6 {
                let _ = h.judge(&a[0], Some(0.0));
            }
        }
        // Both stay alive at 0; 6 was claimed (refused after the bound),
        // 7 never was (nothing answers it).
        for _ in 0..=HOST_STUCK_FRAMES {
            let _ = h.acks(|_| Some(0.0), true);
            let _ = h.overdue(|_| Some(0.0));
            h.watch(6, 0.0);
            h.watch(7, 0.0);
        }
        assert_eq!(h.counts().refused_late, 1);
        assert_eq!(h.counts().stuck, 1, "7 only");
    }

    #[test]
    fn a_body_the_game_took_below_zero_is_not_stuck_under_peer_overkill() {
        // A boss body the game's own damage took below 0, taking peer drops.
        let mut h = HostDamage::new();
        let mut seen = Vec::new();
        for (net, before, after) in [(307, -1101.0, -1500.0), (9, 40.0, -5.0)] {
            let a = h.on_report(&DamageReport {
                by: ME,
                entries: vec![DamageEntry {
                    net_id: net,
                    total: 400.0,
                    claim: 0,
                }],
            });
            h.applied(&a[0], Some((before, after)), 30000.0, 768);
        }
        for _ in 0..=HOST_STUCK_FRAMES {
            let _ = h.acks(|_| Some(-1.0), true);
            seen.extend(h.watch(307, -1500.0));
            seen.extend(h.watch(9, -5.0));
        }
        assert_eq!(h.counts().stuck, 1);
        assert_eq!(seen.iter().map(|s| s.net_id).collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn held_reports_fold_to_one_entry_per_reporter_and_enemy() {
        let r = |total: f32, claim: u16| DamageReport {
            by: ME,
            entries: vec![DamageEntry {
                net_id: 4,
                total,
                claim,
            }],
        };
        let c = compact(vec![r(10.0, 0), r(30.0, 2), r(20.0, 1)]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].entries.len(), 1);
        assert!((c[0].entries[0].total - 30.0).abs() < 1e-6 && c[0].entries[0].claim == 2);
    }

    #[test]
    fn the_host_answers_every_claim_within_the_bound() {
        let mut h = HostDamage::new();
        let claim = |h: &mut HostDamage, total: f32, claim: u16| {
            h.on_report(&DamageReport {
                by: ME,
                entries: vec![DamageEntry {
                    net_id: 4,
                    total,
                    claim,
                }],
            })
        };
        let a = claim(&mut h, 30.0, 1);
        h.applied(&a[0], Some((30.0, 0.0)), 30.0, 786);
        assert_eq!(h.judge(&a[0], Some(0.0)), Verdict::Confirm);
        // Its own code keeps it alive at 0 (an Omega Station wall).
        for _ in 0..REFUSE_AFTER_FRAMES - 1 {
            let _ = h.acks(|_| Some(0.0), true);
            assert!(h.overdue(|_| Some(0.0)).is_empty());
        }
        // A pause does not age the claim.
        let _ = h.acks(|_| Some(0.0), false);
        assert!(h.overdue(|_| Some(0.0)).is_empty());
        let _ = h.acks(|_| Some(0.0), true);
        assert_eq!(h.overdue(|_| Some(0.0)), vec![(4, ME, 0.0, 1)]);
        assert!(h.overdue(|_| Some(0.0)).is_empty(), "answered once");
        // The same claim still riding in later reports: applied, not judged
        // again.
        let a = claim(&mut h, 32.0, 1);
        assert!(a[0].claim == 0 && (a[0].delta - 2.0).abs() < 1e-6);
        h.applied(&a[0], Some((0.0, -2.0)), 30.0, 786);
        for _ in 0..=REFUSE_AFTER_FRAMES {
            let _ = h.acks(|_| Some(-2.0), true);
            assert!(h.overdue(|_| Some(-2.0)).is_empty(), "nothing pending");
        }
        assert_eq!(h.counts().reclaims, 0);
        // A new claim (a higher number) without a hull rise is counted, and
        // refused after the bound.
        let a = claim(&mut h, 35.0, 2);
        h.applied(&a[0], Some((-2.0, -5.0)), 30.0, 786);
        assert_eq!(h.judge(&a[0], Some(-5.0)), Verdict::Confirm);
        assert_eq!(h.counts().reclaims, 1);
        for _ in 0..REFUSE_AFTER_FRAMES {
            let _ = h.acks(|_| Some(-5.0), true);
        }
        assert_eq!(h.overdue(|_| Some(-5.0)), vec![(4, ME, -5.0, 2)]);
        // It dies 1 s after that refusal: refused then killed, by object.
        for _ in 0..60 {
            let _ = h.acks(|_| Some(-5.0), true);
        }
        h.gone(4, true, false);
        let n = h.counts();
        assert_eq!(
            (n.refused_late, n.refused, n.refused_then_killed),
            (2, 2, 1)
        );
        assert_eq!(h.refused_killed_by(), vec![(786, 1)]);
        assert_eq!(h.refused_zero_by(), vec![(786, 2)]);
        // A death within the bound is the answer: nothing refused.
        let a = h.on_report(&DamageReport {
            by: ME,
            entries: vec![DamageEntry {
                net_id: 5,
                total: 10.0,
                claim: 1,
            }],
        });
        h.applied(&a[0], Some((10.0, 0.0)), 10.0, 7);
        let _ = h.judge(&a[0], Some(0.0));
        let _ = h.acks(|_| Some(0.0), true);
        h.gone(5, true, true);
        for _ in 0..=REFUSE_AFTER_FRAMES {
            let _ = h.acks(|_| None, true);
            assert!(h.overdue(|_| None).is_empty());
        }
    }

    /// A shooter hits a wall the host keeps at 0 over a link (`one_way`
    /// frames, `loss` of unreliable messages). Returns the host's counts and
    /// the refusals the shooter saw.
    fn wall_over_a_link(one_way: u64, loss: f64) -> (HostCounts, u64) {
        use ndml_net::damage::Reporter;
        let mut rng = ndml_net::rng::Rng::new(11);
        let mut rep = Reporter::new();
        let mut h = HostDamage::new();
        let mut hull = 40.0_f64;
        let mut to_host: Vec<(u64, Message)> = Vec::new();
        let mut to_shooter: Vec<(u64, Message)> = Vec::new();
        let mut claimed = false;
        let mut refusals = 0;
        // The host's hull per frame: the shooter's snapshots are one way old.
        let mut hulls: Vec<f64> = Vec::new();
        for f in 0..1200u64 {
            let seen = usize::try_from(f.saturating_sub(one_way))
                .ok()
                .and_then(|i| hulls.get(i).copied())
                .unwrap_or(40.0);
            // The shooter: 2 a frame for the first 15 s; its view at 0 claims
            // once (the claim rule: never again without a hull rise).
            if f < 900 {
                rep.add(8, 2.0, seen as f32);
            }
            if !claimed && rep.view(8, seen as f32) <= 0.0 {
                rep.claim_kill(8);
                claimed = true;
            }
            for m in rep.take_reports(ME, f) {
                if !rng.chance(loss) {
                    to_host.push((f + one_way, m));
                }
            }
            // The host.
            let (now, later): (Vec<_>, Vec<_>) = to_host.drain(..).partition(|(at, _)| *at <= f);
            to_host = later;
            for (_, m) in now {
                let Message::DamageReport(r) = m else {
                    continue;
                };
                for a in h.on_report(&r) {
                    let before = hull;
                    hull = (hull - f64::from(a.delta)).max(-1.0e9);
                    h.applied(&a, Some((before, hull)), 40.0, 786);
                    if a.claim > 0 && h.judge(&a, Some(hull)) == Verdict::Refuse {
                        to_shooter.push((f + one_way, refusal(&a, hull)));
                    }
                }
            }
            for m in h.acks(|_| Some(hull as f32), true) {
                if !rng.chance(loss) {
                    to_shooter.push((f + one_way, m));
                }
            }
            for (n, p, hl, c) in h.overdue(|_| Some(hull as f32)) {
                to_shooter.push((
                    f + one_way,
                    Message::KillRefused {
                        net_id: n,
                        peer: p,
                        hull: hl,
                        claim: c,
                    },
                ));
            }
            h.watch(8, hull as f32);
            hulls.push(hull);
            // The shooter's mail.
            let (now, later): (Vec<_>, Vec<_>) = to_shooter.drain(..).partition(|(at, _)| *at <= f);
            to_shooter = later;
            for (_, m) in now {
                match m {
                    Message::DamageAck(a) => rep.on_ack(ME, &a),
                    Message::KillRefused { net_id, claim, .. }
                        if claim == rep.last_claim(net_id) =>
                    {
                        refusals += 1;
                        rep.unclaim(net_id);
                    }
                    _ => {}
                }
            }
        }
        (h.counts(), refusals)
    }

    fn refusal(a: &Apply, hull: f64) -> Message {
        Message::KillRefused {
            net_id: a.net_id,
            peer: a.by,
            hull: hull as f32,
            claim: a.claim,
        }
    }

    #[test]
    fn a_claim_in_flight_is_never_a_claim_again_however_slow_the_link() {
        // 40 ms clean, 120 ms with 2 % loss, 300 ms with 10 % loss (one way).
        for (one_way, loss) in [(3, 0.0), (8, 0.02), (18, 0.1)] {
            let (n, refusals) = wall_over_a_link(one_way, loss);
            assert_eq!(
                (n.refused, n.refused_late, n.reclaims, refusals),
                (1, 1, 0, 1),
                "one way {one_way} frames, loss {loss}: {n:?}"
            );
            assert!(n.claims >= 1);
        }
    }

    #[test]
    fn a_claim_short_only_by_the_carried_hull_is_a_kill() {
        // An enemy at 31 of a max of 160 is seen on the joining side as
        // 30.99901 (the snapshot's 16-bit fraction); a report of that leaves
        // the host's at 0.00099, which is no hull the claimant could see. The
        // same for 13 of 80 seen as 12.99947.
        for (hull, max, reported) in [(31.0_f64, 160.0_f32, 30.99901_f32), (13.0, 80.0, 12.99947)] {
            let mut h = HostDamage::new();
            let applies = h.on_report(&DamageReport {
                by: PeerId(1),
                entries: vec![DamageEntry {
                    net_id: 7,
                    total: reported,
                    claim: 1,
                }],
            });
            let a = applies[0];
            let after = hull - f64::from(a.delta);
            assert!(after > 0.0 && after < 0.002, "{after}");
            assert!(
                h.carried_short(&a, after, max),
                "{after} within {}",
                carry_bound(max)
            );
            h.applied(&a, Some((hull, 0.0)), max, 300);
            assert_eq!(h.judge(&a, Some(0.0)), Verdict::Confirm);
            assert_eq!(h.counts().carried, 1);
            assert_eq!(h.counts().refused, 0);
            assert_eq!(h.counts().mismatched, 0);
        }
        // Not without a claim, and not a rest the claimant could see.
        let mut h = HostDamage::new();
        let a = Apply {
            net_id: 7,
            by: PeerId(1),
            delta: 25.0,
            claim: 1,
        };
        assert!(!h.carried_short(&a, 11.0, 160.0));
        assert!(!h.carried_short(&a, 0.003, 160.0));
        assert!(!h.carried_short(&Apply { claim: 0, ..a }, 0.0011, 160.0));
        assert!(!h.carried_short(&a, 0.0, 160.0), "dead already");
        assert_eq!(h.counts().carried, 0);
        // A large max carries more: 612,000 gives 4.67. Its rest written to
        // 0 is no mismatch; the same extra loss without a claim, or past the
        // bound, still is one.
        let max = 612_000.0;
        assert!(
            (carry_bound(max) - 4.670_0).abs() < 0.001,
            "{}",
            carry_bound(max)
        );
        let mut h = HostDamage::new();
        let a = Apply {
            net_id: 7,
            by: PeerId(1),
            delta: 999.0,
            claim: 1,
        };
        assert!(h.carried_short(&a, 1.0, max));
        h.applied(&a, Some((1000.0, 0.0)), max, 768);
        assert_eq!(h.counts().mismatched, 0);
        h.applied(&Apply { claim: 0, ..a }, Some((1000.0, 0.0)), max, 768);
        assert_eq!(h.counts().mismatched, 1, "no claim");
        h.applied(&a, Some((1010.0, 0.0)), max, 768);
        assert_eq!(h.counts().mismatched, 2, "past the bound");
    }

    /// A host with enemy 4 settled (past [`HONOUR_SETTLE_FRAMES`]).
    fn settled_host() -> HostDamage {
        let mut h = HostDamage::new();
        let _ = h.watch(2, 100.0);
        frames(&mut h, HONOUR_SETTLE_FRAMES);
        h
    }

    fn frames(h: &mut HostDamage, n: u64) {
        for _ in 0..n {
            let _ = h.acks(|_| None, true);
        }
    }

    /// The honour window at a 170 ms round trip: 12 frames.
    const W: Option<u64> = Some(12);

    #[test]
    fn the_honour_window_is_one_round_trip_and_a_snapshot_capped_at_250_ms() {
        let ms = |n| Some(std::time::Duration::from_millis(n));
        // 170 ms + 33 ms is 12.2 frames: a repair 12 frames before counts,
        // 13 does not.
        assert_eq!(honour_window(ms(170)), W);
        assert_eq!(
            honour_window(ms(150)),
            Some(10),
            "11 frames is 183 ms, past 150 + 33"
        );
        let mut h = settled_host();
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 12);
        assert!(
            h.honour(&JOINING_KILL, 20.0, 600.0, W).is_some(),
            "just inside"
        );
        let mut h = settled_host();
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 13);
        assert_eq!(
            h.honour(&JOINING_KILL, 20.0, 600.0, W),
            None,
            "just outside"
        );
        // No measure yet (no link, or none sampled): nothing counts.
        assert_eq!(honour_window(None), None);
        assert_eq!(honour_window(ms(0)), None);
        let mut h = settled_host();
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 2);
        assert_eq!(h.honour(&JOINING_KILL, 20.0, 600.0, None), None);
        assert_eq!(
            h.repairs_seen(2, None),
            RepairsSeen {
                counted: 0,
                sum: 0.0,
                uncounted: 1
            }
        );
        // The cap, 250 ms: a 400 ms round trip still counts no repair over
        // 15 frames before; one 200 ms before counts, one 300 ms before does
        // not.
        let wide = honour_window(ms(400));
        assert_eq!(wide, Some(HONOUR_CAP_FRAMES));
        assert_eq!(honour_window(ms(5000)), Some(HONOUR_CAP_FRAMES));
        let mut h = settled_host();
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 12);
        assert!(
            h.honour(&JOINING_KILL, 20.0, 600.0, wide).is_some(),
            "200 ms"
        );
        let mut h = settled_host();
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 18);
        assert_eq!(h.honour(&JOINING_KILL, 20.0, 600.0, wide), None, "300 ms");
    }

    /// The joining player's killing claim on enemy 2 (max 600): the hit
    /// took 100.
    const JOINING_KILL: Apply = Apply {
        net_id: 2,
        by: PeerId(2),
        delta: 100.0,
        claim: 1,
    };

    #[test]
    fn a_kill_racing_a_repair_the_claimer_could_not_see_is_honoured() {
        // +25 on the enemy, the claim 8 frames (136 ms) later left 20 of
        // 600.
        let mut h = settled_host();
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 8);
        let got = h.honour(&JOINING_KILL, 20.0, 600.0, W);
        assert_eq!(
            got,
            Some(Honour {
                rest: 20.0,
                repairs: 1,
                repaired: 25.0,
                window: 12
            })
        );
        // The glue writes 0: the rest is the lost repair, not a mismatch,
        // and the claim is an ordinary confirmed kill of the claimer's.
        h.applied(&JOINING_KILL, Some((120.0, 0.0)), 600.0, 301);
        assert_eq!(h.judge(&JOINING_KILL, Some(0.0)), Verdict::Confirm);
        assert_eq!(h.confirmed_claim(2), Some(PeerId(2)));
        let c = h.counts();
        assert_eq!((c.honoured, c.mismatched, c.refused), (1, 0, 0));
        assert!((c.lost_repair - 20.0).abs() < 1e-6, "{}", c.lost_repair);
        // Two repairs in the window add up.
        let mut h = settled_host();
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 10);
        h.repaired(2, 25.0, 600.0, false);
        assert!(h.honour(&JOINING_KILL, 40.0, 600.0, W).is_some());
    }

    #[test]
    fn a_kill_short_of_more_than_the_repairs_in_the_window_is_refused() {
        // No repair at all.
        let mut h = settled_host();
        assert_eq!(h.honour(&JOINING_KILL, 20.0, 600.0, W), None);
        // The repair more than the window before the claim.
        h.repaired(2, 25.0, 600.0, false);
        frames(&mut h, 12 + 1);
        assert_eq!(h.honour(&JOINING_KILL, 20.0, 600.0, W), None);
        assert_eq!(h.repairs_seen(2, W), RepairsSeen::default());
        // More left than the repairs gave back.
        h.repaired(2, 25.0, 600.0, false);
        assert_eq!(h.honour(&JOINING_KILL, 25.5, 600.0, W), None);
        assert_eq!(
            h.repairs_seen(2, W),
            RepairsSeen {
                counted: 1,
                sum: 25.0,
                uncounted: 0
            }
        );
        // No claim, or already dead: nothing to honour.
        assert_eq!(
            h.honour(
                &Apply {
                    claim: 0,
                    ..JOINING_KILL
                },
                10.0,
                600.0,
                W
            ),
            None
        );
        assert_eq!(h.honour(&JOINING_KILL, 0.0, 600.0, W), None);
        // The rest written off without an honour is still a mismatch.
        h.applied(&JOINING_KILL, Some((120.0, 0.0)), 600.0, 301);
        let c = h.counts();
        assert_eq!((c.honoured, c.mismatched), (0, 1));
        assert!(c.lost_repair.abs() < 1e-9);
    }

    #[test]
    fn refills_set_up_rises_and_bosses_are_never_honoured() {
        // A phase refill: one rise past the largest repair.
        let mut h = settled_host();
        h.repaired(2, 286.0, 9000.0, false);
        assert_eq!(h.honour(&JOINING_KILL, 10.0, 9000.0, W), None);
        // A repair that is a large share of the enemy (25 of 60).
        h.repaired(2, 25.0, 60.0, false);
        assert_eq!(h.honour(&JOINING_KILL, 10.0, 60.0, W), None);
        // A boss, or a boss's part.
        h.repaired(2, 25.0, 30_000.0, true);
        assert_eq!(h.honour(&JOINING_KILL, 10.0, 30_000.0, W), None);
        assert_eq!(
            h.repairs_seen(2, W),
            RepairsSeen {
                counted: 0,
                sum: 0.0,
                uncounted: 3
            }
        );
        // A rise in its set-up frames: enemy 5, just seen.
        let _ = h.watch(5, 50.0);
        frames(&mut h, HONOUR_SETTLE_FRAMES - 1);
        h.repaired(5, 25.0, 600.0, false);
        assert_eq!(
            h.honour(
                &Apply {
                    net_id: 5,
                    ..JOINING_KILL
                },
                10.0,
                600.0,
                W
            ),
            None
        );
        // Never seen ticking here: no set-up known, never counted.
        h.repaired(6, 25.0, 600.0, false);
        assert_eq!(
            h.honour(
                &Apply {
                    net_id: 6,
                    ..JOINING_KILL
                },
                10.0,
                600.0,
                W
            ),
            None
        );
        let c = h.counts();
        assert_eq!(
            (c.repairs_counted, c.repairs_uncounted, c.honoured),
            (0, 5, 0)
        );
        // Gone: its repairs go with it.
        h.repaired(2, 25.0, 600.0, false);
        h.gone(2, true, true);
        let _ = h.watch(2, 100.0);
        assert_eq!(h.repairs_seen(2, W), RepairsSeen::default());
        assert!(repair_counts(25.0, 100.0) && !repair_counts(25.0, 99.0));
        assert!(!repair_counts(25.5, 1000.0) && !repair_counts(0.0, 1000.0));
    }
}

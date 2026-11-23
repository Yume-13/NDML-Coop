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

//! The co-op session as the game thread sees it: links, the current run, the
//! start gate and the partners' ships. Pure: network events and game readings
//! in, [`Out`] messages, freeze decisions and draw items out.
//!
//! A run is the life of one `oController` (new instance or new
//! `timeGameStart`). A run that starts linked is a **co-op run**, a latch
//! that holds until it ends: save protection and the leaderboard block
//! follow it. A lost link is waited for ([`crate::wait`]); when the host is
//! gone for good the joining player's run is over ([`Core::run_over`]).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use ndml_net::endpoint::DisconnectReason;
use ndml_net::endpoint::Event;
use ndml_net::lobby::files::clean_name;
use ndml_net::lobby::refusal_message;
use ndml_net::msg::{Message, PeerId, PlayerLook};
use ndml_net::session::PeerSet;

use crate::gate::{FreezeAction, Freezer, GatePhase, StartGate};
use crate::net::Out;
use crate::overlay::{Line, RunView, run_lines};
use crate::player::{
    LOOK_INTERVAL, PowersRead, RemoteShip, STATE_INTERVAL, ShipRead, look_changed, pack_look,
    pack_look_frame, pack_state,
};
use crate::render::{DrawItem, Gui, LookParams, PartnerFrame, View, plan_partner};
use crate::wait::{LinkWait, PLAYS_ON_DEFAULT, WaitEvent, WaitInput, WaitView};

/// How long a one-off overlay message stays.
pub const NOTICE_FOR: Duration = Duration::from_secs(6);

/// A linked partner.
#[derive(Debug, Clone)]
pub struct Partner {
    /// Name from its `Hello`.
    pub name: String,
    /// Its ship.
    pub ship: RemoteShip,
}

/// The current run.
#[allow(clippy::struct_excessive_bools)] // independent flags of one run
#[derive(Debug, Clone, Default)]
struct Run {
    key: usize,
    stamp: Option<f64>,
    arrived: bool,
    coop: bool,
    /// Started co-op with the partner's link just lost: arrival at the gate
    /// waits until the partner is linked again.
    awaiting_partner: bool,
    ship_seen: bool,
    frame: u64,
    left: Option<String>,
    /// The partner left for good (a goodbye, a refusal): no wait.
    gone: bool,
    /// Joining game: the host is gone for good (it left for the menu or
    /// closed its game); this run is over (since).
    over: Option<Instant>,
}

/// How long the joining game shows why its run is over (paused) before it
/// leaves for the menu as Leave Co-op Run does.
pub const OVER_HOLD: Duration = Duration::from_secs(4);

/// What the run tick read from `oController` and the globals.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // independent readings of one frame
pub struct RunFrame {
    /// `oController` instance key.
    pub key: usize,
    /// `timeGameStart`.
    pub stamp: Option<f64>,
    /// `pause != 0`.
    pub paused: bool,
    /// `gameplay`.
    pub gameplay: bool,
    /// `gameOver`.
    pub game_over: bool,
    /// The arena.
    pub view: Option<View>,
    /// Game-mode hash and a readable summary (for the log).
    pub mode: (u32, String),
    /// While linked: how long nothing has arrived from the partner.
    pub silent_for: Option<Duration>,
    /// When this game last woke from its own stall (`wait::OWN_STALL`).
    pub woke: Option<Instant>,
    /// The upgrade screen is open here (the wait's pause comes when it closes).
    pub upgrade_screen: bool,
}

/// The local ship's send state.
#[derive(Debug, Clone, Default)]
struct Local {
    last_state: Option<Instant>,
    last_look: Option<PlayerLook>,
    last_look_at: Option<Instant>,
    resend_look: bool,
    powers: Option<PowersRead>,
    /// The local ship's sprite and look.
    own: Option<(u32, LookParams)>,
    /// The ship is down (the session rules), sent in `PlayerState.downed`.
    downed: bool,
    /// The ship's discontinuity segment (`PlayerState.seg`: a wrap, a respawn).
    segs: ndml_net::interp::Segments,
}

/// See the module docs.
#[derive(Debug)]
pub struct Core {
    extrapolation: Duration,
    me: Option<PeerId>,
    partners: BTreeMap<PeerId, Partner>,
    gate: StartGate,
    freezer: Freezer,
    run: Option<Run>,
    view: Option<View>,
    local: Local,
    notices: VecDeque<(String, Instant)>,
    /// Partners still linked that left this co-op run (`RunLeft`).
    left_run: BTreeSet<PeerId>,
    /// A run started without a partner: newcomers are turned away until it
    /// ends (no linking during a run, on either route).
    solo_refusing: bool,
    /// When the last partner's link timed out (cleared when one links).
    lost_at: Option<Instant>,
    /// The co-op run's wait for a lost link.
    wait: LinkWait,
    /// Wait events for the glue (the rules, the UI).
    wait_events: Vec<WaitEvent>,
    /// Host: how long it plays on alone after a loss.
    plays_on: Duration,
    /// The partner last linked (kept while it is away) and its name.
    last_partner: Option<(PeerId, String)>,
    /// Whether this game hosted when last linked.
    last_host: bool,
}

/// A run that starts this soon after the partner's link timed out is a
/// co-op run waiting for the partner (a long load can stall past the timeout).
pub const RUN_START_GRACE: Duration = Duration::from_secs(2);

fn look_params(l: &PlayerLook) -> LookParams {
    LookParams {
        xscale: f64::from(l.xscale),
        yscale: f64::from(l.yscale),
        blend: l.blend,
        alpha: f64::from(l.alpha),
        visible: l.visible,
        cloaked: l.cloaked,
    }
}

impl Core {
    /// A session with nobody linked. `extrapolation` caps remote ships.
    #[must_use]
    pub fn new(extrapolation: Duration) -> Self {
        Self {
            extrapolation,
            me: None,
            partners: BTreeMap::new(),
            gate: StartGate::default(),
            freezer: Freezer::default(),
            run: None,
            view: None,
            local: Local::default(),
            notices: VecDeque::new(),
            left_run: BTreeSet::new(),
            solo_refusing: false,
            lost_at: None,
            wait: LinkWait::default(),
            wait_events: Vec::new(),
            plays_on: PLAYS_ON_DEFAULT,
            last_partner: None,
            last_host: false,
        }
    }

    /// Host: how long its game plays on alone after a loss.
    pub fn set_plays_on(&mut self, d: Duration) {
        self.plays_on = d;
    }

    /// This game hosts (or hosted when last linked: kept through a wait).
    #[must_use]
    pub fn hosting(&self) -> bool {
        if self.linked() {
            self.me().is_host()
        } else {
            self.last_host
        }
    }

    /// Joining game: since when its co-op run is over (the host gone).
    #[must_use]
    pub fn run_over(&self) -> Option<Instant> {
        self.run.as_ref().and_then(|r| r.over)
    }

    /// The host is gone for good: the joining player's run is over.
    fn host_gone(&mut self, now: Instant) {
        let Some(run) = self.run.as_mut().filter(|r| r.coop && r.over.is_none()) else {
            return;
        };
        run.over = Some(now);
        self.wait.reset();
        self.note("the host left; this run is over".into(), now);
    }

    /// What the wait shows now.
    #[must_use]
    pub fn wait_view(&self, now: Instant) -> WaitView {
        self.wait.view(now)
    }

    /// The run waits for a lost link.
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.wait.waiting()
    }

    /// The link wait holds the game frozen.
    #[must_use]
    pub fn wait_hold(&self) -> bool {
        self.wait.hold()
    }

    /// The wait's events since the last call.
    pub fn take_wait_events(&mut self) -> Vec<WaitEvent> {
        std::mem::take(&mut self.wait_events)
    }

    /// The partner last linked (kept while it is away).
    #[must_use]
    pub fn last_partner(&self) -> Option<&(PeerId, String)> {
        self.last_partner.as_ref()
    }

    /// A co-op run whose partner is still in it.
    fn wait_active(&self) -> bool {
        let Some(run) = &self.run else { return false };
        let left = self
            .last_partner
            .as_ref()
            .is_some_and(|(p, _)| self.left_run.contains(p));
        run.coop && !run.gone && run.over.is_none() && !left
    }

    /// A partner is connected.
    #[must_use]
    pub fn linked(&self) -> bool {
        !self.partners.is_empty()
    }

    /// Own peer id: a client's if a host is linked, else the host's.
    #[must_use]
    pub fn me(&self) -> PeerId {
        if self.partners.contains_key(&PeerId::HOST) {
            self.me.filter(|p| !p.is_host()).unwrap_or(PeerId(1))
        } else {
            PeerId::HOST
        }
    }

    /// Whether this game hosts a link.
    #[must_use]
    pub fn is_host(&self) -> bool {
        self.linked() && self.me().is_host()
    }

    /// The partners.
    #[must_use]
    pub fn partners(&self) -> &BTreeMap<PeerId, Partner> {
        &self.partners
    }

    fn peers(&self) -> PeerSet {
        let mut s: PeerSet = self.partners.keys().copied().collect();
        s.insert(self.me());
        s
    }

    /// In a run (the controller ticked since the last menu frame).
    #[must_use]
    pub fn in_run(&self) -> bool {
        self.run.is_some()
    }

    /// A partner is linked and still in this run (none said `RunLeft`).
    #[must_use]
    pub fn run_linked(&self) -> bool {
        self.partners.keys().any(|p| !self.left_run.contains(p))
    }

    /// Whether `peer` left this co-op run while staying linked.
    #[must_use]
    pub fn left_run(&self, peer: PeerId) -> bool {
        self.left_run.contains(&peer)
    }

    /// The current run started linked.
    #[must_use]
    pub fn coop_run(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.coop)
    }

    /// The start gate.
    #[must_use]
    pub fn gate_phase(&self) -> GatePhase {
        self.gate.phase()
    }

    fn note(&mut self, text: String, now: Instant) {
        log::info!("co-op: {text}");
        self.notice(text, now);
    }

    /// A notice that names the partner; the log gets `logged` (no names).
    fn note_named(&mut self, text: String, logged: &str, now: Instant) {
        log::info!("co-op: {logged}");
        self.notice(text, now);
    }

    fn notice(&mut self, text: String, now: Instant) {
        if self.notices.len() >= 4 {
            self.notices.pop_front();
        }
        self.notices.push_back((text, now));
    }

    /// One network event. `now_ms`: the local wire clock (for `Go`).
    pub fn handle(&mut self, ev: Event, now: Instant, now_ms: u32) -> Vec<Out> {
        match ev {
            Event::Connected { peer, addr } => {
                if self.solo_refusing && !peer.is_host() {
                    // Hosting began mid-run: turned away as any newcomer
                    // during a run is.
                    self.note(
                        format!(
                            "turned away {}: this game is in a solo run",
                            ndml_net::steam::display_addr(&addr)
                        ),
                        now,
                    );
                    let why = ndml_net::endpoint::RefuseReason::RunInProgress;
                    return vec![Out::Refusing(Some(why)), Out::Refuse(peer, why)];
                }
                self.lost_at = None;
                self.partners.insert(
                    peer,
                    Partner {
                        name: "partner".into(),
                        ship: RemoteShip::new(self.extrapolation),
                    },
                );
                self.local.resend_look = true;
                self.note(
                    format!(
                        "linked with {peer} at {}",
                        ndml_net::steam::display_addr(&addr)
                    ),
                    now,
                );
                let mut outs = Vec::new();
                if !peer.is_host() {
                    self.me = Some(PeerId::HOST);
                    outs.push(Out::Send(peer, Message::Welcome { peer, peers: 2 }));
                }
                self.gate.set_peers(self.peers());
                // The host back in the shared run: readiness again.
                if peer.is_host() && self.coop_run() && !self.left_run.contains(&peer) {
                    outs.extend(self.gate.partner_back());
                }
                outs
            }
            Event::Disconnected { peer, reason } => self.on_left(peer, reason, now),
            Event::Message { from, msg } => self.on_message(from, msg, now, now_ms),
            Event::Refused { addr, reason } => {
                self.note(
                    format!(
                        "turned away {}: {reason}",
                        ndml_net::steam::display_addr(&addr)
                    ),
                    now,
                );
                Vec::new()
            }
        }
    }

    fn on_left(&mut self, peer: PeerId, reason: DisconnectReason, now: Instant) -> Vec<Out> {
        let Some(p) = self.partners.remove(&peer) else {
            return Vec::new();
        };
        // `left_run` lasts until this run ends (`leave_run`).
        let why = match reason {
            DisconnectReason::Refused(r) => refusal_message(r),
            other => other.to_string(),
        };
        let text = format!("{} left ({why})", p.name);
        if let Some(run) = &mut self.run {
            run.left = Some(text.clone());
        }
        if reason == DisconnectReason::Timeout && self.partners.is_empty() {
            self.lost_at = Some(now);
        }
        self.note_named(text, &format!("{peer} left ({why})"), now);
        let lost = matches!(
            reason,
            DisconnectReason::Timeout | DisconnectReason::HandshakeTimeout
        );
        if lost && self.coop_run() {
            // Waited for (`wait`): the gate keeps holding.
            return Vec::new();
        }
        if peer.is_host() {
            // Gone for good: the joining player's run is over.
            self.host_gone(now);
        }
        if let Some(run) = self.run.as_mut().filter(|r| r.coop) {
            run.gone = true;
        }
        self.gate.on_peer_left(self.peers());
        Vec::new()
    }

    fn on_message(&mut self, from: PeerId, msg: Message, now: Instant, now_ms: u32) -> Vec<Out> {
        match msg {
            Message::Hello(h) => {
                if let Some(p) = self.partners.get_mut(&from) {
                    p.name = clean_name(&h.name).unwrap_or_else(|| "partner".into());
                    // No name in the log.
                    log::info!(
                        "co-op: {from} said hello (build {}, mod version {})",
                        h.build,
                        h.mod_version
                    );
                }
            }
            Message::Welcome { peer, .. } => self.me = Some(peer),
            Message::PlayerState(ps) => {
                let key = if self.partners.contains_key(&ps.peer) {
                    ps.peer
                } else {
                    from
                };
                if let Some(p) = self.partners.get_mut(&key) {
                    p.ship.push(&ps, now);
                }
            }
            Message::PlayerLookFrame(f) => {
                let key = if self.partners.contains_key(&f.peer) {
                    f.peer
                } else {
                    from
                };
                if let Some(p) = self.partners.get_mut(&key) {
                    p.ship.push_look_frame(&f);
                }
            }
            Message::PlayerLook(l) => {
                let key = if self.partners.contains_key(&l.peer) {
                    l.peer
                } else {
                    from
                };
                if let Some(p) = self.partners.get_mut(&key) {
                    p.ship.set_look(l);
                }
            }
            Message::RunStart { peer, mode } => {
                if self.me().is_host() && self.left_run.contains(&from) {
                    // Its next run is held at its gate until this run ends.
                    return Vec::new();
                }
                if self.me().is_host() {
                    return self.gate.on_run_start(peer, mode, now_ms);
                }
            }
            Message::Go { .. } => {
                if !self.me().is_host() {
                    if self.gate.holding() {
                        self.note("everyone is in: go!".into(), now);
                    }
                    self.gate.on_go();
                }
            }
            Message::RunRefused { peer } if !self.me().is_host() => self.gate.on_refused(peer),
            // Only for the shared run (past the gate).
            Message::RunLeft { .. }
                if self.coop_run()
                    && self.gate.phase() == GatePhase::Running
                    && self.partners.contains_key(&from)
                    && !self.left_run.contains(&from) =>
            {
                self.left_run.insert(from);
                let name = self.partners.get(&from).map(|p| p.name.clone());
                let name = name.unwrap_or_default();
                if let Some(run) = &mut self.run {
                    run.left = Some(format!("{name} left the run"));
                }
                self.note_named(
                    format!("{name} left the run for the menu"),
                    &format!("{from} left the run for the menu"),
                    now,
                );
                if from.is_host() {
                    self.host_gone(now);
                }
            }
            _ => {} // not the session's
        }
        Vec::new()
    }

    /// A menu frame: any run is over.
    pub fn menu_frame(&mut self) -> Vec<Out> {
        if self.run.is_some() {
            self.leave_run()
        } else {
            Vec::new()
        }
    }

    fn leave_run(&mut self) -> Vec<Out> {
        let was_coop = self.run.take().is_some_and(|r| r.coop);
        self.freezer.reset();
        self.local.last_look = None;
        self.left_run.clear();
        let mut outs = self.gate.leave_run();
        if std::mem::take(&mut self.solo_refusing) && !outs.contains(&Out::Refusing(None)) {
            outs.push(Out::Refusing(None));
        }
        if was_coop && self.linked() {
            // The partner plays on alone (or is told at its gate).
            outs.push(Out::Broadcast(Message::RunLeft { peer: self.me() }));
        }
        if was_coop {
            outs.push(Out::HoldLink(false));
        }
        self.wait.reset();
        outs
    }

    /// A run-tick frame. Returns what to send and what to do to the game.
    pub fn run_frame(
        &mut self,
        f: &RunFrame,
        now: Instant,
        now_ms: u32,
    ) -> (Vec<Out>, FreezeAction) {
        let mut outs = Vec::new();
        let new_run = self.run.as_ref().is_none_or(|r| {
            r.key != f.key || (r.stamp.is_some() && f.stamp.is_some() && r.stamp != f.stamp)
        });
        if new_run {
            if self.run.is_some() {
                outs.extend(self.leave_run());
            }
            let awaiting = !self.linked()
                && self
                    .lost_at
                    .is_some_and(|t| now.saturating_duration_since(t) <= RUN_START_GRACE);
            self.run = Some(Run {
                key: f.key,
                stamp: f.stamp,
                coop: self.linked() || awaiting,
                awaiting_partner: awaiting,
                ..Run::default()
            });
            if self.linked() || awaiting {
                // The link is held for the whole co-op run.
                outs.push(Out::HoldLink(true));
            }
            if self.linked() {
                log::info!(
                    "co-op: a co-op run starts (saves and leaderboard uploads off until it ends)"
                );
            } else if awaiting {
                log::info!(
                    "co-op: a co-op run starts with the partner's link just lost; it waits for the partner at the start gate when they are back (saves and leaderboard uploads off until it ends)"
                );
            } else if !self.solo_refusing {
                // A solo run: nobody links into it.
                self.solo_refusing = true;
                outs.push(Out::Refusing(Some(
                    ndml_net::endpoint::RefuseReason::RunInProgress,
                )));
            }
        }
        self.view = f.view;
        let linked = self.linked();
        let me = self.me();
        let peers = self.peers();
        let mode = f.mode.0;
        let Some(run) = self.run.as_mut() else {
            return (outs, FreezeAction::None);
        };
        run.frame += 1;
        let frame = run.frame;
        let waiting = run.awaiting_partner && !linked;
        if !run.arrived && !waiting && !f.paused && !f.game_over && (f.gameplay || run.ship_seen) {
            run.arrived = true;
            if run.coop && linked {
                log::info!(
                    "co-op: at the start gate as {me} (mode {mode:#010x}: {})",
                    f.mode.1
                );
                outs.extend(self.gate.arrive(me, peers, mode, now, now_ms));
            } else {
                log::debug!("co-op: no start gate (solo run, or the partner left)");
            }
        }
        outs.extend(self.gate.tick(now));
        if linked {
            self.last_host = me.is_host();
            if let Some((&p, partner)) = self.partners.iter().next() {
                self.last_partner = Some((p, partner.name.clone()));
            }
        }
        let gate_hold = self.coop_run() && self.gate.holding();
        let ev = self.wait.update(&WaitInput {
            active: self.wait_active(),
            linked,
            silent_for: f.silent_for,
            is_host: if linked { me.is_host() } else { self.last_host },
            plays_on: self.plays_on,
            woke: f.woke,
            now,
        });
        if let Some(mut ev) = ev {
            log::info!("co-op: link wait {ev:?}");
            if ev == (WaitEvent::Back { long: true }) {
                if !gate_hold && self.freezer.adopt() {
                    // The freeze stays as a player's pause: the shared pause.
                    log::info!("co-op: frozen 10 s or more; the wait becomes the shared pause");
                } else {
                    // Not paused by the wait (on the upgrade screen, or held
                    // by the start gate): an ordinary return.
                    log::info!("co-op: not frozen by the wait here; an ordinary return");
                    ev = WaitEvent::Back { long: false };
                }
            }
            self.wait_events.push(ev);
        }
        // A joining game whose run is over stands paused until it leaves.
        let over = self.run.as_ref().is_some_and(|r| r.over.is_some());
        let hold = gate_hold || ((over || self.wait.hold()) && !f.upgrade_screen);
        let action = self.freezer.decide(hold, f.paused, frame);
        (outs, action)
    }

    /// The local ship is down (or not): sent in `PlayerState.downed`.
    pub fn set_downed(&mut self, downed: bool) {
        self.local.downed = downed;
    }

    /// Hull and flags from `oPowers` this frame.
    pub fn set_powers(&mut self, p: PowersRead) {
        self.local.powers = Some(p);
    }

    /// The local ship's step: what to send about it.
    pub fn local_ship(
        &mut self,
        ship: &ShipRead,
        steps_per_sec: f64,
        time_ms: u32,
        now: Instant,
    ) -> Vec<Out> {
        if let Some(run) = &mut self.run {
            run.ship_seen = true;
        }
        // The f32 casts keep game-pixel positions.
        let seg = self
            .local
            .segs
            .observe(ndml_net::interp::Vec2::new(ship.x as f32, ship.y as f32));
        let look = pack_look(self.me(), ship, self.local.powers.as_ref());
        if let Some(s) = ship.sprite {
            self.local.own = Some((s, look_params(&look)));
        }
        if !(self.linked() && self.coop_run()) {
            return Vec::new();
        }
        let mut outs = Vec::new();
        if self
            .local
            .last_state
            .is_none_or(|t| now.saturating_duration_since(t) >= STATE_INTERVAL)
        {
            self.local.last_state = Some(now);
            let mut ps = pack_state(
                self.me(),
                time_ms,
                ship,
                self.local.powers.as_ref(),
                steps_per_sec,
            );
            ps.downed = self.local.downed;
            ps.seg = seg;
            outs.push(Out::Broadcast(Message::PlayerState(ps)));
            // The continuous look goes with every tick (about 20 bytes).
            outs.push(Out::Broadcast(Message::PlayerLookFrame(pack_look_frame(
                &look, time_ms,
            ))));
        }
        let due = self
            .local
            .last_look_at
            .is_none_or(|t| now.saturating_duration_since(t) >= LOOK_INTERVAL);
        let changed = self
            .local
            .last_look
            .is_none_or(|old| look_changed(&old, &look));
        if due && (changed || self.local.resend_look) {
            self.local.last_look = Some(look);
            self.local.last_look_at = Some(now);
            self.local.resend_look = false;
            outs.push(Out::Broadcast(Message::PlayerLook(look)));
        }
        outs
    }

    /// Draw items for every fresh partner ship, once the start gate let the
    /// run go. `screen` maps world to GUI; `render` gives each peer's
    /// partner-clock draw time (`None`: the newest state).
    pub fn draw_partners(
        &mut self,
        now: Instant,
        gui: Gui,
        screen: Option<View>,
        line_h: f64,
        render: &dyn Fn(PeerId) -> Option<Duration>,
    ) -> Vec<DrawItem> {
        if !(self.linked() && self.coop_run()) || self.gate.phase() != GatePhase::Running {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (&peer, p) in &mut self.partners {
            // Out of this run (`RunLeft`): nothing of it here any more.
            if self.left_run.contains(&peer) || !p.ship.is_fresh(now) {
                continue;
            }
            let Some(latest) = p.ship.latest().copied() else {
                continue;
            };
            if latest.downed {
                // Down (the session rules): no ship, label or marker.
                continue;
            }
            let t = render(peer).unwrap_or_else(|| ndml_net::time::from_wire_ms(latest.time_ms));
            let Some(pose) = p.ship.pose(t, now) else {
                continue;
            };
            let mut look = p.ship.look().map_or_default(look_params);
            // The continuous look at the same render time as the pose.
            if let Some(l) = p.ship.look_at(t) {
                look.xscale = f64::from(l.xscale);
                look.yscale = f64::from(l.yscale);
                look.alpha = f64::from(l.alpha);
                look.blend = l.blend;
            }
            let sprite = (latest.sprite != u16::MAX).then_some(u32::from(latest.sprite));
            out.extend(plan_partner(
                &PartnerFrame {
                    pos: pose.pos,
                    angle: pose.angle,
                    sprite,
                    frame: latest.frame,
                    look,
                    name: &p.name,
                },
                screen,
                gui,
                line_h,
            ));
        }
        out
    }

    /// The run overlay lines. `rtt_ms` is the link's round trip.
    #[must_use]
    pub fn run_overlay(&self, now: Instant, rtt_ms: Option<f64>) -> Vec<Line> {
        let notices: Vec<&str> = self
            .notices
            .iter()
            .filter(|(_, at)| now.saturating_duration_since(*at) < NOTICE_FOR)
            .map(|(t, _)| t.as_str())
            .collect();
        let view = RunView {
            linked: self.linked(),
            coop_run: self.coop_run(),
            phase: Some(self.gate.phase()),
            partner: self.partners.values().next().map(|p| p.name.as_str()),
            rtt_ms,
            left: self.run.as_ref().and_then(|r| r.left.as_deref()),
            notices,
        };
        run_lines(&view)
    }

    /// The partners, for the shared world (`world::partner_views`).
    pub fn partners_mut(&mut self) -> &mut BTreeMap<PeerId, Partner> {
        &mut self.partners
    }

    /// The partners still in this run (none who said `RunLeft`).
    pub fn run_partners_mut(&mut self) -> impl Iterator<Item = (&PeerId, &mut Partner)> {
        let left = &self.left_run;
        self.partners
            .iter_mut()
            .filter(move |(p, _)| !left.contains(p))
    }

    /// The arena from the last run frame.
    #[must_use]
    pub fn view(&self) -> Option<View> {
        self.view
    }

    /// The local ship's sprite and look.
    #[must_use]
    pub fn own_look(&self) -> Option<(u32, LookParams)> {
        self.local.own
    }

    /// The local `oPowers` reading (cloak, hull).
    #[must_use]
    pub fn local_powers(&self) -> Option<PowersRead> {
        self.local.powers
    }

    /// The start-gate banner lines (empty unless held in a linked co-op run).
    #[must_use]
    pub fn gate_banner(&self) -> Vec<Line> {
        if !(self.linked() && self.coop_run()) {
            return Vec::new();
        }
        crate::overlay::gate_banner(
            self.gate.phase(),
            self.partners.values().next().map(|p| p.name.as_str()),
        )
    }

    /// `(scaling, save_protect)` for the hot hooks.
    #[must_use]
    pub fn switches(&self) -> (bool, bool) {
        crate::scaling::switches(self.run_linked(), self.in_run(), self.coop_run())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::msg::{Hello, PlayerState};

    const HOST: PeerId = PeerId::HOST;
    const CLIENT: PeerId = PeerId(1);

    fn addr() -> std::net::SocketAddr {
        "10.0.0.2:7777".parse().unwrap()
    }

    fn frame(key: usize, paused: bool, gameplay: bool) -> RunFrame {
        RunFrame {
            key,
            stamp: Some(1000.0),
            paused,
            gameplay,
            game_over: false,
            view: Some(View {
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            }),
            mode: (0xAB, "gameModEndless=0".into()),
            silent_for: Some(Duration::ZERO),
            woke: None,
            upgrade_screen: false,
        }
    }

    fn ship() -> ShipRead {
        ShipRead {
            x: 10.0,
            y: 20.0,
            hspeed: 1.0,
            vspeed: 0.0,
            angle: 0.0,
            sprite: Some(5),
            frame: 0.0,
            xscale: 1.0,
            yscale: 1.0,
            blend: 16_777_215.0,
            alpha: 1.0,
            visible: true,
        }
    }

    fn hello(name: &str, build: &str) -> Message {
        Message::Hello(Hello {
            mod_version: 1,
            build: build.into(),
            name: name.into(),
        })
    }

    fn msg(from: PeerId, msg: Message) -> Event {
        Event::Message { from, msg }
    }

    /// Wire tags 40 to 43 from a partner: the receive path (`world::route`,
    /// the rules, this core) answers none of them and changes nothing.
    #[test]
    fn report_messages_from_a_partner_are_ignored() {
        use ndml_net::msg::{ReportDigest, ReportInfo, ReportMark, ReportTransfer};
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        let mut rules = crate::rules::Rules::new(crate::rules::RulesSettings::default());
        let linked = Event::Connected {
            peer: CLIENT,
            addr: addr(),
        };
        assert!(!c.handle(linked, t, 0).is_empty(), "the host welcomes");
        let before = (c.me(), c.linked(), c.coop_run(), c.switches());
        let reports = [
            Message::ReportInfo(ReportInfo {
                peer: CLIENT,
                kind: ReportInfo::HELLO,
                text: "session=1".into(),
            }),
            Message::ReportMark(ReportMark {
                peer: CLIENT,
                id: 1,
                kind: ReportMark::KEY,
                time_ms: 5,
                note: String::new(),
            }),
            Message::ReportDigest(ReportDigest {
                peer: CLIENT,
                time_ms: 5,
                flags: 0,
                wave: 1,
                enemies: 3,
                ids_hash: 7,
                frame_max_ms: 16,
            }),
            Message::ReportTransfer(ReportTransfer::Request),
        ];
        for m in reports {
            let routed = crate::world::route(msg(CLIENT, m));
            assert!(matches!(
                routed,
                crate::world::Routed::Session(Event::Message { .. }, None)
            ));
            if let crate::world::Routed::Session(ev, _) = routed {
                if let Event::Message { from, msg } = &ev {
                    assert!(!rules.on_message(*from, msg, t));
                }
                assert!(c.handle(ev, t, 0).is_empty());
            }
        }
        assert!(rules.take_outs().is_empty());
        assert_eq!((c.me(), c.linked(), c.coop_run(), c.switches()), before);
    }

    #[test]
    fn solo_runs_never_hold_or_send() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        let (outs, act) = c.run_frame(&frame(1, false, true), t, 0);
        // A solo run turns newcomers away until it ends.
        assert_eq!(
            outs,
            vec![Out::Refusing(Some(
                ndml_net::endpoint::RefuseReason::RunInProgress
            ))]
        );
        let (outs, _) = c.run_frame(&frame(1, false, true), t, 0);
        assert!(outs.is_empty(), "once");
        assert_eq!(act, FreezeAction::None);
        assert!(!c.coop_run());
        assert!(c.local_ship(&ship(), 60.0, 0, t).is_empty());
        assert_eq!(c.switches(), (false, false));
        assert!(c.run_overlay(t, None).is_empty());
        assert_eq!(
            c.menu_frame(),
            vec![Out::Refusing(None)],
            "the menu admits again"
        );
    }

    /// Hosting began after the solo run started: the core turns the newcomer away.
    #[test]
    fn a_newcomer_during_a_solo_run_is_turned_away() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        let _ = c.run_frame(&frame(1, false, true), t, 0);
        let why = ndml_net::endpoint::RefuseReason::RunInProgress;
        let outs = c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        assert_eq!(
            outs,
            vec![Out::Refusing(Some(why)), Out::Refuse(CLIENT, why)]
        );
        assert!(!c.linked());
        // Back in the menu, a newcomer links as usual.
        assert_eq!(c.menu_frame(), vec![Out::Refusing(None)]);
        let outs = c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        assert!(c.linked());
        assert!(matches!(
            outs.as_slice(),
            [Out::Send(CLIENT, Message::Welcome { .. })]
        ));
    }

    #[test]
    fn host_flow_welcome_hold_go_and_refusing() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        let outs = c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        assert_eq!(
            outs,
            vec![Out::Send(
                CLIENT,
                Message::Welcome {
                    peer: CLIENT,
                    peers: 2
                }
            )]
        );
        c.handle(msg(CLIENT, hello("Wing", "pe-1-2")), t, 0);
        assert_eq!(c.partners()[&CLIENT].name, "Wing");
        assert!(c.is_host());
        // Menu → loadout → first controller frame (paused intro), then gameplay.
        let (outs, act) = c.run_frame(&frame(7, true, false), t, 0);
        assert_eq!(outs, vec![Out::HoldLink(true)], "the link held for the run");
        assert_eq!(act, FreezeAction::None);
        let (outs, act) = c.run_frame(&frame(7, false, true), t, 10);
        assert!(outs.is_empty(), "waiting for the client");
        assert_eq!(act, FreezeAction::Pause);
        assert!(c.coop_run());
        assert_eq!(c.switches(), (true, true));
        assert!(c.run_overlay(t, None)[0].0.contains("waiting for Wing"));
        let outs = c.handle(
            msg(
                CLIENT,
                Message::RunStart {
                    peer: CLIENT,
                    mode: 0xAB,
                },
            ),
            t,
            20,
        );
        assert!(outs.contains(&Out::Broadcast(Message::Go { start_ms: 20 })));
        assert!(outs.contains(&Out::Refusing(Some(
            ndml_net::endpoint::RefuseReason::RunInProgress
        ))));
        let (_, act) = c.run_frame(&frame(7, true, false), t, 30);
        assert_eq!(act, FreezeAction::Unpause);
        // Ship states flow at 60 Hz, looks once (the reliable look and the
        // first continuous look frame).
        let outs = c.local_ship(&ship(), 60.0, 40, t);
        assert_eq!(outs.len(), 3);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Out::Broadcast(Message::PlayerLookFrame(_))))
        );
        assert!(
            c.local_ship(&ship(), 60.0, 41, t).is_empty(),
            "rate limited"
        );
        // Back at the menu: newcomers admitted again, the latch is cleared,
        // and the partner is told this game left the run.
        assert_eq!(
            c.menu_frame(),
            vec![
                Out::Refusing(None),
                Out::Broadcast(Message::RunLeft { peer: HOST }),
                Out::HoldLink(false)
            ]
        );
        assert_eq!(c.switches(), (false, false));
    }

    #[test]
    fn client_flow_and_partner_leaving_mid_run() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        assert!(
            c.handle(
                Event::Connected {
                    peer: HOST,
                    addr: addr()
                },
                t,
                0
            )
            .is_empty()
        );
        c.handle(
            msg(
                HOST,
                Message::Welcome {
                    peer: PeerId(1),
                    peers: 2,
                },
            ),
            t,
            0,
        );
        assert_eq!(c.me(), CLIENT);
        let (outs, act) = c.run_frame(&frame(3, false, true), t, 0);
        assert_eq!(
            outs,
            vec![
                Out::HoldLink(true),
                Out::Send(
                    HOST,
                    Message::RunStart {
                        peer: CLIENT,
                        mode: 0xAB
                    }
                )
            ]
        );
        assert_eq!(act, FreezeAction::Pause);
        c.handle(msg(HOST, Message::Go { start_ms: 5 }), t, 0);
        assert_eq!(c.gate_phase(), GatePhase::Running);
        c.handle(
            Event::Disconnected {
                peer: HOST,
                reason: DisconnectReason::Timeout,
            },
            t,
            0,
        );
        assert!(!c.linked());
        assert_eq!(c.switches(), (false, true), "saves stay off for this run");
        assert!(c.run_overlay(t, None)[0].0.contains("left (timed out)"));
        assert!(c.waiting() || c.coop_run(), "waited for");
        assert_eq!(c.menu_frame(), vec![Out::HoldLink(false)]);
        assert_eq!(c.switches(), (false, false), "solo again at the menu");
    }

    /// The joining game stalls past the link's timeout on its run's first
    /// frame: the run is still co-op, arrives at its gate when the host is
    /// back, and the host says go.
    #[test]
    fn a_run_started_as_the_link_times_out_waits_for_the_partner() {
        let t = Instant::now();
        let link = |c: &mut Core, peer| c.handle(Event::Connected { peer, addr: addr() }, t, 0);
        let gone = |peer| Event::Disconnected {
            peer,
            reason: DisconnectReason::Timeout,
        };
        let mut host = Core::new(Duration::from_millis(250));
        let mut cl = Core::new(Duration::from_millis(250));
        link(&mut host, CLIENT);
        link(&mut cl, HOST);
        cl.handle(
            msg(
                HOST,
                Message::Welcome {
                    peer: CLIENT,
                    peers: 2,
                },
            ),
            t,
            0,
        );
        // The host at its gate; the client stalls; both time out. The host's
        // gate waits for the client (nothing lets it go by itself).
        let _ = host.run_frame(&frame(7, false, true), t, 0);
        host.handle(gone(CLIENT), t, 0);
        assert_eq!(host.gate_phase(), GatePhase::Holding);
        let (_, act) = host.run_frame(&frame(7, false, true), t + Duration::from_secs(600), 0);
        assert_eq!(act, FreezeAction::None, "still held (paused once already)");
        assert_eq!(host.gate_phase(), GatePhase::Holding);
        let t1 = t + Duration::from_secs(12);
        cl.handle(gone(HOST), t1, 0);
        // The client's run starts in that frame: co-op, no refusal, no gate yet.
        let (outs, _) = cl.run_frame(&frame(3, false, true), t1, 0);
        assert!(cl.coop_run());
        assert!(
            !outs.iter().any(|o| matches!(o, Out::Refusing(Some(_)))),
            "{outs:?}"
        );
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Out::Send(_, Message::RunStart { .. }))),
            "no gate while the host is away: {outs:?}"
        );
        // Back a moment later: the client arrives; the host says go.
        link(&mut host, CLIENT);
        let t2 = t1 + Duration::from_millis(30);
        cl.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t2,
            0,
        );
        let (outs, _) = cl.run_frame(&frame(3, false, true), t2, 0);
        let start: Vec<_> = outs
            .into_iter()
            .filter(|o| matches!(o, Out::Send(HOST, Message::RunStart { .. })))
            .collect();
        assert_eq!(start.len(), 1, "the client arrives at its gate");
        let mut go = Vec::new();
        for o in start {
            if let Out::Send(HOST, m) = o {
                go.extend(host.handle(msg(CLIENT, m), t2, 5));
            }
        }
        assert!(
            go.iter()
                .any(|o| matches!(o, Out::Broadcast(Message::Go { .. }))),
            "{go:?}"
        );
        assert_eq!(host.gate_phase(), GatePhase::Running);
        // A run started long after the link went is solo.
        let mut late = Core::new(Duration::from_millis(250));
        link(&mut late, HOST);
        late.handle(gone(HOST), t, 0);
        let (outs, _) = late.run_frame(&frame(3, false, true), t + Duration::from_secs(5), 0);
        assert!(!late.coop_run());
        assert!(outs.iter().any(|o| matches!(o, Out::Refusing(Some(_)))));
    }

    /// A cut at the start gate holds both; the repeated `RunStart` lets both go.
    #[test]
    fn a_cut_at_the_start_gate_holds_both_until_the_link_is_back() {
        let t = Instant::now();
        let link = |c: &mut Core, peer| c.handle(Event::Connected { peer, addr: addr() }, t, 0);
        let gone = |peer| Event::Disconnected {
            peer,
            reason: DisconnectReason::Timeout,
        };
        let mut host = Core::new(Duration::from_millis(250));
        let mut cl = Core::new(Duration::from_millis(250));
        link(&mut host, CLIENT);
        link(&mut cl, HOST);
        cl.handle(
            msg(
                HOST,
                Message::Welcome {
                    peer: CLIENT,
                    peers: 2,
                },
            ),
            t,
            0,
        );
        // The client at its gate; its RunStart reaches the host in the menu.
        let (outs, _) = cl.run_frame(&frame(3, false, true), t, 0);
        for o in outs {
            if let Out::Send(HOST, m) = o {
                host.handle(msg(CLIENT, m), t, 0);
            }
        }
        // The cut: both time out; the client stays held at its gate.
        host.handle(gone(CLIENT), t, 0);
        cl.handle(gone(HOST), t, 0);
        assert_eq!(cl.gate_phase(), GatePhase::Holding);
        let (_, act) = cl.run_frame(&frame(3, true, true), t + Duration::from_secs(300), 0);
        assert_eq!(act, FreezeAction::None, "held on its pause screen");
        // Back; the client's repeat says it is in again.
        link(&mut host, CLIENT);
        link(&mut cl, HOST);
        let (outs, _) = cl.run_frame(&frame(3, true, true), t + Duration::from_secs(303), 0);
        assert!(
            outs.contains(&Out::Send(
                HOST,
                Message::RunStart {
                    peer: CLIENT,
                    mode: 0xAB
                }
            )),
            "{outs:?}"
        );
        for o in outs {
            if let Out::Send(HOST, m) = o {
                host.handle(msg(CLIENT, m), t, 0);
            }
        }
        // The host reaches its gate: it goes at once.
        let (outs, _) = host.run_frame(&frame(7, false, true), t, 10);
        assert!(
            outs.contains(&Out::Broadcast(Message::Go { start_ms: 10 })),
            "{outs:?}"
        );
        assert_ne!(host.gate_phase(), GatePhase::Holding);
    }

    /// A partner who quits to the menu says `RunLeft`; its next run waits at
    /// its gate until this one is over.
    #[test]
    fn a_partner_who_leaves_the_run_is_out_of_it_and_waits_for_the_next() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        c.run_frame(&frame(7, false, true), t, 0);
        let start = Message::RunStart {
            peer: CLIENT,
            mode: 0xAB,
        };
        assert!(
            !c.handle(msg(CLIENT, start.clone()), t, 10).is_empty(),
            "go"
        );
        assert!(c.run_linked());
        assert_eq!(c.switches(), (true, true));
        // The client quits to its menu.
        let out = c.handle(msg(CLIENT, Message::RunLeft { peer: CLIENT }), t, 20);
        assert!(out.is_empty());
        assert!(c.left_run(CLIENT) && c.linked() && !c.run_linked());
        assert_eq!(
            c.switches(),
            (false, true),
            "alone: no scaling; saves stay off"
        );
        // Its next run: held (no Go) meanwhile.
        assert!(c.handle(msg(CLIENT, start.clone()), t, 30).is_empty());
        // This run ends: RunLeft goes out; the next gate takes the repeat.
        assert_eq!(
            c.menu_frame(),
            vec![
                Out::Refusing(None),
                Out::Broadcast(Message::RunLeft { peer: HOST }),
                Out::HoldLink(false)
            ]
        );
        assert!(!c.left_run(CLIENT));
        let mut f = frame(8, false, true);
        f.stamp = Some(5000.0);
        c.run_frame(&f, t, 40);
        let outs = c.handle(msg(CLIENT, start), t, 50);
        assert!(outs.contains(&Out::Broadcast(Message::Go { start_ms: 50 })));
        // A client held at its gate: the host's RunLeft is for the run before.
        let mut cl = Core::new(Duration::from_millis(250));
        cl.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t,
            0,
        );
        cl.handle(
            msg(
                HOST,
                Message::Welcome {
                    peer: CLIENT,
                    peers: 2,
                },
            ),
            t,
            0,
        );
        cl.run_frame(&frame(3, false, true), t, 0);
        assert_eq!(cl.gate_phase(), GatePhase::Holding);
        cl.handle(msg(HOST, Message::RunLeft { peer: HOST }), t, 0);
        assert!(!cl.left_run(HOST) && cl.run_linked(), "not this run's");
        // A solo run's end says nothing to anyone (it admits newcomers again).
        let mut solo = Core::new(Duration::from_millis(250));
        solo.run_frame(&frame(1, false, true), t, 0);
        assert_eq!(solo.menu_frame(), vec![Out::Refusing(None)]);
    }

    #[test]
    fn a_new_controller_is_a_new_run() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        c.run_frame(&frame(1, false, true), t, 0);
        assert_eq!(c.gate_phase(), GatePhase::Holding);
        let mut f = frame(1, true, false);
        f.stamp = Some(2000.0);
        c.run_frame(&f, t, 0);
        assert_eq!(c.gate_phase(), GatePhase::Idle, "restart: back to the gate");
    }

    #[test]
    fn the_downed_flag_is_carried_in_player_state() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        c.run_frame(&frame(1, false, true), t, 0);
        let downed = |outs: &[Out]| {
            outs.iter().find_map(|o| match o {
                Out::Broadcast(Message::PlayerState(ps)) => Some(ps.downed),
                _ => None,
            })
        };
        assert_eq!(downed(&c.local_ship(&ship(), 60.0, 0, t)), Some(false));
        c.set_downed(true);
        let later = t + STATE_INTERVAL;
        assert_eq!(downed(&c.local_ship(&ship(), 60.0, 1, later)), Some(true));
    }

    #[test]
    fn partners_are_drawn_once_fresh_and_in_a_coop_run() {
        let t = Instant::now();
        let gui = Gui {
            w: 1920.0,
            h: 1080.0,
        };
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        c.handle(msg(CLIENT, hello("Wing", "pe-1-2")), t, 0);
        let ps = PlayerState {
            seg: 0,
            peer: CLIENT,
            time_ms: 100,
            x: 500.0,
            y: 400.0,
            vx: 0.0,
            vy: 0.0,
            angle: 10.0,
            sprite: 9,
            frame: 1,
            firing: false,
            hull: 50.0,
            downed: false,
        };
        c.handle(msg(CLIENT, Message::PlayerState(ps)), t, 0);
        assert!(
            c.draw_partners(t, gui, None, 16.0, &|_| None).is_empty(),
            "not a co-op run yet"
        );
        c.run_frame(&frame(1, false, true), t, 0);
        assert_eq!(c.gate_phase(), GatePhase::Holding);
        assert!(
            c.draw_partners(t, gui, None, 16.0, &|_| Some(Duration::from_millis(100)))
                .is_empty(),
            "fresh, but the gate holds: not drawn (a partner in another run)"
        );
        let start = Message::RunStart {
            peer: CLIENT,
            mode: 0xAB,
        };
        let _ = c.handle(msg(CLIENT, start), t, 10);
        assert_eq!(c.gate_phase(), GatePhase::Running);
        let items = c.draw_partners(t, gui, None, 16.0, &|_| Some(Duration::from_millis(100)));
        assert!(
            items
                .iter()
                .any(|i| matches!(i, DrawItem::Sprite(s) if s.sprite == 9))
        );
        assert!(
            items
                .iter()
                .any(|i| matches!(i, DrawItem::Text { text, .. } if text == "Wing"))
        );
        let down = PlayerState {
            time_ms: 116,
            downed: true,
            ..ps
        };
        c.handle(msg(CLIENT, Message::PlayerState(down)), t, 0);
        assert!(
            c.draw_partners(t, gui, None, 16.0, &|_| None).is_empty(),
            "a downed partner is not drawn"
        );
        let later = t + Duration::from_secs(2);
        assert!(
            c.draw_partners(later, gui, None, 16.0, &|_| None)
                .is_empty(),
            "stale"
        );
    }

    /// A partner who left the run for the menu is not drawn until the next run.
    #[test]
    fn a_partner_who_left_the_run_is_not_drawn_until_the_next() {
        let t = Instant::now();
        let gui = Gui {
            w: 1920.0,
            h: 1080.0,
        };
        let colors = crate::world::color::ColorSync::default();
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        c.run_frame(&frame(7, false, true), t, 0);
        let start = Message::RunStart {
            peer: CLIENT,
            mode: 0xAB,
        };
        let _ = c.handle(msg(CLIENT, start.clone()), t, 10);
        let ps = PlayerState {
            seg: 0,
            peer: CLIENT,
            time_ms: 100,
            x: 500.0,
            y: 400.0,
            vx: 0.0,
            vy: 0.0,
            angle: 10.0,
            sprite: 9,
            frame: 1,
            firing: false,
            hull: 50.0,
            downed: false,
        };
        c.handle(msg(CLIENT, Message::PlayerState(ps)), t, 0);
        let render = |_| Some(Duration::from_millis(100));
        let drawn = |c: &mut Core| !c.draw_partners(t, gui, None, 16.0, &render).is_empty();
        let viewed = |c: &mut Core| {
            crate::world::partner_views(c, t, &render, &colors)
                .iter()
                .any(|v| v.peer == CLIENT)
        };
        assert!(drawn(&mut c) && viewed(&mut c), "in the run");
        let _ = c.handle(msg(CLIENT, Message::RunLeft { peer: CLIENT }), t, 20);
        assert!(c.left_run(CLIENT) && c.linked());
        assert!(!drawn(&mut c), "left: not drawn");
        assert!(!viewed(&mut c), "left: no world view (no avatar)");
        // Run over; the next run is co-op again and the partner is in it.
        let _ = c.menu_frame();
        let mut f = frame(8, false, true);
        f.stamp = Some(5000.0);
        c.run_frame(&f, t, 40);
        let _ = c.handle(msg(CLIENT, start), t, 50);
        let back = PlayerState { time_ms: 116, ..ps };
        c.handle(msg(CLIENT, Message::PlayerState(back)), t, 50);
        assert!(
            drawn(&mut c) && viewed(&mut c),
            "drawn again in the next run"
        );
    }

    /// A linked co-op run past its gate, with `partner` linked.
    fn running(partner: PeerId, t: Instant) -> Core {
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: partner,
                addr: addr(),
            },
            t,
            0,
        );
        c.handle(msg(partner, hello("Sam", "pe-1-2")), t, 0);
        if partner.is_host() {
            c.handle(
                msg(
                    HOST,
                    Message::Welcome {
                        peer: CLIENT,
                        peers: 2,
                    },
                ),
                t,
                0,
            );
            c.run_frame(&frame(3, false, true), t, 0);
            c.handle(msg(HOST, Message::Go { start_ms: 1 }), t, 0);
        } else {
            c.run_frame(&frame(3, false, true), t, 0);
            c.handle(
                msg(
                    CLIENT,
                    Message::RunStart {
                        peer: CLIENT,
                        mode: 0xAB,
                    },
                ),
                t,
                0,
            );
        }
        assert_eq!(c.gate_phase(), GatePhase::Running);
        // The gate's pause let go.
        let (_, act) = c.run_frame(&frame(3, true, true), t, 0);
        assert_eq!(act, FreezeAction::Unpause);
        c
    }

    /// One frame `s` seconds in, `silent` seconds of silence (while linked).
    fn at(c: &mut Core, t: Instant, s: f64, paused: bool, silent: f64) -> (Vec<Out>, FreezeAction) {
        let mut f = frame(3, paused, true);
        f.silent_for = c.linked().then(|| Duration::from_secs_f64(silent));
        c.run_frame(&f, t + Duration::from_secs_f64(s), 0)
    }

    fn timeout(c: &mut Core, peer: PeerId, t: Instant) {
        c.handle(
            Event::Disconnected {
                peer,
                reason: DisconnectReason::Timeout,
            },
            t,
            0,
        );
    }

    #[test]
    fn a_blip_plays_through_and_an_eight_second_cut_freezes_the_joining_game() {
        let t = Instant::now();
        let mut c = running(HOST, t);
        let (_, act) = at(&mut c, t, 12.0, false, 2.0);
        assert_eq!(act, FreezeAction::None, "a blip plays through");
        assert!(c.take_wait_events().is_empty());
        let (_, act) = at(&mut c, t, 13.0, false, 3.0);
        assert_eq!(act, FreezeAction::Pause, "the joining game freezes");
        assert!(!c.hosting(), "the joining game");
        assert_eq!(c.take_wait_events(), vec![WaitEvent::Lost]);
        assert_eq!(c.wait_view(t + Duration::from_secs(13)), WaitView::Frozen);
        assert!(c.wait_hold());
        let (_, act) = at(&mut c, t, 17.0, true, 7.0);
        assert_eq!(act, FreezeAction::None, "held");
        let (_, act) = at(&mut c, t, 18.0, true, 0.0);
        assert_eq!(act, FreezeAction::Unpause, "a short freeze resumes");
        assert_eq!(c.take_wait_events(), vec![WaitEvent::Back { long: false }]);
        assert!(!c.waiting());
    }

    #[test]
    fn the_host_plays_on_fifteen_seconds_then_waits_and_a_long_freeze_is_a_pause() {
        let t = Instant::now();
        let mut c = running(CLIENT, t);
        at(&mut c, t, 13.0, false, 3.0);
        assert_eq!(c.take_wait_events(), vec![WaitEvent::Lost]);
        assert_eq!(
            c.wait_view(t + Duration::from_secs(16)),
            WaitView::PlayingOn {
                left: Duration::from_secs(9)
            }
        );
        timeout(&mut c, CLIENT, t + Duration::from_secs(20));
        let (_, act) = at(&mut c, t, 24.9, false, 0.0);
        assert_eq!(act, FreezeAction::None, "still playing on alone");
        let (_, act) = at(&mut c, t, 25.0, false, 0.0);
        assert_eq!(act, FreezeAction::Pause, "15 s after the last packet");
        assert!(
            c.hosting() && !c.linked(),
            "still the host through the wait (its leave question)"
        );
        // Minutes: nothing ends the wait.
        for s in (30..600).step_by(30) {
            let (_, act) = at(&mut c, t, f64::from(s), true, 0.0);
            assert_eq!(act, FreezeAction::None);
            assert!(c.waiting() && c.coop_run());
        }
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t + Duration::from_secs(600),
            0,
        );
        let (_, act) = at(&mut c, t, 600.1, true, 0.0);
        assert_eq!(
            act,
            FreezeAction::None,
            "the freeze stays: the shared pause"
        );
        assert_eq!(c.take_wait_events(), vec![WaitEvent::Back { long: true }]);
        let (_, act) = at(&mut c, t, 601.0, true, 0.0);
        assert_eq!(act, FreezeAction::None, "never unpaused by the wait");
    }

    /// The host gone for good, each way: the joining player's run is over.
    #[test]
    fn the_host_gone_for_good_ends_the_joining_players_run() {
        let t = Instant::now();
        let goodbye = Event::Disconnected {
            peer: HOST,
            reason: DisconnectReason::Remote,
        };
        for (way, stays) in [("RunLeft", true), ("goodbye", false)] {
            let mut c = running(HOST, t);
            let ev = match way {
                "RunLeft" => msg(HOST, Message::RunLeft { peer: HOST }),
                _ => goodbye.clone(),
            };
            let t1 = t + Duration::from_secs(5);
            assert!(c.handle(ev, t1, 0).is_empty(), "{way}");
            assert_eq!(c.run_over(), Some(t1), "{way}");
            assert!(!c.waiting(), "{way}");
            let (_, act) = at(&mut c, t, 5.1, false, 0.0);
            assert_eq!(act, FreezeAction::Pause, "{way}: it stands");
            let (_, act) = at(&mut c, t, 9.0, true, 0.0);
            assert_eq!(act, FreezeAction::None, "{way}: held until it leaves");
            assert!(c.take_wait_events().is_empty(), "{way}: no wait");
            let outs = c.menu_frame();
            assert!(outs.contains(&Out::HoldLink(false)), "{way}");
            assert_eq!(c.linked(), stays, "{way}");
            let mut f = frame(4, false, true);
            f.stamp = Some(9.0);
            let (outs, _) = c.run_frame(&f, t + Duration::from_secs(20), 0);
            assert_eq!(c.coop_run(), stays, "{way}: the next run");
            assert_eq!(outs.contains(&Out::HoldLink(true)), stays, "{way}");
            assert_eq!(c.run_over(), None, "{way}");
        }
        // The host's own partner leaving changes nothing of this: it plays on.
        let mut h = running(CLIENT, t);
        h.handle(msg(CLIENT, Message::RunLeft { peer: CLIENT }), t, 0);
        assert_eq!(h.run_over(), None);
        let (_, act) = at(&mut h, t, 1.0, false, 0.0);
        assert_eq!(act, FreezeAction::None);
    }

    #[test]
    fn a_goodbye_is_not_waited_for() {
        let t = Instant::now();
        let mut c = running(HOST, t);
        c.handle(
            Event::Disconnected {
                peer: HOST,
                reason: DisconnectReason::Remote,
            },
            t,
            0,
        );
        let (_, act) = at(&mut c, t, 1.0, false, 0.0);
        assert_eq!(act, FreezeAction::Pause, "the run is over: the message");
        assert!(!c.waiting());
        assert!(c.run_over().is_some());
        // Nor a partner that left the run for the menu.
        let mut c = running(CLIENT, t);
        c.handle(msg(CLIENT, Message::RunLeft { peer: CLIENT }), t, 0);
        timeout(&mut c, CLIENT, t);
        let (_, act) = at(&mut c, t, 30.0, false, 0.0);
        assert_eq!(act, FreezeAction::None);
        assert!(!c.waiting());
    }

    /// On the upgrade screen the wait pauses only as the screen closes.
    #[test]
    fn the_wait_pauses_after_the_upgrade_screen() {
        let t = Instant::now();
        let mut c = running(HOST, t);
        let mut f = frame(3, false, true);
        f.upgrade_screen = true;
        f.silent_for = Some(Duration::from_secs(3));
        let (_, act) = c.run_frame(&f, t + Duration::from_secs(13), 0);
        assert_eq!(act, FreezeAction::None);
        assert!(c.wait_hold(), "waiting, frozen by the upgrade screen");
        f.upgrade_screen = false;
        f.silent_for = Some(Duration::from_secs(5));
        let (_, act) = c.run_frame(&f, t + Duration::from_secs(15), 0);
        assert_eq!(act, FreezeAction::Pause);
    }

    /// Back after 15 s on the upgrade screen: an ordinary return.
    #[test]
    fn a_return_on_the_upgrade_screen_is_not_a_long_freeze() {
        let t = Instant::now();
        let mut c = running(HOST, t);
        let mut f = frame(3, false, true);
        f.upgrade_screen = true;
        for s in [3, 10, 18] {
            f.silent_for = Some(Duration::from_secs(s));
            let (_, act) = c.run_frame(&f, t + Duration::from_secs(s), 0);
            assert_eq!(act, FreezeAction::None);
        }
        f.silent_for = Some(Duration::ZERO);
        let (_, act) = c.run_frame(&f, t + Duration::from_secs(19), 0);
        assert_eq!(act, FreezeAction::None);
        assert_eq!(
            c.take_wait_events(),
            vec![WaitEvent::Lost, WaitEvent::Back { long: false }]
        );
    }

    #[test]
    fn a_player_who_paused_keeps_their_pause_at_the_return() {
        let t = Instant::now();
        let mut c = running(HOST, t);
        // Paused by the player before the cut: the freeze adds nothing.
        let (_, act) = at(&mut c, t, 13.0, true, 3.0);
        assert_eq!(act, FreezeAction::None);
        let (_, act) = at(&mut c, t, 18.0, true, 0.0);
        assert_eq!(act, FreezeAction::None, "not the gate's to unpause");
        assert_eq!(
            c.take_wait_events(),
            vec![WaitEvent::Lost, WaitEvent::Back { long: false }]
        );
    }
}

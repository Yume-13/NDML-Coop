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
//! `timeGameStart`). A run that starts linked, or whose Start was taken while
//! linked ([`Core::start_taken`]), is a co-op run until it ends. Save
//! protection and the leaderboard block follow it. A lost link is waited for
//! ([`crate::wait`]). A partner gone for good ends the run in either role
//! ([`Core::run_over`]). A resumed run is never a co-op run.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use ndml_net::endpoint::DisconnectReason;
use ndml_net::endpoint::Event;
use ndml_net::lobby::files::clean_name;
use ndml_net::lobby::refusal_message;
use ndml_net::msg::{Message, PeerId, PlayerLook};
use ndml_net::session::PeerSet;

use crate::gate::{FreezeAction, Freezer, GatePhase, StartAsk, StartGate};
use crate::net::Out;
use crate::overlay::{Line, RunView, run_lines};
use crate::player::{
    LOOK_INTERVAL, PowersRead, RemoteShip, STATE_INTERVAL, ShipRead, look_changed, pack_look,
    pack_look_frame, pack_state,
};
use crate::render::{DrawItem, Gui, LookParams, PartnerFrame, View, plan_partner};
use crate::wait::{HOST_PLAYS_ON, LinkWait, WaitEvent, WaitInput, WaitView};

/// How long a one-off overlay message stays.
pub const NOTICE_FOR: Duration = Duration::from_secs(6);

/// A linked partner.
#[derive(Debug, Clone)]
pub struct Partner {
    /// Name from its `Hello`.
    pub name: String,
    /// Its `Hello` arrived on this link.
    pub greeted: bool,
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
    /// The partner left this run by choice, since then: it is over here too.
    over: Option<Instant>,
    /// The game's own end screen is up: a partner leaving now ends nothing.
    ended: bool,
}

/// How long a game whose co-op run is over (the partner left) shows why,
/// paused, before it leaves for the menu as Leave Co-op Run does.
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
    /// The session rules keep this game paused (`rules::Rules::holds_game`).
    /// A lifting wait freeze leaves the pause to them.
    pub rules_hold: bool,
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
#[allow(clippy::struct_excessive_bools)] // independent session facts, one each
pub struct Core {
    extrapolation: Duration,
    me: Option<PeerId>,
    partners: BTreeMap<PeerId, Partner>,
    gate: StartGate,
    freezer: Freezer,
    /// An upgrade screen asked for before the gate's release ([`StartAsk`]).
    start_ask: StartAsk,
    run: Option<Run>,
    view: Option<View>,
    local: Local,
    notices: VecDeque<(String, Instant)>,
    /// Partners still linked that left this co-op run (`RunLeft`).
    left_run: BTreeSet<PeerId>,
    /// A run started without a partner: newcomers are turned away until it
    /// ends (no linking during a run, on either route).
    solo_refusing: bool,
    /// The next run is a resumed one ([`Self::resumed_next`]).
    resumed_next: bool,
    /// Why no run can be a co-op run in this game ([`Self::coop_runs_off`]).
    coop_off: Option<String>,
    /// When the last partner's link timed out (cleared when one links).
    lost_at: Option<Instant>,
    /// A Start taken while linked, until its run's first frame.
    start_linked: bool,
    /// This game left a co-op run while the link was down: the partner is
    /// sent `RunLeft` when it links again.
    owe_run_left: bool,
    /// The co-op run's wait for a lost link.
    wait: LinkWait,
    /// Wait events for the glue (the rules, the UI).
    wait_events: Vec<WaitEvent>,
    /// When the freezer paused this game for the link wait.
    wait_frozen: Option<Instant>,
    /// The link wait had this game frozen when the link came back.
    frozen_at_back: bool,
    /// The partner last linked (kept while it is away) and its name.
    last_partner: Option<(PeerId, String)>,
    /// Whether this game hosted when last linked.
    last_host: bool,
}

/// A run that starts this soon after the partner's link timed out is a
/// co-op run waiting for the partner. After a Start taken while linked, a
/// timeout at any time since that Start counts too.
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
            start_ask: StartAsk::default(),
            run: None,
            view: None,
            local: Local::default(),
            notices: VecDeque::new(),
            left_run: BTreeSet::new(),
            solo_refusing: false,
            resumed_next: false,
            coop_off: None,
            lost_at: None,
            start_linked: false,
            owe_run_left: false,
            wait: LinkWait::default(),
            wait_events: Vec::new(),
            wait_frozen: None,
            frozen_at_back: false,
            last_partner: None,
            last_host: false,
        }
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

    /// When this game's co-op run ended (the partner left it by choice).
    #[must_use]
    pub fn run_over(&self) -> Option<Instant> {
        self.run.as_ref().and_then(|r| r.over)
    }

    /// The partner left this co-op run by choice: the run is over here too,
    /// in either role. Nothing on this game's own end screen.
    fn partner_gone(&mut self, now: Instant) {
        let host = self.me().is_host();
        let Some(run) = self
            .run
            .as_mut()
            .filter(|r| r.coop && r.over.is_none() && !r.ended)
        else {
            return;
        };
        run.over = Some(now);
        self.wait.reset();
        let who = if host { "the partner" } else { "the host" };
        self.note(format!("{who} left; this run is over"), now);
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

    /// The link wait had this game frozen at its last return.
    #[must_use]
    pub fn frozen_at_back(&self) -> bool {
        self.frozen_at_back
    }

    /// This game's run is over: a game over's end screen, or the partner gone.
    #[must_use]
    pub fn run_over_here(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|r| r.over.is_some() || r.ended)
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

    /// A partner is connected and its `Hello` is in.
    #[must_use]
    pub fn greeted(&self) -> bool {
        self.partners.values().any(|p| p.greeted)
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

    /// The game called `gotoUpgrade` ([`StartAsk::note`]). `true` if kept for
    /// the gate's release.
    pub fn note_start_ask(&mut self) -> bool {
        let released = self.gate.phase() == GatePhase::Running;
        self.start_ask.note(self.linked(), released)
    }

    /// Whether to ask for that screen again this frame ([`StartAsk::due`]).
    pub fn start_ask_due(&mut self, paused: bool, screen_open: bool) -> bool {
        let ready = self.gate.phase() == GatePhase::Running && !paused;
        let wanted = self.coop_run() && !screen_open;
        self.start_ask.due(ready, wanted)
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
                        greeted: false,
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
                if std::mem::take(&mut self.owe_run_left) {
                    log::info!("co-op: telling {peer} we left the run during the lost link");
                    outs.push(Out::Send(peer, Message::RunLeft { peer: self.me() }));
                } else if self.run.is_none() {
                    // A partner in a run with this game takes it as a leave.
                    // `RunLeft` counts only past the partner's gate.
                    log::info!("co-op: telling {peer} this game is in no run");
                    outs.push(Out::Send(peer, Message::RunLeft { peer: self.me() }));
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
        if peer.is_host() || self.gate.phase() == GatePhase::Running {
            // Gone for good (a goodbye, a restart, a refusal): the run is over.
            self.partner_gone(now);
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
                    p.greeted = true;
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
                self.partner_gone(now);
            }
            _ => {} // not the session's
        }
        Vec::new()
    }

    /// `resumeRun` ran: the next run is resumed and solo. A partner linked at
    /// its first frame is turned away, and the run saves as the game does.
    pub fn resumed_next(&mut self) {
        self.resumed_next = true;
    }

    /// The game took a new run's Start (its fade began). Taken while linked, it
    /// makes a co-op run whatever the link does next. The resume file stays off
    /// until the run's first frame ([`Self::start_held`]).
    pub fn start_taken(&mut self) {
        self.start_linked |= self.linked();
    }

    /// A Start taken while linked waits for its run's first frame.
    #[must_use]
    pub fn start_held(&self) -> bool {
        self.start_linked
    }

    /// The co-op rules cannot be kept in this game (`why`, from
    /// `loadout::run_safety`). Every run is solo, a partner linked at its start
    /// turned away. The menus link as usual.
    pub fn coop_runs_off(&mut self, why: String) {
        self.coop_off = Some(why);
    }

    /// A run started linked can be a co-op run here (no [`Self::coop_runs_off`]).
    /// Only then does `ui::menu` answer the saved-run question for a linked player.
    #[must_use]
    pub fn coop_runs_on(&self) -> bool {
        self.coop_off.is_none()
    }

    /// Every linked partner turned away with "in a run", out of this run.
    fn turn_partners_away(&mut self, now: Instant) -> Vec<Out> {
        let why = ndml_net::endpoint::RefuseReason::RunInProgress;
        let peers: Vec<PeerId> = self.partners.keys().copied().collect();
        let mut outs = Vec::new();
        for p in peers {
            self.left_run.insert(p);
            self.note(
                format!("{p} turned away: this game resumed a solo run"),
                now,
            );
            outs.push(Out::Refuse(p, why));
        }
        outs
    }

    /// A menu frame: any run is over.
    pub fn menu_frame(&mut self) -> Vec<Out> {
        // A `resumeRun` or Start whose run never came (a game error): forgotten.
        self.resumed_next = false;
        self.start_linked = false;
        if self.run.is_some() {
            self.leave_run()
        } else {
            Vec::new()
        }
    }

    fn leave_run(&mut self) -> Vec<Out> {
        let was_coop = self.run.take().is_some_and(|r| r.coop);
        self.freezer.reset();
        self.start_ask.reset();
        self.wait_frozen = None;
        self.local.last_look = None;
        self.left_run.clear();
        let mut outs = self.gate.leave_run();
        if std::mem::take(&mut self.solo_refusing) && !outs.contains(&Out::Refusing(None)) {
            outs.push(Out::Refusing(None));
        }
        if was_coop && self.linked() {
            // The partner's run ends with this one (or it is told at its gate).
            outs.push(Out::Broadcast(Message::RunLeft { peer: self.me() }));
        }
        if was_coop && (!self.linked() || self.wait.waiting()) {
            // The link is down or silent: told again at the next link.
            self.owe_run_left = true;
        }
        if was_coop {
            outs.push(Out::HoldLink(false));
        }
        self.wait.reset();
        outs
    }

    /// A run-tick frame. Returns what to send and what to do to the game.
    #[allow(clippy::too_many_lines)] // the run start's steps in order
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
            // A resumed run is solo: a partner linked now is turned away.
            let resumed = std::mem::take(&mut self.resumed_next);
            let started_linked = std::mem::take(&mut self.start_linked);
            let solo = resumed || self.coop_off.is_some();
            if solo {
                outs.extend(self.turn_partners_away(now));
            }
            let unlinked = !solo && !self.linked();
            // The link timed out just before, or since a Start taken while
            // linked: a lost link, waited for.
            let awaiting = unlinked
                && self.lost_at.is_some_and(|t| {
                    started_linked || now.saturating_duration_since(t) <= RUN_START_GRACE
                });
            // A linked Start whose partner is gone for good since: over at once.
            let gone = unlinked && started_linked && !awaiting;
            let coop = !solo && (self.linked() || awaiting || gone);
            self.run = Some(Run {
                key: f.key,
                stamp: f.stamp,
                coop,
                awaiting_partner: awaiting,
                gone,
                ..Run::default()
            });
            if coop {
                // The link is held for the whole co-op run.
                outs.push(Out::HoldLink(true));
            }
            if self.linked() {
                // Linked at a new run's start: an owed `RunLeft` is void.
                self.owe_run_left = false;
            }
            if resumed {
                log::info!(
                    "co-op: a resumed run starts: a solo run (a resumed run is never a co-op run; it saves as the game does)"
                );
            } else if let Some(why) = &self.coop_off {
                log::info!("co-op: a run starts solo: co-op runs are off in this game ({why})");
            } else if self.linked() {
                log::info!(
                    "co-op: a co-op run starts (saves and leaderboard uploads off until it ends)"
                );
            } else if awaiting {
                log::info!(
                    "co-op: a co-op run starts with the partner's link just lost; it waits for the partner at the start gate when they are back (saves and leaderboard uploads off until it ends)"
                );
            } else if gone {
                log::info!(
                    "co-op: a co-op run starts with the partner gone since its Start (saves and leaderboard uploads off until it ends)"
                );
                self.partner_gone(now);
            }
            if !coop && !self.solo_refusing {
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
        run.ended |= f.game_over;
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
            plays_on: HOST_PLAYS_ON,
            woke: f.woke,
            now,
        });
        if let Some(ev) = ev {
            self.wait_event(ev, gate_hold);
        }
        // A run that is over stands paused until it leaves, after the upgrade
        // screen. The game's own end screen is never frozen.
        let over = self.run.as_ref().is_some_and(|r| r.over.is_some());
        let ended = self.run.as_ref().is_some_and(|r| r.ended);
        let hold = gate_hold || ((over || self.wait.hold()) && !f.upgrade_screen && !ended);
        if !hold && f.paused && f.rules_hold && self.freezer.made_pause() {
            // The freeze lifts into a pause the rules hold: no unpause.
            self.freezer.adopt();
            if let Some(t) = self.wait_frozen.take() {
                log::info!(
                    "co-op: the link wait's freeze lifts after {:.2} s into the session's pause or round",
                    now.saturating_duration_since(t).as_secs_f64()
                );
            }
        }
        let action = self.freezer.decide(hold, f.paused, frame);
        self.note_freeze(action, gate_hold, now);
        (outs, action)
    }

    /// One wait event. A long freeze stays as the shared pause. A return to a
    /// game the wait did not freeze holds nothing.
    fn wait_event(&mut self, mut ev: WaitEvent, gate_hold: bool) {
        log::info!("co-op: link wait {ev:?}");
        if matches!(ev, WaitEvent::Back { .. }) {
            self.frozen_at_back = self.wait_frozen.is_some() || self.freezer.made_pause();
        }
        if ev == (WaitEvent::Back { long: true }) {
            if !gate_hold && self.freezer.adopt() {
                self.wait_frozen = None;
                // The freeze stays as a player's pause: the shared pause.
                log::info!("co-op: frozen 10 s or more; the wait becomes the shared pause");
            } else {
                // Not paused by the wait (upgrade screen, start gate): a plain return.
                log::info!("co-op: not frozen by the wait here; an ordinary return");
                ev = WaitEvent::Back { long: false };
            }
        } else if matches!(ev, WaitEvent::Back { .. }) && !self.freezer.made_pause() {
            // Not frozen by the wait: nothing to hold.
            self.wait.lift_now();
        }
        self.wait_events.push(ev);
    }

    /// Logs the link wait's freeze and how long it lasted.
    fn note_freeze(&mut self, action: FreezeAction, gate_hold: bool, now: Instant) {
        match action {
            FreezeAction::Pause if !gate_hold && self.wait.hold() => {
                if self.wait_frozen.is_none() {
                    log::info!("co-op: the link wait freezes this game");
                }
                self.wait_frozen.get_or_insert(now);
            }
            FreezeAction::Unpause => {
                if let Some(t) = self.wait_frozen.take() {
                    log::info!(
                        "co-op: the link wait's freeze lifts after {:.2} s",
                        now.saturating_duration_since(t).as_secs_f64()
                    );
                }
            }
            _ => {}
        }
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
            let t = render(peer).unwrap_or_else(|| ndml_net::time::from_wire_ms(latest.time_ms));
            if p.ship.downed_at(t) {
                // Down at the render time: no ship, label or marker.
                continue;
            }
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
            rules_hold: false,
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

    /// Co-op runs off in this game: a partner linked at the run's start is
    /// turned away with "in a run", and nothing is held or sent.
    #[test]
    fn without_the_saved_runs_hooks_every_run_is_solo() {
        let t = Instant::now();
        let why = ndml_net::endpoint::RefuseReason::RunInProgress;
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        c.handle(msg(CLIENT, hello("Sam", "pe-1-2")), t, 0);
        assert!(c.linked());
        c.coop_runs_off("no resumeRun or confirmLoadout hook".into());
        let (outs, _) = c.run_frame(&frame(5, false, true), t, 0);
        assert_eq!(
            outs,
            vec![Out::Refuse(CLIENT, why), Out::Refusing(Some(why))],
            "turned away, newcomers too; no hold, no RunStart"
        );
        assert!(!c.coop_run() && c.in_run() && !c.run_linked());
        assert_eq!(
            c.switches(),
            (false, false),
            "no scaling, saves as the game does"
        );
    }

    /// A resumed run is solo: a link that completed during its start is
    /// turned away with "in a run", and the next run starts afresh.
    #[test]
    fn a_resumed_run_is_never_a_coop_run() {
        let t = Instant::now();
        let why = ndml_net::endpoint::RefuseReason::RunInProgress;
        for partner in [CLIENT, HOST] {
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
            }
            assert!(c.linked());
            c.resumed_next();
            let (outs, act) = c.run_frame(&frame(5, false, true), t, 0);
            assert_eq!(
                outs,
                vec![Out::Refuse(partner, why), Out::Refusing(Some(why))],
                "{partner:?}: turned away, newcomers too; no hold, no RunStart"
            );
            assert_eq!(act, FreezeAction::None);
            assert!(!c.coop_run() && c.in_run());
            assert!(!c.run_linked(), "the partner is out of this run at once");
            assert_eq!(c.switches(), (false, false), "no scaling, saves on");
            let (outs, _) = c.run_frame(&frame(5, false, true), t, 10);
            assert!(outs.is_empty(), "{outs:?}");
            assert!(c.local_ship(&ship(), 60.0, 20, t).is_empty());
            // The run ends: newcomers admitted, the next linked run is co-op.
            assert_eq!(c.menu_frame(), vec![Out::Refusing(None)]);
            let (outs, _) = c.run_frame(&frame(6, false, true), t, 30);
            assert!(outs.contains(&Out::HoldLink(true)), "{outs:?}");
            assert!(c.coop_run());
        }
        // Unlinked: a plain solo run. A stale mark is dropped at the menu.
        let mut c = Core::new(Duration::from_millis(250));
        c.resumed_next();
        let (outs, _) = c.run_frame(&frame(1, false, true), t, 0);
        assert_eq!(outs, vec![Out::Refusing(Some(why))]);
        assert!(!c.coop_run());
        let mut c = Core::new(Duration::from_millis(250));
        c.resumed_next();
        let _ = c.menu_frame();
        c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        let (outs, _) = c.run_frame(&frame(2, false, true), t, 0);
        assert!(outs.contains(&Out::HoldLink(true)) && c.coop_run());
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
        // In no run: it says so.
        assert!(matches!(
            outs.as_slice(),
            [
                Out::Send(CLIENT, Message::Welcome { .. }),
                Out::Send(CLIENT, Message::RunLeft { peer: HOST })
            ]
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
            vec![
                Out::Send(
                    CLIENT,
                    Message::Welcome {
                        peer: CLIENT,
                        peers: 2
                    }
                ),
                Out::Send(CLIENT, Message::RunLeft { peer: HOST })
            ]
        );
        assert!(c.linked() && !c.greeted(), "linked, its hello not in yet");
        c.handle(msg(CLIENT, hello("Wing", "pe-1-2")), t, 0);
        assert!(c.greeted());
        assert_eq!(c.partners()[&CLIENT].name, "Wing");
        assert!(c.is_host());
        // Menu → new game screen → first controller frame (paused intro), then gameplay.
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
        // In no run: it says so, and the host, in no run either, ignores it.
        assert_eq!(
            c.handle(
                Event::Connected {
                    peer: HOST,
                    addr: addr()
                },
                t,
                0
            ),
            vec![Out::Send(HOST, Message::RunLeft { peer: PeerId(1) })]
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

    /// On the new game screen, linked to `partner` (no run yet).
    fn on_new_game_screen(partner: PeerId, t: Instant) -> Core {
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: partner,
                addr: addr(),
            },
            t,
            0,
        );
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
        }
        c
    }

    /// A linked Start makes a co-op run in either role, the resume file off
    /// until its first frame. A partner gone for good ends it at once, a lost
    /// link is waited for, and an earlier run's `RunLeft` is ignored.
    #[test]
    fn a_start_taken_linked_is_a_coop_run_whatever_the_link_does_before_its_first_frame() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        for partner in [HOST, CLIENT] {
            for (way, first, over) in [
                ("goodbye", 300, true),
                ("restart", 300, true),
                ("timeout in the grace", 500, false),
                ("timeout outside the grace", 5_000, false),
                ("RunLeft", 300, false),
                ("crash", 300, false),
            ] {
                let way = format!("{partner} {way}");
                let mut c = on_new_game_screen(partner, t);
                c.start_taken();
                assert!(c.start_held(), "{way}: off from the Start");
                let gone = |reason| Event::Disconnected {
                    peer: partner,
                    reason,
                };
                if way.ends_with("goodbye") {
                    c.handle(gone(DisconnectReason::Remote), ms(100), 0);
                } else if way.ends_with("restart") {
                    c.handle(gone(DisconnectReason::Replaced), ms(100), 0);
                } else if way.contains("timeout") {
                    c.handle(gone(DisconnectReason::Timeout), ms(100), 0);
                } else if way.ends_with("RunLeft") {
                    c.handle(msg(partner, Message::RunLeft { peer: partner }), ms(100), 0);
                }
                assert!(c.start_held(), "{way}: still off before the run");
                let t1 = ms(first);
                let (outs, act) = c.run_frame(&frame(3, false, true), t1, 0);
                assert!(c.coop_run(), "{way}");
                assert!(
                    c.switches().1,
                    "{way}: the run's latch from its first frame"
                );
                assert!(!c.start_held(), "{way}: taken over");
                assert!(outs.contains(&Out::HoldLink(true)), "{way}");
                assert!(
                    !outs.iter().any(|o| matches!(o, Out::Refusing(Some(_)))),
                    "{way}: not a solo run: {outs:?}"
                );
                assert_eq!(c.run_over(), over.then_some(t1), "{way}");
                if over {
                    assert_eq!(act, FreezeAction::Pause, "{way}: it stands, then the menu");
                }
                if way.ends_with("crash") {
                    timeout(&mut c, partner, ms(10_300));
                }
                let _ = at(&mut c, t, 20.0, false, 0.0);
                if over {
                    assert!(!c.waiting(), "{way}: not waited for");
                } else if c.linked() {
                    assert!(c.run_over().is_none(), "{way}: the run goes on");
                } else {
                    assert!(c.waiting(), "{way}: a lost link is waited for");
                    assert!(c.run_over().is_none(), "{way}");
                }
                assert!(c.switches().1, "{way}: saves off to its end");
                let outs = c.menu_frame();
                assert!(outs.contains(&Out::HoldLink(false)), "{way}");
                assert!(!c.switches().1, "{way}: the menu ends it");
            }
        }
    }

    /// Only a linked Start sets the latch. A latch whose run never came is
    /// dropped at the menu.
    #[test]
    fn an_unlinked_start_is_solo_and_the_menu_drops_a_start_latch() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        c.start_taken();
        assert!(!c.start_held());
        let (outs, _) = c.run_frame(&frame(3, false, true), t, 0);
        assert!(!c.coop_run());
        assert!(outs.iter().any(|o| matches!(o, Out::Refusing(Some(_)))));
        let mut c = on_new_game_screen(CLIENT, t);
        c.start_taken();
        c.handle(
            Event::Disconnected {
                peer: CLIENT,
                reason: DisconnectReason::Remote,
            },
            t,
            0,
        );
        let _ = c.menu_frame();
        assert!(!c.start_held());
        let (_, _) = c.run_frame(&frame(3, false, true), t, 0);
        assert!(!c.coop_run(), "a later unlinked Start is solo");
    }

    /// Where co-op runs are off, the game asks its saved-run question itself.
    #[test]
    fn coop_runs_off_leave_the_saved_run_question_to_the_game() {
        let mut c = Core::new(Duration::from_millis(250));
        assert!(c.coop_runs_on());
        c.coop_runs_off("a hook".into());
        assert!(!c.coop_runs_on());
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

    /// A partner who quits to the menu says `RunLeft`. The run is over here
    /// too, with saves off while it stands. The partner's next run waits at
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
        assert!(c.run_over().is_some(), "the run is over here too");
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

    /// Draft's opening screen kept at the gate is asked once at its own run's
    /// release. A run left at the gate leaves nothing for the next run.
    #[test]
    fn an_ask_kept_at_the_gate_is_its_own_runs_and_cleared_at_the_menu() {
        let t = Instant::now();
        let mut c = Core::new(Duration::from_millis(250));
        c.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t,
            0,
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
        // Run 1's Create keeps it, and the run is left at the gate.
        assert!(c.note_start_ask());
        c.run_frame(&frame(3, false, true), t, 0);
        c.menu_frame();
        // Run 2, with no ask of its own: nothing at its release.
        c.run_frame(&frame(4, false, true), t, 0);
        c.handle(msg(HOST, Message::Go { start_ms: 5 }), t, 0);
        assert_eq!(c.gate_phase(), GatePhase::Running);
        assert!(!c.start_ask_due(false, false));
        assert!(!c.note_start_ask(), "a call in play is not kept");
        // Run 3's Create keeps it again: once, at its release.
        c.menu_frame();
        assert!(c.note_start_ask());
        c.run_frame(&frame(5, false, true), t, 0);
        assert!(!c.start_ask_due(false, false), "held at the gate");
        c.handle(msg(HOST, Message::Go { start_ms: 6 }), t, 0);
        assert_eq!(c.gate_phase(), GatePhase::Running);
        assert!(!c.start_ask_due(true, false), "paused: not yet");
        assert!(c.start_ask_due(false, false));
        assert!(!c.start_ask_due(false, false));
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

    /// After a 3.4 s silence the freeze holds 2 s. A game the wait did not
    /// freeze is not held after the return.
    #[test]
    fn a_short_freeze_is_held_two_seconds_and_only_if_it_was_shown() {
        let t = Instant::now();
        let mut c = running(HOST, t);
        let (_, act) = at(&mut c, t, 13.0, false, 3.0);
        assert_eq!(act, FreezeAction::Pause);
        let (_, act) = at(&mut c, t, 13.4, true, 0.0);
        assert_eq!(act, FreezeAction::None, "held");
        assert_eq!(
            c.take_wait_events(),
            vec![WaitEvent::Lost, WaitEvent::Back { long: false }]
        );
        let (_, act) = at(&mut c, t, 14.9, true, 0.0);
        assert_eq!(act, FreezeAction::None, "held");
        assert!(c.wait_hold());
        let (_, act) = at(&mut c, t, 15.0, true, 0.0);
        assert_eq!(act, FreezeAction::Unpause, "2 s after the freeze began");
        // Paused by the player when the wait began: not the wait's hold.
        let mut c = running(HOST, t);
        at(&mut c, t, 13.0, true, 3.0);
        at(&mut c, t, 13.4, true, 0.0);
        assert!(!c.wait_hold(), "nothing of the wait's to hold");
    }

    /// A 2 s hold that lifts while the rules hold the game leaves them the pause.
    #[test]
    fn the_hold_lifts_into_the_rules_pause_without_an_unpause() {
        let t = Instant::now();
        let mut c = running(HOST, t);
        let (_, act) = at(&mut c, t, 13.0, false, 3.0);
        assert_eq!(act, FreezeAction::Pause);
        at(&mut c, t, 13.4, true, 0.0);
        let mut f = frame(3, true, true);
        f.silent_for = Some(Duration::ZERO);
        f.rules_hold = true;
        let (_, act) = c.run_frame(&f, t + Duration::from_secs(15), 0);
        assert_eq!(act, FreezeAction::None, "left paused for the rules");
        assert!(!c.wait_hold());
        f.rules_hold = false;
        let (_, act) = c.run_frame(&f, t + Duration::from_secs(16), 0);
        assert_eq!(
            act,
            FreezeAction::None,
            "never the freezer's to unpause now"
        );
        // Without a rules hold the lift unpauses.
        let mut c = running(HOST, t);
        at(&mut c, t, 13.0, false, 3.0);
        at(&mut c, t, 13.4, true, 0.0);
        let (_, act) = at(&mut c, t, 15.0, true, 0.0);
        assert_eq!(act, FreezeAction::Unpause);
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

    /// The partner gone for good, each way and in either role: the run is over
    /// and stands paused. The next run is co-op again where the link stays.
    #[test]
    fn a_partner_gone_for_good_ends_the_run_in_either_role() {
        let t = Instant::now();
        for (partner, way, stays) in [
            (HOST, "RunLeft", true),
            (HOST, "goodbye", false),
            (CLIENT, "RunLeft", true),
            (CLIENT, "goodbye", false),
        ] {
            let mut c = running(partner, t);
            let ev = match way {
                "RunLeft" => msg(partner, Message::RunLeft { peer: partner }),
                _ => Event::Disconnected {
                    peer: partner,
                    reason: DisconnectReason::Remote,
                },
            };
            let way = format!("{partner} {way}");
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
        // Not a run end: the partner leaving from this game's end screen.
        let mut h = running(CLIENT, t);
        let mut f = frame(3, false, true);
        f.game_over = true;
        h.run_frame(&f, t, 0);
        h.handle(msg(CLIENT, Message::RunLeft { peer: CLIENT }), t, 0);
        assert_eq!(h.run_over(), None, "the end screen stays as it is");
        let (_, act) = h.run_frame(&f, t + Duration::from_secs(1), 0);
        assert_eq!(act, FreezeAction::None);
    }

    /// A leave while the link is down is told to the partner at the next link.
    #[test]
    fn leaving_during_a_lost_link_is_told_at_the_next_link() {
        let t = Instant::now();
        let mut c = running(CLIENT, t);
        timeout(&mut c, CLIENT, t);
        let outs = c.menu_frame();
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Out::Broadcast(Message::RunLeft { .. }))),
            "nobody to tell now: {outs:?}"
        );
        let outs = c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        assert!(
            outs.contains(&Out::Send(CLIENT, Message::RunLeft { peer: HOST })),
            "{outs:?}"
        );
        // The debt is paid once. In no run, each new link says so once.
        assert!(!c.owe_run_left);
        c.handle(
            Event::Disconnected {
                peer: CLIENT,
                reason: DisconnectReason::Timeout,
            },
            t,
            0,
        );
        let outs = c.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        assert_eq!(
            outs.iter()
                .filter(|o| matches!(o, Out::Send(_, Message::RunLeft { .. })))
                .count(),
            1
        );
        // The partner waiting in that run takes it: run over.
        let mut w = running(HOST, t);
        timeout(&mut w, HOST, t);
        w.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t,
            0,
        );
        w.handle(msg(HOST, Message::RunLeft { peer: HOST }), t, 0);
        assert!(w.run_over().is_some());
    }

    /// A leave in a silent wait sends `RunLeft` at once and at the next link (a
    /// link timeout drops what was unsent). A new co-op run together drops it.
    #[test]
    fn leaving_in_a_silent_wait_is_told_now_and_at_the_next_link() {
        let t = Instant::now();
        let mut c = running(CLIENT, t);
        at(&mut c, t, 13.0, false, 3.0);
        assert!(c.waiting() && c.linked());
        let outs = c.menu_frame();
        assert!(outs.contains(&Out::Broadcast(Message::RunLeft { peer: HOST })));
        timeout(&mut c, CLIENT, t);
        let connect = Event::Connected {
            peer: CLIENT,
            addr: addr(),
        };
        let outs = c.handle(connect.clone(), t, 0);
        assert!(outs.contains(&Out::Send(CLIENT, Message::RunLeft { peer: HOST })));
        // Told by the return itself, and a new run together since: no debt.
        let mut c = running(CLIENT, t);
        at(&mut c, t, 13.0, false, 3.0);
        c.menu_frame();
        c.run_frame(&frame(4, false, true), t + Duration::from_secs(20), 0);
        timeout(&mut c, CLIENT, t);
        let outs = c.handle(connect, t, 0);
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Out::Send(_, Message::RunLeft { .. }))),
            "{outs:?}"
        );
    }

    /// A game that links again from its menus says it is in no run (`RunLeft`).
    /// The partner in the run takes it as a leave: its run is over.
    #[test]
    fn a_game_back_from_its_menus_ends_the_run_it_left() {
        let t = Instant::now();
        // The host restarted while the joining player was frozen in the run.
        let mut j = running(HOST, t);
        at(&mut j, t, 13.0, false, 3.0);
        assert!(j.wait_hold(), "frozen in the wait");
        timeout(&mut j, HOST, t + Duration::from_secs(20));
        let mut restarted = Core::new(Duration::from_millis(250));
        let said = restarted.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        assert!(said.contains(&Out::Send(CLIENT, Message::RunLeft { peer: HOST })));
        let t1 = t + Duration::from_secs(40);
        j.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t1,
            0,
        );
        j.handle(msg(HOST, hello("Sam", "pe-1-2")), t1, 0);
        j.handle(msg(HOST, Message::RunLeft { peer: HOST }), t1, 0);
        assert_eq!(j.run_over(), Some(t1), "the joining player's run is over");
        assert!(j.left_run(HOST));
        assert_eq!(
            j.run.as_ref().and_then(|r| r.left.clone()).as_deref(),
            Some("Sam left the run"),
            "as at a leave"
        );
        assert!(
            j.notices
                .iter()
                .any(|(n, _)| n == "Sam left the run for the menu")
        );
        // The joining game restarted while the host was in the run.
        let mut h = running(CLIENT, t);
        at(&mut h, t, 13.0, false, 3.0);
        timeout(&mut h, CLIENT, t + Duration::from_secs(20));
        let mut restarted = Core::new(Duration::from_millis(250));
        let said = restarted.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t,
            0,
        );
        assert_eq!(
            said,
            vec![Out::Send(HOST, Message::RunLeft { peer: PeerId(1) })]
        );
        h.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t1,
            0,
        );
        h.handle(msg(CLIENT, hello("Sam", "pe-1-2")), t1, 0);
        h.handle(msg(CLIENT, Message::RunLeft { peer: PeerId(1) }), t1, 0);
        assert_eq!(h.run_over(), Some(t1), "the host's run is over");
        assert!(h.left_run(CLIENT));
        let (_, act) = at(&mut h, t, 40.1, true, 0.0);
        assert_eq!(act, FreezeAction::None, "it stands until it leaves");
        assert!(!h.waiting(), "no wait: the run is over");
    }

    /// A first link with both games in the menus: each says it is in no run,
    /// and neither ends a run or shows a notice.
    #[test]
    fn a_first_link_in_the_menus_ends_nothing() {
        let t = Instant::now();
        let mut h = Core::new(Duration::from_millis(250));
        let mut j = Core::new(Duration::from_millis(250));
        let from_h = h.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t,
            0,
        );
        let from_j = j.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t,
            0,
        );
        assert!(from_h.contains(&Out::Send(CLIENT, Message::RunLeft { peer: HOST })));
        assert!(from_j.contains(&Out::Send(HOST, Message::RunLeft { peer: PeerId(1) })));
        j.handle(msg(HOST, Message::RunLeft { peer: HOST }), t, 0);
        h.handle(msg(CLIENT, Message::RunLeft { peer: PeerId(1) }), t, 0);
        for c in [&h, &j] {
            assert!(c.linked());
            assert!(!c.left_run(HOST) && !c.left_run(CLIENT));
            assert_eq!(c.run_over(), None);
            assert!(
                !c.notices.iter().any(|(n, _)| n.contains("left the run")),
                "{:?}",
                c.notices
            );
        }
        let (outs, _) = h.run_frame(&frame(3, false, true), t, 0);
        assert!(h.coop_run() && outs.contains(&Out::HoldLink(true)));
    }

    /// A return inside the run (the endpoint timed out and linked again)
    /// sends no `RunLeft`, and a long freeze is the shared pause.
    #[test]
    fn a_return_inside_the_run_sends_no_run_left() {
        let t = Instant::now();
        let mut h = running(CLIENT, t);
        let mut j = running(HOST, t);
        at(&mut h, t, 13.0, false, 3.0);
        at(&mut j, t, 13.0, false, 3.0);
        timeout(&mut h, CLIENT, t + Duration::from_secs(20));
        timeout(&mut j, HOST, t + Duration::from_secs(20));
        let t1 = t + Duration::from_secs(40);
        let outs_h = h.handle(
            Event::Connected {
                peer: CLIENT,
                addr: addr(),
            },
            t1,
            0,
        );
        let outs_j = j.handle(
            Event::Connected {
                peer: HOST,
                addr: addr(),
            },
            t1,
            0,
        );
        for outs in [&outs_h, &outs_j] {
            assert!(
                !outs
                    .iter()
                    .any(|o| matches!(o, Out::Send(_, Message::RunLeft { .. }))),
                "{outs:?}"
            );
        }
        let (_, act) = at(&mut j, t, 40.1, true, 0.0);
        assert_eq!(
            act,
            FreezeAction::None,
            "the freeze stays: the shared pause"
        );
        assert_eq!(
            j.take_wait_events(),
            vec![WaitEvent::Lost, WaitEvent::Back { long: true }]
        );
        assert_eq!(j.run_over(), None);
        assert_eq!(h.run_over(), None);
    }

    /// The host's game ends (everyone down) in its 15 s of playing on: its
    /// end screen is not frozen when they run out.
    #[test]
    fn the_wait_never_freezes_this_games_end_screen() {
        let t = Instant::now();
        let mut c = running(CLIENT, t);
        at(&mut c, t, 13.0, false, 3.0);
        assert!(c.waiting());
        let mut f = frame(3, false, true);
        f.game_over = true;
        for s in [16.0, 30.0, 40.0] {
            f.silent_for = Some(Duration::from_secs_f64(s - 10.0));
            let (_, act) = c.run_frame(&f, t + Duration::from_secs_f64(s), 0);
            assert_eq!(act, FreezeAction::None, "at {s} s");
        }
        // Without the end screen the same wait freezes the host.
        let mut c = running(CLIENT, t);
        at(&mut c, t, 13.0, false, 3.0);
        let (_, act) = at(&mut c, t, 30.0, false, 20.0);
        assert_eq!(act, FreezeAction::Pause);
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
        assert_eq!(act, FreezeAction::Pause);
        assert!(!c.waiting() && c.run_over().is_some());
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

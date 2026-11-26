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

//! # Co-op
//!
//! Two-player co-op for Nova Drift: a "Co-op" page in the game's main menu,
//! the partner's ship, a shared run start, save and leaderboard protection
//! for co-op runs, enemy scaling, the shared world and the session rules.
//!
//! This file holds init and the core hooks; the other subsystems register
//! their own (`ui::glue`, `rules::glue`, `world::glue`, `steam`, ...).
//!
//! No lock is held across a call into game code: each hook copies what it
//! needs under the lock, releases it, then calls builtins or scripts.

#![forbid(unsafe_code)]

/// A line about a problem that can repeat at play rate: `log::log!` at
/// `level`, a site's first, then at most one every [`game::LIMIT_EVERY`]
/// with how many were not logged since.
macro_rules! log_limited {
    ($level:expr, $site:expr, $($arg:tt)+) => {{
        match $crate::game::limit_due($site) {
            Some(0) => log::log!($level, $($arg)+),
            Some(q) => log::log!(
                $level,
                "{} ({q} more at this site not logged before it)",
                format_args!($($arg)+)
            ),
            None => {}
        }
    }};
}

pub mod callsigns;
pub mod config;
pub mod damage;
pub mod game;
pub mod gate;
pub mod guard;
pub mod ids;
pub mod link_log;
pub mod loadout;
mod loadout_glue;
pub mod mirror;
pub mod names;
pub mod net;
pub mod overlay;
pub mod player;
pub mod render;
pub mod rules;
pub mod scaling;
pub mod singularity;
mod singularity_glue;
pub mod slow_log;
pub mod sound;
pub mod state;
pub mod steam;
pub mod ui;
pub mod wait;
pub mod wild;
mod wild_glue;
pub mod world;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ndml_net::endpoint::Event;
use ndml_net::msg::PeerId;
use ndml_net::worker::{NetWorker, ShutdownStage};
use ndml_sdk::{Call, Error, ModContext, Value};

use config::{Config, Settings};
use game::{Handles, camera_view, gui_size, info_once, num, truthy, warn_once};
use gate::FreezeAction;
use names::builtin_var as bv;
use player::{PowersRead, ShipRead, sprite_index};
use render::View;
use scaling::{Flags, scaled};
use state::{Core, RunFrame};
use ui::page::{LabelMode, Page};
use ui::run::{LinkEvent, RunMessages, RunUi, SilenceWatch};
use ui::steam_coop::CoopSteam;
use ui::steam_link::{NotYet, SteamLink};
use world::present::{self, PartnerView};

/// Everything the hooks share, behind one lock.
#[derive(Debug)]
pub(crate) struct Coop {
    pub(crate) settings: Settings,
    pub(crate) core: Core,
    pub(crate) net: NetWorker,
    /// The co-op page (pure model).
    pub(crate) page: Page,
    /// What the UI glue remembers about the main menu.
    pub(crate) menu: ui::glue::MenuState,
    /// The Steam connector, through the page's one seam.
    pub(crate) steam: Box<dyn SteamLink>,
    /// Own co-op colour out, the partners' in.
    pub(crate) colors: world::color::ColorSync,
    /// Partner labels and markers in effect (the page can change them).
    pub(crate) labels: LabelMode,
    /// In-run messages and their timers.
    pub(crate) run_msgs: RunMessages,
    /// How long the partner's link has been silent (the "no signal" text).
    pub(crate) silence: SilenceWatch,
    /// This frame's silence (from the run tick).
    pub(crate) silent_for: Option<Duration>,
    /// The link line in the log and the traffic line's numbers.
    pub(crate) link_log: link_log::LinkLog,
    /// The slow-frames line in the log.
    slow_log: slow_log::SlowLog,
    /// The controller's last GUI draw (`run_draw`).
    drawn_at: Option<Instant>,
    /// The joining game's over run already asked to leave for the menu.
    over_quit: bool,
    /// This frame's run UI (chosen in the Draw `before`).
    pub(crate) run_ui: RunUi,
    /// The partner's Ready in this frame's run UI, drawn apart (`run_draw`).
    pub(crate) run_ready: RunUi,
    /// The post-run screen the partner's Ready is drawn on this frame. `None`
    /// outside a run that is over.
    pub(crate) end_place: Option<ui::run::ReadyPlace>,
    /// A link that was up went while this player was outside a run's play.
    pub(crate) link_notice: Option<ui::run::LinkNotice>,
    /// A link wait ended: when, and this game's state then (`return_line`).
    return_note: Option<(Instant, String)>,
    /// The run's end is on screen (why): nothing of co-op's drawn over it.
    pub(crate) end_shown: Option<&'static str>,
    /// Links that went away since the last run frame.
    pub(crate) run_events: Vec<LinkEvent>,
    /// The partner's display name, kept after they leave.
    pub(crate) last_partner: String,
    /// Partners styled for the labels (colour = accent).
    pub(crate) label_partners: Vec<PartnerView>,
    /// Draw-call path: the ship blend override and the accent.
    pub(crate) draw_style: Option<(Option<u32>, u32)>,
    /// `global.language`.
    pub(crate) lang: String,
    /// The player's config file (the page saves its choices there).
    pub(crate) config_path: PathBuf,
    /// Host messages for the shared world, taken by the next run tick.
    world_inbox: Vec<ndml_net::Message>,
    /// Partner mirror messages, taken by the next run tick.
    mirror_inbox: Vec<ndml_net::Message>,
    /// How the last link went away (for the world's lifecycle).
    world_loss: Option<world::Loss>,
    /// The session rules.
    pub(crate) rules: rules::Rules,
    /// The host's enemy multipliers (`SessionSettings`), used while linked to it.
    host_mults: Option<(f32, f32)>,
    /// The host's respawn time (`SessionSettings`), in force in the rules.
    /// Forgotten at a menu frame without a link.
    host_respawn: Option<Duration>,
    /// The host's run settings on a joining game (`loadout.rs`).
    pub(crate) loadout: loadout::Joiner,
    /// The new game screen: the bar line, the link line, the Start row's "Ready".
    pub(crate) loadout_screen: loadout_glue::Screen,
    /// Host: its run settings as last sent.
    pub(crate) host_sent: loadout::HostSent,
    /// This player's Ready as the partner last heard it. `None` while unlinked.
    ready_sent: Option<bool>,
    /// The partner's Ready notice, drawn wherever this player is.
    pub(crate) ready_notice: ui::run::ReadyNotice,
}

impl Coop {
    /// Each connected partner's measured round trip (the link's smoothed
    /// RTT), for the honour rule's window (`damage::honour_window`).
    fn partner_rtts(&self) -> Vec<(PeerId, Duration)> {
        self.core
            .partners()
            .keys()
            .filter_map(|&p| {
                self.net
                    .peer_info(p)
                    .filter(|i| i.connected)
                    .map(|i| (p, i.stats.rtt))
            })
            .collect()
    }

    /// Drain network events into the session and send what it answers.
    fn pump(&mut self, now: Instant) {
        let now_ms = ndml_net::time::to_wire_ms(self.net.local_time(now));
        let events: Vec<_> = self.net.events().collect();
        for ev in events {
            if let Event::Message {
                msg:
                    msg @ (ndml_net::Message::MirrorFrame(_)
                    | ndml_net::Message::MirrorLooks(_)
                    | ndml_net::Message::MirrorLeaders(_)
                    | ndml_net::Message::MirrorDraw(_)),
                ..
            } = ev
            {
                if self.mirror_inbox.len() >= 4096 {
                    self.mirror_inbox.clear();
                }
                self.mirror_inbox.push(msg);
                continue;
            }
            let ev = match world::route(ev) {
                world::Routed::World(m) => {
                    self.world_inbox.push(m);
                    continue;
                }
                world::Routed::Color(peer, color) => {
                    self.colors.on_color(peer, color);
                    continue;
                }
                world::Routed::Session(ev, loss) => {
                    // The joining game never takes the world over.
                    let host_gone =
                        matches!(&ev, Event::Disconnected { peer, .. } if peer.is_host());
                    if loss.is_some() && !(host_gone && loss == Some(world::Loss::Left)) {
                        self.world_loss = loss;
                    }
                    ev
                }
            };
            if let Event::Message { from, msg } = &ev
                && self.on_session_message(*from, msg, now)
            {
                continue;
            }
            if let Event::Message { from, msg } = &ev {
                if singularity::is_mine(msg) {
                    singularity_glue::route(*from, msg.clone());
                    continue;
                }
                if self.rules.on_message(*from, msg, now) {
                    continue;
                }
            }
            if let Event::Disconnected { peer, .. } = &ev
                && peer.is_host()
            {
                self.host_gone();
            }
            if let Event::Disconnected { peer, reason } = &ev {
                // Told outside a run's play (before the core forgets it).
                let name = self.core.partners().get(peer).map(|p| p.name.clone());
                self.note_link_lost(*reason, name, now);
                // Both players unready after a link change.
                self.ready_gone();
                // A timeout starts the wait. Any other reason ends the run.
                let interrupted = world::Loss::of(*reason) == world::Loss::Interrupted;
                self.rules.peer_left(*peer, interrupted, now);
            }
            let joined = match &ev {
                Event::Connected { peer, .. } => Some(*peer),
                _ => None,
            };
            let run_left = self.run_left_of(&ev);
            let outs = self.core.handle(ev, now, now_ms);
            net::apply(&self.net, outs);
            if let Some(peer) = run_left {
                self.partner_left_run(peer, now);
            }
            // A newcomer the core turned away (a solo run) never joined.
            if let Some(peer) = joined.filter(|p| self.core.partners().contains_key(p)) {
                // A new link starts unready on both sides; the partner
                // assumes this one is not ready.
                self.ready_gone();
                self.ready_sent = Some(false);
                self.link_notice = None;
                self.rules.peer_joined(peer, now);
                if self.core.is_host() {
                    self.send_session_settings(peer);
                    // Its run settings go out at the next menu or new game
                    // screen frame. A start the partner may have missed goes at once.
                    self.host_sent.reset();
                    if let Some(m) = loadout_glue::start_again(self) {
                        net::apply(&self.net, vec![net::Out::Send(peer, m)]);
                    }
                }
            }
        }
        let outs = self.rules.take_outs();
        net::apply(&self.net, outs);
        self.send_start_count();
        self.sync_ready();
        let linked: Vec<_> = self.core.partners().keys().copied().collect();
        let me = self.core.me();
        let outs = self.colors.outs(me, &linked);
        net::apply(&self.net, outs);
    }

    /// The link line's frame (`link_log`), in a co-op run only.
    fn link_frame(
        &mut self,
        now: Instant,
        stats: Option<&ndml_net::endpoint::Stats>,
        steam: Option<link_log::SteamCounts>,
        silent_for: Option<Duration>,
    ) {
        if !self.core.coop_run() {
            self.link_log.reset();
            return;
        }
        let f = link_log::LinkFrame {
            now,
            net: stats.map(link_log::NetCounts::from),
            steam,
            silent_for,
            waiting: self.core.waiting(),
        };
        if let Some(line) = self.link_log.frame(&f) {
            log::info!("{line}");
        }
    }

    /// The slow-frames line's frame (`slow_log`): the line due, if any, and
    /// this game's role in it.
    fn slow_frame(
        &mut self,
        now: Instant,
        running: bool,
        stats: Option<&ndml_net::endpoint::Stats>,
        steam: Option<link_log::SteamCounts>,
        host_snaps: impl FnOnce() -> Option<(u64, u32)>,
    ) -> Option<(slow_log::Due, &'static str)> {
        let mod_time = self
            .slow_log
            .hooks_timed
            .then(ndml_sdk::profile::take_sides);
        let f = slow_log::SlowFrame {
            now,
            running,
            mod_time,
            drawn_at: self.drawn_at,
        };
        let totals = || slow_log::Totals {
            cpu: cpu_times(),
            net: stats.map(link_log::NetCounts::from),
            steam,
            host_snaps: host_snaps(),
        };
        let due = self.slow_log.frame(&f, totals);
        let want = self.slow_log.timing();
        if want != self.slow_log.hooks_timed {
            ndml_sdk::profile::time_sides(want);
            let _ = ndml_sdk::profile::take_sides();
            self.slow_log.hooks_timed = want;
        }
        let role = match (self.core.coop_run(), self.core.is_host()) {
            (false, _) => "solo",
            (true, true) => "host",
            (true, false) => "joining",
        };
        due.map(|d| (d, role))
    }

    /// A `RunLeft` from a partner still in this run.
    fn run_left_of(&self, ev: &Event) -> Option<PeerId> {
        match ev {
            Event::Message {
                from,
                msg: ndml_net::Message::RunLeft { .. },
            } if !self.core.left_run(*from) => Some(*from),
            _ => None,
        }
    }

    /// The wait's events: the rules count the partner gone during the wait;
    /// a long freeze stays as the shared pause.
    fn wait_events(&mut self, now: Instant) {
        let peer = self.core.last_partner().map(|p| p.0);
        for ev in self.core.take_wait_events() {
            match ev {
                wait::WaitEvent::Lost => {
                    log::info!("{}", self.link_log.lost(now));
                    if let Some(p) = peer {
                        self.rules.peer_left(p, true, now);
                    }
                }
                wait::WaitEvent::Back { long } => {
                    log::info!("{}", self.link_log.back(now, long));
                    self.return_note = Some((now, self.own_state_at_return()));
                    if let Some(p) = peer.filter(|p| self.core.partners().contains_key(p)) {
                        self.rules.peer_joined(p, now);
                    }
                    if long {
                        self.rules.adopt_pause(now);
                    }
                    self.run_events.push(LinkEvent::Back { long });
                }
            }
        }
        let outs = self.rules.take_outs();
        net::apply(&self.net, outs);
        self.return_line(now);
    }

    /// This game's state when a link wait ends, for the return's line.
    fn own_state_at_return(&self) -> String {
        let at = if self.core.run_over_here() || self.rules.run_over() {
            PartnerAtReturn::RunOver
        } else if self.rules.is_down() {
            PartnerAtReturn::Down
        } else {
            PartnerAtReturn::Up
        };
        own_state(self.core.is_host(), self.core.frozen_at_back(), at)
    }

    /// The return's state line, once, [`RETURN_LINE_AFTER`] after a link wait
    /// ends: this game's state then and the partner's since. No names or ids.
    fn return_line(&mut self, now: Instant) {
        let Some((at, own)) = self.return_note.as_ref() else {
            return;
        };
        if now.saturating_duration_since(*at) < RETURN_LINE_AFTER {
            return;
        }
        let partner = match self.core.last_partner().map(|p| p.0) {
            Some(p) if self.core.left_run(p) => PartnerAtReturn::LeftRun,
            Some(p) if self.core.partners().contains_key(&p) => {
                if self.rules.run_over() || self.core.run_over_here() {
                    PartnerAtReturn::RunOver
                } else if self.rules.peer_down(p) {
                    PartnerAtReturn::Down
                } else {
                    PartnerAtReturn::Up
                }
            }
            _ => PartnerAtReturn::Gone,
        };
        log::info!("{}", return_text(own, partner));
        self.return_note = None;
    }

    /// The partner left this co-op run for the menu (its `RunLeft`): the run
    /// is over here too. The rules drop the partner, and on the host the world
    /// lets it go ([`world::Loss::Left`]).
    fn partner_left_run(&mut self, peer: PeerId, now: Instant) {
        if !self.core.left_run(peer) {
            return;
        }
        self.rules.peer_left(peer, false, now);
        if !peer.is_host() {
            self.world_loss = Some(world::Loss::Left);
        }
    }

    /// Host: `SessionSettings` to a partner that just linked.
    fn send_session_settings(&self, peer: PeerId) {
        let (h, d) = (self.settings.health_mult, self.settings.damage_mult);
        // `plays_on` is the fixed `wait::HOST_PLAYS_ON`.
        let plays_on = wait::HOST_PLAYS_ON;
        let respawn = self.settings.rules.respawn;
        let msg = ndml_net::Message::SessionSettings {
            health_mult: h as f32,
            damage_mult: d as f32,
            plays_on_s: plays_on.as_secs_f32(),
            respawn_s: respawn.as_secs_f32(),
        };
        net::apply(&self.net, vec![net::Out::Send(peer, msg)]);
        log::info!(
            "co-op: sent {peer} this host's enemy health x{h}, damage x{d}, plays on alone {plays_on:?} after a loss, respawn {respawn:?}"
        );
    }

    /// The host is gone: its multipliers and run settings no longer count.
    fn host_gone(&mut self) {
        if self.host_mults.take().is_some() {
            log::info!("co-op: host gone; own enemy multipliers");
        }
        if self.loadout.shares() {
            log::info!(
                "co-op: host gone; its run settings are forgotten (this player's picks come back outside a run)"
            );
        }
        self.loadout.host_gone();
        self.loadout_screen.note = None;
    }

    /// A link that was up went while this player is outside a run's play: the
    /// notice that says so. Nothing when this player dropped it.
    fn note_link_lost(
        &mut self,
        reason: ndml_net::endpoint::DisconnectReason,
        name: Option<String>,
        now: Instant,
    ) {
        let Some(raw) = name else { return };
        if self.core.coop_run() && self.end_shown.is_none() {
            return;
        }
        let partner = ui::text::display_name(&raw, &self.lang, ui::text::FALLBACK_PARTNER);
        if let Some(line) = ui::run::lost_link_line(reason, &partner) {
            log::info!("co-op: the link went outside a run's play ({reason}): the player is told");
            self.link_notice = Some(ui::run::LinkNotice::new(line, now));
        }
    }

    /// The link changed: neither player is ready, no notice.
    fn ready_gone(&mut self) {
        self.loadout.link_changed();
        self.ready_notice.clear();
        self.ready_sent = None;
    }

    /// This player's Ready out to the partner whenever it changed, on the
    /// reliable channel.
    fn sync_ready(&mut self) {
        if !self.core.linked() {
            self.ready_sent = None;
            return;
        }
        let ready = self.loadout.ready();
        // A partner that has heard nothing in this link takes "not ready".
        let heard = *self.ready_sent.get_or_insert(false);
        if heard == ready {
            return;
        }
        self.ready_sent = Some(ready);
        log::info!(
            "co-op: told the partner this player is {}",
            if ready { "ready" } else { "not ready" }
        );
        net::apply(
            &self.net,
            vec![net::Out::Broadcast(ndml_net::Message::Ready { ready })],
        );
    }

    /// The start count's messages (`loadout::StartCount`) in order, on the
    /// reliable channel.
    pub(crate) fn send_start_count(&mut self) {
        let outs = self.loadout.take_start_outs();
        if outs.is_empty() || !self.core.linked() {
            return;
        }
        let outs = outs
            .into_iter()
            .map(|(round, step)| net::Out::Broadcast(ndml_net::Message::StartCount { round, step }))
            .collect();
        net::apply(&self.net, outs);
    }

    /// The partner's `Ready`: its state, and on a new Ready the notice and
    /// its cue.
    fn on_partner_ready(&mut self, ready: bool) {
        let was = self.loadout.partner_ready();
        let news = self.loadout.partner(ready);
        if was != self.loadout.partner_ready() {
            log::info!(
                "co-op: the partner is {}",
                if ready {
                    "ready on the new game screen"
                } else {
                    "not ready any more"
                }
            );
        }
        if news {
            let name = loadout_glue::partner_shown(self);
            self.ready_notice.start(loadout::partner_ready_alert(&name));
        } else if !ready {
            self.ready_notice.clear();
        }
    }

    /// A session message this layer takes itself (`SessionSettings`,
    /// `RunRules`, `Ready`, `StartCount`).
    fn on_session_message(&mut self, from: PeerId, msg: &ndml_net::Message, now: Instant) -> bool {
        if let ndml_net::Message::Ready { ready } = *msg {
            self.on_partner_ready(ready);
            return true;
        }
        if let ndml_net::Message::StartCount { round, step } = *msg {
            let host = self.core.is_host();
            self.loadout
                .start_msg(host, from.is_host(), round, step, now);
            return true;
        }
        if let ndml_net::Message::RunRules {
            start,
            mods,
            daily,
            on_loadout,
        } = msg
        {
            if from.is_host() && !self.core.is_host() {
                loadout_glue::on_rules(self, *start, *mods, daily.clone(), *on_loadout);
            }
            return true;
        }
        let ndml_net::Message::SessionSettings {
            health_mult,
            damage_mult,
            plays_on_s,
            respawn_s,
        } = *msg
        else {
            return false;
        };
        self.on_session_settings(from, (health_mult, damage_mult));
        self.on_session_plays_on(from, plays_on_s);
        self.on_session_respawn(from, respawn_s);
        true
    }

    /// A joining game: the host's respawn time replaces its file's.
    fn on_session_respawn(&mut self, from: PeerId, respawn_s: f32) {
        if !from.is_host() || self.core.is_host() {
            return;
        }
        let own = self.settings.rules.respawn;
        let r = scaling::host_respawn(respawn_s).unwrap_or(own);
        self.host_respawn = Some(r);
        self.rules.set_respawn(r);
        log::info!("co-op: the host's respawn time {r:?} applies (this game's file: {own:?})");
    }

    /// A joining game: the host's multipliers, used while linked to it.
    fn on_session_settings(&mut self, from: PeerId, (health, damage): (f32, f32)) {
        if !from.is_host() || self.core.is_host() {
            return;
        }
        self.host_mults = Some((health, damage));
        let own = (self.settings.health_mult, self.settings.damage_mult);
        let (h, d) = scaling::session_mults(own, self.host_mults);
        if (h - own.0).abs() > 1e-6 || (d - own.1).abs() > 1e-6 {
            log::info!(
                "co-op: the host's enemy health x{h}, damage x{d} apply while linked (this game's file: x{}, x{})",
                own.0,
                own.1
            );
        } else {
            log::info!("co-op: the host's enemy health x{h}, damage x{d} (as this game's file)");
        }
    }

    /// A joining game: the host's plays-on time from its `SessionSettings`,
    /// logged only.
    fn on_session_plays_on(&self, from: PeerId, plays_on_s: f32) {
        if !from.is_host() || self.core.is_host() {
            return;
        }
        let p = (plays_on_s.is_finite() && plays_on_s >= 0.0)
            .then(|| Duration::from_secs_f32(plays_on_s.min(600.0)));
        log::info!("co-op: the host plays on alone {p:?} after a loss");
    }

    /// At a menu frame without a link: the host's respawn time and multipliers are forgotten.
    fn forget_host_session(&mut self) {
        if self.core.linked() {
            return;
        }
        if self.host_respawn.take().is_some() {
            self.rules.set_respawn(self.settings.rules.respawn);
            log::info!("co-op: no host now; this game's own respawn time again");
        }
        self.host_mults = None;
    }

    /// The multipliers in effect: the host's while linked to it, else own.
    fn mults(&self) -> (f64, f64) {
        let own = (self.settings.health_mult, self.settings.damage_mult);
        let host = self
            .host_mults
            .filter(|_| self.core.linked() && !self.core.is_host());
        scaling::session_mults(own, host)
    }
}

/// Shared by every hook.
#[derive(Debug)]
pub(crate) struct Shared {
    coop: Mutex<Coop>,
    flags: Flags,
    pub(crate) h: Handles,
    pub(crate) ui: ui::glue::UiHandles,
    pub(crate) world: Arc<world::glue::WorldHooks>,
    /// The generic player-side mirror.
    pub(crate) mirror: Arc<mirror::glue::MirrorHooks>,
    /// The Steam connector (`None` with `connector = "udp"`).
    pub(crate) steam: Option<Arc<steam::SteamCoop>>,
    /// The session rules' handles and hot-path flags.
    pub(crate) rules: rules::glue::RulesHooks,
    /// `wild_mods`: the Wild Metamorphosis value put aside for one event.
    pub(crate) wild: wild::Pending,
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Coop> {
        game::lock_or_report(&self.coop, "session")
    }

    fn update_flags(&self, c: &Coop) {
        let (scaling, protect) = c.core.switches();
        self.flags.set(scaling, protect);
        self.flags.set_start_held(c.core.start_held());
        self.flags.set_linked(c.core.linked());
        let (health, damage) = c.mults();
        self.flags.set_mults(health, damage);
    }
}

fn hook_or_warn(name: &str, r: Result<(), Error>) -> bool {
    match r {
        Ok(()) => true,
        Err(e) => {
            log::warn!("co-op: unable to hook {name} ({e})");
            loadout_glue::note_failed(name);
            false
        }
    }
}

#[allow(clippy::too_many_lines)] // one block per subsystem, in start-up order
fn init(ctx: &ModContext) -> Result<(), Error> {
    let cfg: Config = config::load(ctx)?;
    let (settings, warnings) = cfg.validate();
    let partner_sounds = settings.partner_sound_volume;
    for w in &warnings {
        log::warn!("config: {w}");
    }
    guard::init(ctx);
    // The wall clock against this log's clock.
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        log::info!("co-op wall clock: unix ms {}", d.as_millis());
    }
    let ndml_root = ctx.config_dir().parent().unwrap_or(ctx.config_dir());
    let (steam_transport, steam_bridge) = if settings.steam.enabled() {
        let (t, b) =
            ndml_net::steam::channel(ndml_net::steam::OUT_CAPACITY, ndml_net::steam::IN_CAPACITY);
        (Some(t), Some(b))
    } else {
        (None, None)
    };
    let started = match net::start_with_steam(&settings, ndml_root, Instant::now(), steam_transport)
    {
        Ok(s) => s,
        Err(e) => {
            log::error!("co-op: unable to open the network socket ({e})");
            return Ok(());
        }
    };
    let net = started.worker;
    let steam = steam_bridge.map(|bridge| {
        Arc::new(steam::SteamCoop::new(
            ctx,
            settings.steam.clone(),
            net.handle(),
            bridge,
            net::COOP_VERSION,
            started.build.clone(),
        ))
    });
    let h = Handles::resolve(ctx);
    let ui_handles = ui::glue::UiHandles::resolve(ctx);
    let world = Arc::new(world::glue::WorldHooks::new(ctx, settings.world.clone()));
    let mirror = Arc::new(mirror::glue::MirrorHooks::new(ctx, settings.mirror.clone()));
    let labels = LabelMode::from_flags(settings.world.show_names, settings.world.show_marker);
    let shared = Arc::new(Shared {
        flags: Flags::new(settings.health_mult, settings.damage_mult),
        coop: Mutex::new(Coop {
            core: Core::new(config::EXTRAPOLATION),
            page: {
                let mut page = Page::new(
                    &settings.name,
                    world::color::page_choices(&cfg.color),
                    labels,
                );
                page.set_wild(settings.wild_mods);
                page.lan.name_auto = settings.name_alt.is_some();
                page
            },
            menu: ui::glue::MenuState::default(),
            steam: steam.as_ref().map_or_else(
                || Box::new(NotYet::udp()) as Box<dyn SteamLink>,
                |s| Box::new(CoopSteam(Arc::clone(s))),
            ),
            colors: world::color::ColorSync::new(settings.world.color),
            labels,
            run_msgs: RunMessages::default(),
            silence: SilenceWatch::default(),
            silent_for: None,
            link_log: link_log::LinkLog::default(),
            slow_log: slow_log::SlowLog::default(),
            drawn_at: None,
            over_quit: false,
            run_ui: RunUi::default(),
            run_ready: RunUi::default(),
            end_place: None,
            link_notice: None,
            return_note: None,
            end_shown: None,
            run_events: Vec::new(),
            last_partner: String::new(),
            label_partners: Vec::new(),
            draw_style: None,
            lang: "english".into(),
            config_path: ctx.config_dir().join(format!("{}.toml", ctx.name())),
            world_inbox: Vec::new(),
            mirror_inbox: Vec::new(),
            world_loss: None,
            rules: rules::Rules::new(settings.rules.clone()),
            host_mults: None,
            host_respawn: None,
            loadout: loadout::Joiner::default(),
            loadout_screen: loadout_glue::Screen::default(),
            host_sent: loadout::HostSent::default(),
            ready_sent: None,
            ready_notice: ui::run::ReadyNotice::default(),
            settings,
            net,
        }),
        h,
        ui: ui_handles,
        world,
        mirror,
        steam,
        rules: rules::glue::RulesHooks::resolve(ctx),
        wild: wild::Pending::default(),
    });
    register(ctx, &shared);
    loadout_glue::register(ctx, &shared);
    wild_glue::register(ctx, &shared);
    rules::glue::register(ctx, &shared);
    rules::glue::register_score(ctx, &shared);
    world::glue::register(ctx, &shared.world);
    singularity_glue::register(ctx, &shared);
    sound::glue::register(ctx, partner_sounds, &shared.world);
    if shared.world.is_on() {
        let m = Arc::clone(&shared.mirror);
        shared
            .world
            .dmg
            .set_player_side(damage::glue::PlayerSide(Box::new(move |call, inst| {
                m.is_player_side(call, inst)
            })));
        let m = Arc::clone(&shared.mirror);
        shared
            .world
            .set_shot_side(damage::glue::PlayerSide(Box::new(move |call, inst| {
                m.is_player_side(call, inst)
            })));
        let (m, m2, m3) = (
            Arc::clone(&shared.mirror),
            Arc::clone(&shared.mirror),
            Arc::clone(&shared.mirror),
        );
        shared.world.set_thing_links(world::glue::ThingLinks {
            key_of: Box::new(move |inst| {
                m.mirror_key_of(inst)
                    .map(|(owner, key)| ndml_net::shots::ThingRef { owner, key })
            }),
            proxy: Box::new(move |t| m2.proxy_for(t.owner, t.key)),
            proxies_of: Box::new(move |objects| m3.proxies_of(objects)),
        });
        damage::glue::register(ctx, &shared.world.dmg);
    }
    mirror::glue::MirrorHooks::register(ctx, &shared.mirror);
    if let Some(s) = &shared.steam {
        let sites = steam::register(ctx, s);
        log::info!("co-op steam: ready ({sites} hook sites)");
    } else {
        log::info!("co-op steam: off: connector = \"udp\"");
    }
    // The host's run settings: on only if every hook they need registered.
    loadout_glue::decide(&shared);
    {
        let c = shared.lock();
        log::info!("co-op: ready: tint {:?}", c.settings.ui.tint);
    }
    Ok(())
}

fn register(ctx: &ModContext, shared: &Arc<Shared>) {
    register_menu(ctx, shared);
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    hook_or_warn(
        names::CONTROLLER_STEP,
        ctx.hook_after(names::CONTROLLER_STEP, move |call| run_step(call, &c1)),
    );
    hook_or_warn(
        names::CONTROLLER_DRAW_GUI,
        ctx.hook(
            names::CONTROLLER_DRAW_GUI,
            move |call| ui::glue::run_draw_before(call, &c2),
            {
                let c = Arc::clone(shared);
                move |call, saved| run_draw(call, &c, saved)
            },
        ),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::CONFIRM_PAUSE,
        ctx.hook_before(names::CONFIRM_PAUSE, move |call| {
            ui::glue::confirm_pause_before(call, &c);
        }),
    );
    ui::glue::set_run_pieces(shared);
    let c = Arc::clone(shared);
    hook_or_warn(
        world::names::CONTROLLER_END_STEP,
        ctx.hook_before(world::names::CONTROLLER_END_STEP, move |call| {
            // What a downed ship made in this frame's Steps, before the draw.
            c.rules.downed.settle(call);
            if let Some(moved) = c.world.ending_end_step(call) {
                c.mirror.move_shifted(call, moved);
            }
        }),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::PLAYER_STEP,
        ctx.hook_after(names::PLAYER_STEP, move |call| player_step(call, &c)),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::POWERS_STEP,
        ctx.hook_after(names::POWERS_STEP, move |call| powers_step(call, &c)),
    );
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    hook_or_warn(
        names::DRAW_END_SCORE_CREATE,
        ctx.hook(
            names::DRAW_END_SCORE_CREATE,
            move |call| {
                rules::glue::end_screen_before(call, &c1);
                block_leaderboards(call, &c1)
            },
            move |call, saved| restore_leaderboards(call, &c2, saved),
        ),
    );
    for name in [names::TRIGGER_SAVE_RUN, names::DELETE_RUN_FILES] {
        let c = Arc::clone(shared);
        hook_or_warn(
            name,
            ctx.hook_before(name, move |call| {
                // A co-op run, a Start taken while linked (to its run's first
                // tick), or the host's run settings held.
                if c.flags.resume_writes_off() {
                    call.skip_original();
                    info_once(
                        name,
                        &format!("co-op run: skipping {name} (solo resume file kept)"),
                    );
                }
            }),
        );
    }
    register_teardown(ctx, shared);
    let c = Arc::clone(shared);
    hook_or_warn(
        names::ENEMY_INIT,
        ctx.hook_after(names::ENEMY_INIT, move |call| enemy_init(call, &c)),
    );
    register_damage_scaling(ctx, shared);
}

/// `enemyDamageScaling`: the session's damage multiplier on its result.
fn register_damage_scaling(ctx: &ModContext, shared: &Arc<Shared>) {
    let c = Arc::clone(shared);
    hook_or_warn(
        names::ENEMY_DAMAGE_SCALING,
        ctx.hook_after(names::ENEMY_DAMAGE_SCALING, move |call| {
            let Some(dmg) = call.result().and_then(|v| v.as_f64()) else {
                return;
            };
            let out = c.flags.damage_mult().and_then(|mult| {
                let out = scaled(dmg, mult)?;
                match call.set_return(out) {
                    Ok(()) => info_once(
                        "damage-scaled",
                        &format!("co-op: first scaled enemy damage {dmg} -> {out} (x{mult})"),
                    ),
                    Err(e) => warn_once("damage-scaling", &format!("co-op: damage scaling: {e}")),
                }
                Some(out)
            });
            let _ = out;
        }),
    );
}

/// The main-menu hooks. The Co-op row is only turned on when every one of
/// them is in place.
fn register_menu(ctx: &ModContext, shared: &Arc<Shared>) {
    let mut missing = Vec::new();
    let mut need = |name: &'static str, hooked: bool| {
        if !hooked {
            missing.push(name);
        }
    };
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    need(
        names::CONFIRM_MENU,
        hook_or_warn(
            names::CONFIRM_MENU,
            ctx.hook(
                names::CONFIRM_MENU,
                move |call| ui::glue::confirm_menu_before(call, &c1),
                move |call, saved| ui::glue::confirm_menu_after(call, &c2, saved),
            ),
        ),
    );
    let c = Arc::clone(shared);
    need(
        names::MENU_CREATE,
        hook_or_warn(
            names::MENU_CREATE,
            ctx.hook_after(names::MENU_CREATE, move |call| {
                ui::glue::menu_create_after(call, &c);
            }),
        ),
    );
    let c = Arc::clone(shared);
    need(
        names::MENU_STEP,
        hook_or_warn(
            names::MENU_STEP,
            ctx.hook_before(names::MENU_STEP, move |call| menu_step(call, &c)),
        ),
    );
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    need(
        names::MENU_END_STEP,
        hook_or_warn(
            names::MENU_END_STEP,
            ctx.hook(
                names::MENU_END_STEP,
                move |call| ui::glue::menu_step2_before(call, &c1),
                move |call, ()| ui::glue::menu_step2_after(call, &c2),
            ),
        ),
    );
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    need(
        names::MENU_DRAW_GUI,
        hook_or_warn(
            names::MENU_DRAW_GUI,
            ctx.hook(
                names::MENU_DRAW_GUI,
                move |call| ui::glue::menu_draw_before(call, &c1),
                move |call, saved| ui::glue::menu_draw_after(call, &c2, saved),
            ),
        ),
    );
    ui::glue::set_row_pieces(shared, missing);
}

/// The menu tick. It never skips `oMenu_Step_0` (it runs `steam_update()`).
fn menu_step(call: &mut Call<'_>, s: &Shared) {
    let now = Instant::now();
    // The partner's new Ready, or a cue's next notes (`ui::cues`).
    let cue = s.lock().ready_notice.take_cue();
    if cue {
        ui::glue::play_cue(call, s, ui::cues::Cue::PartnerReady, now);
    } else {
        ui::glue::cue_tick(call, s, now);
    }
    {
        let mut c = s.lock();
        c.pump(now);
        let outs = c.core.menu_frame();
        net::apply(&c.net, outs);
        c.world_inbox.clear();
        c.world_loss = None;
        c.run_events.clear();
        c.run_msgs.reset();
        c.run_ui = RunUi::default();
        c.end_shown = None;
        c.over_quit = false;
        c.rules.end_run();
        c.forget_host_session();
        s.rules.deactivate();
        s.update_flags(&c);
    }
    wild_glue::restore_pending(call, s);
    // The run settings: this player's picks back, the host's out.
    loadout_glue::menu_frame(call, s);
    s.world.menu_frame();
    ui::glue::menu_tick(call, s);
}

fn read_mode(call: &Call<'_>, h: &Handles) -> (u32, String) {
    let g = call.global();
    let fields: Vec<(&str, Option<f64>)> = h
        .mode
        .iter()
        .map(|(n, v)| (*n, g.as_ref().and_then(|g| num(g, *v))))
        .collect();
    (gate::mode_hash(&fields), gate::mode_summary(&fields))
}

fn read_view(call: &Call<'_>, h: &Handles) -> Option<View> {
    let globals = call.global()?;
    let [left, top, width, height] = h.view.map(|var| num(&globals, var));
    let view = View {
        x: left?,
        y: top?,
        w: width?,
        h: height?,
    };
    (view.w > 1.0 && view.h > 1.0).then_some(view)
}

/// The process's CPU time and this thread's, for the slow-frames line.
/// `None` where the system refuses.
fn cpu_times() -> Option<(Duration, Duration)> {
    let p = cpu_time::ProcessTime::try_now().ok()?;
    let t = cpu_time::ThreadTime::try_now().ok()?;
    Some((p.as_duration(), t.as_duration()))
}

#[allow(clippy::too_many_lines)] // the run step's parts in their order
fn run_step(call: &mut Call<'_>, s: &Shared) {
    // A Wild Metamorphosis value put aside (`wild`), before anything reads it.
    wild_glue::restore_pending(call, s);
    let now = Instant::now();
    let Some(me) = call.instance() else { return };
    let paused = num(&me, s.h.pause).is_some_and(|p| p != 0.0);
    // Before the session's lock (the world's is never taken inside it).
    let frozen = s.world.was_frozen();
    let frame = RunFrame {
        key: me.key(),
        stamp: num(&me, s.h.time_game_start),
        paused,
        gameplay: truthy(&me, s.h.gameplay).unwrap_or(false),
        game_over: truthy(&me, s.h.game_over).unwrap_or(false),
        view: read_view(call, &s.h),
        mode: read_mode(call, &s.h),
        silent_for: None,
        woke: None,
        upgrade_screen: false,
        rules_hold: false,
    };
    let run = (frame.key, frame.stamp);
    // The Steam connector's counts, before this lock (the connector has its own).
    let steam_counts = s.steam.as_ref().and_then(|st| st.link_counts());
    let (action, input, slow) = {
        let mut c = s.lock();
        c.pump(now);
        let now_ms = ndml_net::time::to_wire_ms(c.net.local_time(now));
        // The partner's silence, every frame (paused too: keepalives flow).
        let linked = c.core.linked();
        let stats = c
            .core
            .partners()
            .keys()
            .next()
            .and_then(|&p| c.net.peer_info(p))
            .filter(|i| linked && i.connected)
            .map(|i| i.stats);
        let silent_for = c.silence.update(stats.map(|s| s.packets_recv), now);
        c.silent_for = silent_for;
        let mut frame = frame.clone();
        frame.silent_for = silent_for;
        frame.woke = c.silence.woke();
        frame.upgrade_screen = c.rules.upgrade_choosing();
        frame.rules_hold = c.rules.holds_game();
        let (outs, action) = c.core.run_frame(&frame, now, now_ms);
        net::apply(&c.net, outs);
        c.link_frame(now, stats.as_ref(), steam_counts, silent_for);
        // Running play: a run on screen, not paused, not held or frozen.
        let running = frame.gameplay
            && !frame.paused
            && !frame.game_over
            && !frozen
            && !frame.rules_hold
            && !frame.upgrade_screen
            && !c.core.waiting();
        let slow = c.slow_frame(now, running, stats.as_ref(), steam_counts, || {
            s.world.host_snapshots()
        });
        c.wait_events(now);
        s.update_flags(&c);
        // The session rules must not take the gate's pauses for a player's.
        match action {
            FreezeAction::Pause => c.rules.note_own(true),
            FreezeAction::Unpause => c.rules.note_own(false),
            FreezeAction::None => {}
        }
        // The honour rule's window (its own lock, never taken with this one).
        s.world.dmg.set_rtts(&c.partner_rtts());
        (action, world_input(&mut c, run, now, now_ms), slow)
    };
    if let Some((due, role)) = slow {
        // Read now, outside the session's lock.
        let (wave, enemies, enemy_shots) = s.world.slow_scene(call);
        let (own, partner_mirrored) = s.mirror.slow_counts();
        let scene = slow_log::Scene {
            role,
            wave,
            enemies,
            enemy_shots,
            partner_mirrored,
            own,
        };
        log::info!("{}", due.line(&scene));
    }
    hold(call, s, action);
    over_step(call, s, paused, now);
    rules::glue::run_tick(
        call,
        s,
        rules::glue::Tick {
            key: run,
            paused,
            gameplay: frame.gameplay,
            game_over: frame.game_over,
        },
        now,
    );
    rules::glue::start_ask_step(call, s, paused);
    let msgs = s.world.run_step(call, input);
    if !msgs.is_empty() {
        let c = s.lock();
        for m in msgs {
            c.net.broadcast(m, None);
        }
    }
    let run = (run.0, run.1.map(f64::to_bits));
    mirror_step(call, s, now, run);
}

/// The generic mirror's tick: own player-side instances out, the partner's drawn.
fn mirror_step(call: &Call<'_>, s: &Shared, now: Instant, run: (usize, Option<u64>)) {
    let (input, inbox) = {
        let mut c = s.lock();
        // A partner who left the run (`RunLeft`) is no partner here.
        let partner = c
            .core
            .partners()
            .keys()
            .copied()
            .find(|&p| !c.core.left_run(p));
        let shown_ms = partner
            .and_then(|p| partner_render_time(&c, p, now))
            .map(ndml_net::time::to_wire_ms);
        let input = mirror::glue::MirrorInput {
            active: c.core.coop_run() && partner.is_some(),
            run,
            me: c.core.me(),
            now_ms: ndml_net::time::to_wire_ms(c.net.local_time(now)),
            shown_ms,
            frozen: s.world.was_frozen(),
        };
        (input, std::mem::take(&mut c.mirror_inbox))
    };
    let msgs = s.mirror.run_step(call, input, inbox, &*s.world);
    if !msgs.is_empty() {
        let c = s.lock();
        for m in msgs {
            c.net.broadcast(m, None);
        }
    }
}

/// When to draw a partner (partner clock), behind by an adaptive delay.
fn partner_render_time(c: &Coop, peer: PeerId, now: Instant) -> Option<Duration> {
    c.net.render_time(peer, now, player::STATE_PERIOD)
}

/// Everything the shared world needs from the session this frame; the
/// partners are coloured here (`partner_tint`).
fn world_input(
    c: &mut Coop,
    run: (usize, Option<f64>),
    now: Instant,
    now_ms: u32,
) -> world::glue::WorldInput {
    let times: Vec<(PeerId, Option<Duration>)> = c
        .core
        .partners()
        .keys()
        .map(|&p| (p, partner_render_time(c, p, now)))
        .collect();
    let render = |p: PeerId| times.iter().find(|t| t.0 == p).and_then(|t| t.1);
    let mut partners = world::partner_views(&mut c.core, now, &render, &c.colors);
    if c.rules.separated() {
        // Handed over for this run: a partner whose link came back plays in
        // another world (no avatar, no label).
        partners.clear();
    }
    let chosen = partners.first().and_then(|p| p.color);
    let own_blend = c.core.own_look().map(|(_, l)| l.blend);
    c.label_partners = present::style_partners(&mut partners, c.settings.ui.tint, own_blend);
    c.draw_style = c.label_partners.first().map(|l| {
        let accent = l.color.unwrap_or(ui::style::color::BLUE);
        let ship = match c.settings.ui.tint {
            present::PartnerTint::Off | present::PartnerTint::Native => Some(l.look.blend),
            present::PartnerTint::Chosen => chosen,
        };
        (ship, accent)
    });
    world::glue::WorldInput {
        run: Some(run),
        coop_run: c.core.coop_run(),
        linked: c.core.run_linked(),
        is_host: c.core.is_host(),
        me: c.core.me(),
        gate_running: c.core.gate_phase() == gate::GatePhase::Running,
        loss: c.world_loss.take(),
        now,
        now_ms,
        render_time: c
            .net
            .render_time(PeerId::HOST, now, ndml_net::clock::SNAPSHOT_INTERVAL),
        view: c.core.view(),
        partners,
        own_cloaked: c.core.local_powers().is_some_and(|p| p.cloaked),
        own_down: c.rules.is_down(),
        inbox: std::mem::take(&mut c.world_inbox),
        sps: 60.0,
        host_now: c
            .net
            .peer_info(PeerId::HOST)
            .filter(|i| i.clock.is_synced())
            .map(|i| i.clock.render_time(c.net.local_time(now), Duration::ZERO)),
    }
}

/// Holds the game at the start gate with the normal pause (`pressPause` /
/// `pressUnpause`, called as `oController`).
fn hold(call: &Call<'_>, s: &Shared, action: FreezeAction) {
    let script = match action {
        FreezeAction::None => return,
        FreezeAction::Pause => s.h.press_pause,
        FreezeAction::Unpause => s.h.press_unpause,
    };
    let Some(script) = script else {
        warn_once("hold", "co-op: unable to find pressPause/pressUnpause");
        return;
    };
    log::debug!("co-op: {action:?} for the start gate");
    if let Err(e) = rules::glue::own_call(s, || call.call_script(script, &[])) {
        warn_once(
            "hold-call",
            &format!("co-op: unable to hold the game at the start gate ({e})"),
        );
    }
}

/// The co-op run is over (the partner left it, either role): after
/// [`state::OVER_HOLD`] paused, the game leaves for the menu as Leave Co-op
/// Run does (`pressQuit`).
fn over_step(call: &Call<'_>, s: &Shared, paused: bool, now: Instant) {
    let go = {
        let mut c = s.lock();
        let due = c
            .core
            .run_over()
            .is_some_and(|at| now.saturating_duration_since(at) >= state::OVER_HOLD);
        let go = due && paused && !c.over_quit;
        if go {
            c.over_quit = true;
        }
        go
    };
    if !go {
        return;
    }
    let Some(script) = s.h.press_quit else {
        warn_once("over-quit", "co-op: unable to find pressQuit");
        return;
    };
    log::info!("co-op: the partner left; the run is over, leaving for the menu");
    if let Err(e) = rules::glue::own_call(s, || call.call_script(script, &[])) {
        warn_once(
            "over-quit-call",
            &format!("co-op: unable to leave the run ({e})"),
        );
    }
}

/// `oController_Draw_64` after: partner labels/markers (or draw-path ships)
/// and the run UI chosen in the `before`.
fn run_draw(call: &mut Call<'_>, s: &Shared, saved: ui::glue::Swapped) {
    let now = Instant::now();
    let active = {
        let mut c = s.lock();
        c.drawn_at = Some(now);
        c.core.linked() || c.core.coop_run()
    };
    if !active {
        ui::glue::run_paint(call, s, saved, Vec::new());
        return;
    }
    let Some(gui) = gui_size(call, &s.h.b) else {
        ui::glue::run_paint(call, s, saved, Vec::new());
        return;
    };
    let lh = ui::glue::label_line(call, s);
    let screen = camera_view(call, &s.h.b);
    if screen.is_none() {
        warn_once(
            "camera",
            "co-op: no camera view; partners are drawn in arena coordinates",
        );
    }
    let end = run_end_shown(call, s);
    // Which post-run screen the partner's Ready is drawn on, while it shows.
    let end_place = (end == Some(END_GAME_OVER) && !s.lock().run_ready.bars.is_empty())
        .then(|| ui::glue::end_place(call, s));
    let items = {
        let mut guard = s.lock();
        let c = &mut *guard;
        let (names, marker) = if end.is_some() {
            (false, false)
        } else {
            (c.labels.names(), c.labels.marker())
        };
        let net = &c.net;
        let render = |peer| net.render_time(peer, now, player::STATE_PERIOD);
        // Avatars: the game draws the ships; only labels and markers here.
        let items = if s.world.partner_items(gui, screen, lh).is_some() {
            present::labels(&c.label_partners, screen, gui, lh, names, marker)
        } else {
            let drawn = if c.rules.separated() {
                Vec::new() // another world's ship (see `world_input`)
            } else {
                c.core.draw_partners(now, gui, screen, lh, &render)
            };
            let (ship, accent) = c.draw_style.unwrap_or((None, ui::style::color::BLUE));
            present::restyle_split(drawn, names, marker, ship, accent)
        };
        if c.end_shown != end {
            match end {
                Some(why) => log::info!(
                    "co-op: the run's end ({why}): partner labels, markers and run messages off"
                ),
                None => log::info!("co-op: partner labels, markers and run messages back on"),
            }
            c.end_shown = end;
        }
        let ready = std::mem::take(&mut c.run_ready);
        if end.is_some() {
            // Only the partner's Ready is drawn on the death and end screen.
            c.run_ui = if end == Some(END_GAME_OVER) {
                ready.clone()
            } else {
                RunUi::default()
            };
        }
        c.end_place = end_place;
        // The partner's Ready only (a lost link's notice there says its own).
        if !ready.bars.is_empty()
            && (end.is_none() || end == Some(END_GAME_OVER))
            && c.core.linked()
            && c.loadout.partner_ready()
        {
            let place = if end.is_some() {
                end_place.unwrap_or(ui::run::ReadyPlace::EndScreen)
            } else {
                ui::run::ReadyPlace::Run
            };
            let text = ready.bars[0].text.clone();
            ui::glue::ready_shown(c, place, &text);
        }
        items
    };
    ui::glue::run_paint(call, s, saved, items);
}

/// The game's end of a run is on screen (the ending cutscene, the death and
/// end screen): why, or `None`. Co-op draws nothing of its own over it.
fn run_end_shown(call: &Call<'_>, s: &Shared) -> Option<&'static str> {
    let flag = |inst: Option<ndml_sdk::Instance<'_>>, v| {
        inst.and_then(|i| i.get_value(v?))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };
    run_end_reason(
        flag(call.global(), s.h.game_ending),
        flag(call.global(), s.h.hide_gui),
        flag(call.instance(), s.h.game_over),
    )
}

/// [`run_end_reason`] for the death and end screen.
const END_GAME_OVER: &str = "game over";

/// [`run_end_shown`]'s rule.
fn run_end_reason(game_ending: bool, hide_gui: bool, game_over: bool) -> Option<&'static str> {
    if game_over {
        Some(END_GAME_OVER)
    } else if game_ending {
        Some("the ending")
    } else if hide_gui {
        Some("the game hides its HUD")
    } else {
        None
    }
}

fn player_step(call: &mut Call<'_>, s: &Shared) {
    let now = Instant::now();
    let Some(me) = call.instance() else { return };
    // Downed or reviving: written before the ship is read and sent.
    rules::glue::player_step(call, &me, s);
    let read = |n: &str| me.builtin_f64(n);
    let (Some(x), Some(y)) = (read(bv::X), read(bv::Y)) else {
        warn_once("ship-xy", "co-op: unable to read the ship's position");
        return;
    };
    let ship = ShipRead {
        x,
        y,
        hspeed: read(bv::HSPEED).unwrap_or(0.0),
        vspeed: read(bv::VSPEED).unwrap_or(0.0),
        angle: read(bv::IMAGE_ANGLE).unwrap_or(0.0),
        sprite: me
            .builtin_value(bv::SPRITE_INDEX)
            .ok()
            .as_ref()
            .and_then(sprite_index),
        frame: read(bv::IMAGE_INDEX).unwrap_or(0.0),
        xscale: read(bv::IMAGE_XSCALE).unwrap_or(1.0),
        yscale: read(bv::IMAGE_YSCALE).unwrap_or(1.0),
        blend: read(bv::IMAGE_BLEND).unwrap_or(f64::from(0x00FF_FFFFu32)),
        alpha: read(bv::IMAGE_ALPHA).unwrap_or(1.0),
        visible: read(bv::VISIBLE).is_none_or(|v| v > 0.5),
    };
    let sps = read(bv::ROOM_SPEED).unwrap_or(60.0);
    s.world
        .player_step(&me, x, y, ship.hspeed, ship.vspeed, ship.visible);
    let mut c = s.lock();
    let time_ms = ndml_net::time::to_wire_ms(c.net.local_time(now));
    let outs = c.core.local_ship(&ship, sps, time_ms, now);
    net::apply(&c.net, outs);
}

fn powers_step(call: &mut Call<'_>, s: &Shared) {
    let Some(me) = call.instance() else { return };
    let p = PowersRead {
        hull: num(&me, s.h.hull).unwrap_or(0.0),
        hull_max: num(&me, s.h.hull_max).unwrap_or(0.0),
        shield: num(&me, s.h.shield).unwrap_or(0.0),
        cloaked: truthy(&me, s.h.cloaked).unwrap_or(false),
    };
    s.lock().core.set_powers(p);
    rules::glue::powers_step(call, &me, s);
}

/// Inside the game's own `destroyDataStructures` (see [`register_teardown`]).
static TEARDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The menu teardown destroys `oLog`'s `waveLog`, which `oLog`'s `CleanUp` would destroy again
/// (an error dialog): after a co-op run, `oLog_CleanUp_0` is skipped inside that teardown.
/// Also the Game End goodbye.
fn register_teardown(ctx: &ModContext, shared: &Arc<Shared>) {
    use std::sync::atomic::Ordering;
    let g = Arc::clone(shared);
    hook_or_warn(
        names::GAME_END_IN_RUN,
        ctx.hook_before(names::GAME_END_IN_RUN, move |call| game_closing(call, &g)),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::DESTROY_DATA_STRUCTURES,
        ctx.hook(
            names::DESTROY_DATA_STRUCTURES,
            |_| TEARDOWN.store(true, Ordering::Relaxed),
            |_, ()| TEARDOWN.store(false, Ordering::Relaxed),
        ),
    );
    hook_or_warn(
        names::LOG_CLEANUP,
        ctx.hook_before(names::LOG_CLEANUP, move |call| {
            if TEARDOWN.load(Ordering::Relaxed) && c.flags.save_protect() {
                call.skip_original();
                info_once(
                    names::LOG_CLEANUP,
                    "teardown: oLog's CleanUp skipped (waveLog already destroyed by destroyDataStructures)",
                );
            }
        }),
    );
}

/// The most the game's close waits for the network thread's shutdown; with
/// Steam's last move ([`steam::glue::GOODBYE_WAIT`]) the hook stays near 100 ms.
const EXIT_WAIT: Duration = Duration::from_millis(98);

/// The game is closing: a goodbye to every partner and the network thread's
/// shutdown, waited for at most [`EXIT_WAIT`]. Over Steam, one last move of
/// the queued datagrams (`SteamCoop::flush_on_exit`).
fn game_closing(call: &Call<'_>, s: &Shared) {
    let started = Instant::now();
    let (peers, shutdown) = {
        let c = s.lock();
        let peers: Vec<PeerId> = c.core.partners().keys().copied().collect();
        for p in &peers {
            c.net.disconnect(*p);
        }
        (peers, c.net.begin_shutdown())
    };
    let stage = shutdown.wait_until(started + EXIT_WAIT);
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let told = peers.len();
    let head = format!("co-op: game closing; {told} partner(s) told");
    match stage {
        ShutdownStage::Done => log::info!("{head}, network stopped in {ms:.1} ms"),
        ShutdownStage::Sent => {
            log::info!("{head}, goodbyes sent in {ms:.1} ms, the last status still being written");
        }
        ShutdownStage::NoAnswer => {
            log::warn!("{head}, no word from the network thread in {ms:.1} ms, closing anyway");
        }
    }
    if peers.is_empty() {
        return;
    }
    if let Some(steam) = &s.steam {
        let moved = steam.flush_on_exit(call);
        log::info!("co-op steam: {moved} datagrams moved on the way out");
    }
}

/// Before the end screen's setup: in a co-op session
/// ([`scaling::upload_blocked`]), turn the leaderboard switch off for the
/// event. Returns the value to restore. (The upload itself is an extension
/// function and cannot be hooked.)
fn block_leaderboards(call: &mut Call<'_>, s: &Shared) -> Option<Value> {
    if !s.flags.uploads_blocked() {
        return None;
    }
    let (g, var) = (call.global()?, s.h.enable_leaderboards?);
    let old = g.get_value(var)?;
    if old.as_f64().is_none() {
        warn_once(
            "leaderboards",
            &format!("co-op: enableLeaderboards is {old}, not a number; left alone"),
        );
        return None;
    }
    match g.set(var, 0.0) {
        Ok(()) => {
            let n = s.flags.count_blocked_upload();
            log::info!("co-op run: leaderboard upload skipped ({n} this session)");
            Some(old)
        }
        Err(e) => {
            warn_once(
                "leaderboards",
                &format!("co-op: unable to block leaderboard uploads ({e})"),
            );
            None
        }
    }
}

fn restore_leaderboards(call: &mut Call<'_>, s: &Shared, saved: Option<Value>) {
    let (Some(old), Some(g), Some(var)) = (saved, call.global(), s.h.enable_leaderboards) else {
        return;
    };
    match g.set(var, old.clone()) {
        Ok(()) => log::info!("co-op run: enableLeaderboards restored to {old}"),
        Err(e) => warn_once(
            "leaderboards-restore",
            &format!("co-op: unable to restore leaderboard uploads ({e})"),
        ),
    }
}

fn enemy_init(call: &mut Call<'_>, s: &Shared) {
    let Some(m) = s.flags.health_mult() else {
        return;
    };
    let Some(me) = call.instance() else { return };
    for v in [s.h.hull, s.h.hull_max, s.h.hull_previous]
        .into_iter()
        .flatten()
    {
        let old = me.get_f64(v);
        if let Some(new) = old.and_then(|h| scaled(h, m)) {
            if me.set_f64(v, new) {
                info_once(
                    "enemy-scaled",
                    &format!(
                        "co-op: first scaled enemy hull {} -> {new} (x{m})",
                        old.unwrap_or(0.0)
                    ),
                );
            } else {
                warn_once(
                    "enemy-hull",
                    "co-op: an enemy hull variable is not a number; not scaled",
                );
            }
        }
    }
}

/// How long after a link wait ends the return's state line is written. The
/// partner's reliable `Downed`, `GameOver` and `RunLeft` have arrived by then.
const RETURN_LINE_AFTER: Duration = Duration::from_secs(2);

/// A game's state after a link wait ended: its partner's as learned, or
/// its own ship (`own_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartnerAtReturn {
    /// In the run, its ship up.
    Up,
    /// In the run, its ship down.
    Down,
    /// The run is over (a game over, told or seen here).
    RunOver,
    /// It left the run for the menu (`RunLeft`).
    LeftRun,
    /// Not linked any more.
    Gone,
}

/// This game's state at a link wait's end, in words: frozen or not, and
/// its ship or the run's end (`at`: `Up`, `Down` or `RunOver`).
fn own_state(host: bool, frozen: bool, at: PartnerAtReturn) -> String {
    let mut s = String::from(match (frozen, host) {
        (true, _) => "frozen in the link wait",
        (false, true) => "playing on",
        (false, false) => "not frozen",
    });
    s.push_str(match at {
        PartnerAtReturn::RunOver => ", the run over",
        PartnerAtReturn::Down => ", in the run, its ship down",
        _ => ", in the run, its ship up",
    });
    s
}

/// The return's state line: no names or ids.
fn return_text(own: &str, partner: PartnerAtReturn) -> String {
    let p = match partner {
        PartnerAtReturn::Up => "in the run, its ship up",
        PartnerAtReturn::Down => "in the run, its ship down",
        PartnerAtReturn::RunOver => "the run over",
        PartnerAtReturn::LeftRun => "left the run for the menu",
        PartnerAtReturn::Gone => "not linked",
    };
    format!("co-op link: at the return this game was {own}; the partner since: {p}")
}

ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml"));

#[cfg(test)]
mod tests {
    #[test]
    fn the_returns_state_line_says_both_states() {
        use super::{PartnerAtReturn as P, own_state, return_text};
        assert_eq!(
            return_text(&own_state(false, true, P::Up), P::Up),
            "co-op link: at the return this game was frozen in the link wait, in the run, its ship up; the partner since: in the run, its ship up"
        );
        assert_eq!(
            return_text(&own_state(true, false, P::Down), P::LeftRun),
            "co-op link: at the return this game was playing on, in the run, its ship down; the partner since: left the run for the menu"
        );
        assert!(
            return_text(&own_state(true, false, P::RunOver), P::Down).contains(
                "was playing on, the run over; the partner since: in the run, its ship down"
            )
        );
        assert!(
            return_text(&own_state(false, false, P::Up), P::Gone)
                .ends_with("not frozen, in the run, its ship up; the partner since: not linked")
        );
        assert!(return_text("x", P::RunOver).ends_with("the partner since: the run over"));
    }

    #[test]
    fn the_run_end_hides_co_op_drawing() {
        use super::run_end_reason as r;
        assert_eq!(r(false, false, false), None);
        assert_eq!(r(true, true, false), Some("the ending"));
        assert_eq!(r(false, true, false), Some("the game hides its HUD"));
        assert_eq!(r(true, true, true), Some("game over"));
        assert_eq!(r(false, false, true), Some("game over"));
    }

    #[test]
    fn shipped_defaults_parse() {
        let _: super::Config =
            ndml_sdk::config::parse_defaults(super::NDML_DEFAULT_CONFIG).unwrap();
    }
}

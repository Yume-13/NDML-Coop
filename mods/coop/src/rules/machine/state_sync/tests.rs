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

//! The rules converge from state with each rules message dropped, duplicated,
//! delayed or reordered, and with random drops; a clean lane adopts nothing.

use std::time::{Duration, Instant};

use ndml_net::msg::{Message, PeerId, RulesState};
use ndml_net::rng::Rng;
use ndml_net::session::PeerSet;

use super::super::{
    Action, Death, FRAME, Observed, PlayerAct, PowersAct, Rules, RunCtx, UnpauseCall,
};
use super::{GRACE, PRESS_GRACE, STATE_EVERY, counted};
use crate::net::Out;
use crate::rules::settings::RulesSettings;

mod counts;
mod table;

const HOST: PeerId = PeerId::HOST;
const CLIENT: PeerId = PeerId(1);

/// What the lane does with one rules message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Pass,
    Drop,
    Dup,
    /// Delivered this many frames later, everything after it waiting (a stall).
    Late(u64),
    /// Delivered half a second later, later messages passing it.
    Swap,
}

/// A held message: when, to which game, whether later ones wait for it.
type Held = (u64, usize, bool, Message);

/// One game as its machine sees it.
#[allow(clippy::struct_excessive_bools)] // the game's flags, one each
struct Game {
    r: Rules,
    paused: bool,
    /// The frame its upgrade screen opened.
    screen: Option<u64>,
    chose: bool,
    menu_since: Option<u64>,
    over: bool,
    /// Frames the player stays on a pause menu (default 120).
    menu_for: u64,
    /// Frames the player takes to choose (default 60).
    choose_after: u64,
    /// Not in play: it ignores the machine's `GotoUpgrade`.
    blind: bool,
    /// Its partner's statements never reach it.
    deaf: bool,
}

type FateFn = Box<dyn FnMut(usize, usize, &Message) -> Fate>;

struct Net {
    g: [Game; 2],
    t: Instant,
    f: u64,
    /// Rules messages each way so far: 0 to the client, 1 to the host.
    n: [usize; 2],
    fate: FateFn,
    held: Vec<Held>,
    /// Frames per part of the session.
    phase: u64,
}

fn cfg() -> RulesSettings {
    RulesSettings {
        respawn: Duration::from_secs(10),
        ..RulesSettings::default()
    }
}

fn ctx(is_host: bool) -> RunCtx {
    RunCtx {
        key: (7, Some(1.0)),
        coop_run: true,
        linked: true,
        gate_running: true,
        me: if is_host { HOST } else { CLIENT },
        is_host,
        partners: [if is_host { CLIENT } else { HOST }].into_iter().collect(),
    }
}

impl Net {
    fn new(fate: FateFn) -> Self {
        let t = Instant::now();
        let game = |is_host: bool| {
            let mut r = Rules::new(cfg());
            r.sync_run(&ctx(is_host));
            Game {
                r,
                paused: false,
                screen: None,
                chose: false,
                menu_since: None,
                over: false,
                menu_for: 120,
                choose_after: 60,
                blind: false,
                deaf: false,
            }
        };
        Self {
            g: [game(true), game(false)],
            t,
            f: 0,
            n: [0, 0],
            fate,
            held: Vec::new(),
            phase: 600,
        }
    }

    fn clean() -> Self {
        Self::new(Box::new(|_, _, _| Fate::Pass))
    }

    /// Message `m` to game `to`, through the lane's fate for rules messages.
    fn route(&mut self, to: usize, m: Message) {
        if !counted(&m) {
            if !(self.g[to].deaf && matches!(m, Message::RulesState(_))) {
                self.hand(to, &m);
            }
            return;
        }
        let dir = 1 - to;
        let k = self.n[dir];
        self.n[dir] += 1;
        let fate = (self.fate)(dir, k, &m);
        // Behind a stalled message, everything that way waits with it.
        if let Some(at) = self
            .held
            .iter()
            .filter(|h| h.1 == to && h.2)
            .map(|h| h.0)
            .max()
            && fate != Fate::Drop
        {
            self.held.push((at, to, true, m));
            return;
        }
        match fate {
            Fate::Pass => self.hand(to, &m),
            Fate::Drop => {}
            Fate::Dup => {
                self.hand(to, &m);
                self.hand(to, &m);
            }
            Fate::Late(d) => self.held.push((self.f + d, to, true, m)),
            Fate::Swap => self.held.push((self.f + 30, to, false, m)),
        }
    }

    fn hand(&mut self, to: usize, m: &Message) {
        let from = if to == 0 { CLIENT } else { HOST };
        let t = self.t;
        self.g[to].r.on_message(from, m, t);
    }

    fn deliver(&mut self) {
        let f = self.f;
        let due: Vec<Held> = self.held.extract_if(.., |h| h.0 <= f).collect();
        for (_, to, _, m) in due {
            self.hand(to, &m);
        }
        let mut settled = false;
        for _ in 0..10 {
            let h = self.g[0].r.take_outs();
            let c = self.g[1].r.take_outs();
            if h.is_empty() && c.is_empty() {
                settled = true;
                break;
            }
            for (to, outs) in [(1, h), (0, c)] {
                for o in outs {
                    if let Out::Broadcast(m) | Out::Send(_, m) = o {
                        self.route(to, m);
                    }
                }
            }
        }
        assert!(settled, "messages never settled");
    }

    /// Each player leaves its pause menu after `menu_for`, chooses after `choose_after`.
    fn players(&mut self) {
        let (f, t) = (self.f, self.t);
        for g in &mut self.g {
            if g.over {
                continue;
            }
            if g.paused && g.r.holding_pause() {
                let since = *g.menu_since.get_or_insert(f);
                if f - since >= g.menu_for && g.r.unpause_request(false, t) == UnpauseCall::Allow {
                    g.paused = false;
                }
            } else {
                g.menu_since = None;
            }
            if let Some(at) = g.screen
                && !g.chose
                && f - at >= g.choose_after
            {
                g.chose = true;
                if !g.r.upgrade_leaving(t) {
                    g.screen = None;
                }
            }
        }
    }

    fn frame(&mut self) {
        self.players();
        let (f, t) = (self.f, self.t);
        for g in &mut self.g {
            let o = Observed {
                paused: g.paused,
                gameplay: !g.blind && !g.paused && g.screen.is_none() && !g.over,
                game_over: g.over,
                levelup: Some(1.0),
                partner: Some((500.0, 300.0, 0.0, 0.0)),
                ..Observed::default()
            };
            for a in g.r.frame(&o, t) {
                match a {
                    Action::PressPause => g.paused = true,
                    Action::PressUnpause => g.paused = false,
                    Action::GotoUpgrade if g.blind => {}
                    Action::GotoUpgrade => {
                        g.r.upgrade_opened();
                        g.screen = Some(f);
                        g.chose = false;
                    }
                    Action::ReleaseUpgrade => {
                        assert!(!g.r.upgrade_leaving(t));
                        g.screen = None;
                    }
                    Action::SetXp(_) | Action::SetScore(_) => {}
                }
            }
            match g.r.powers_action() {
                PowersAct::Revive => g.r.powers_applied(),
                PowersAct::ForceDeath => {
                    g.r.powers_applied();
                    if g.r.death(t) == Death::Die {
                        g.over = true;
                    }
                }
                PowersAct::None | PowersAct::HoldDown => {}
            }
            if matches!(g.r.player_action(), PlayerAct::Revive(_)) {
                g.r.player_applied();
            }
        }
        self.t += FRAME;
        self.f += 1;
        self.deliver();
    }

    fn until(&mut self, f: u64) {
        while self.f < f {
            self.frame();
        }
    }

    fn host(&self) -> &Rules {
        &self.g[0].r
    }

    fn client(&self) -> &Rules {
        &self.g[1].r
    }

    fn adopted(&self) -> u32 {
        self.host().adopted() + self.client().adopted()
    }

    /// Game `i`'s statement to the other as it would send it now.
    fn stated(&mut self, i: usize) -> RulesState {
        let to = if i == 0 { CLIENT } else { HOST };
        let r = &mut self.g[i].r;
        let mut s = r.statement();
        r.sync.link(to).stamp(&mut s);
        s
    }

    /// Pauses, an upgrade round, a down and the game over; `Err` names what did not converge.
    fn session(&mut self) -> Result<(), String> {
        let p = self.phase;
        let check = |n: &Self, what: &str, ok: bool| {
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "{what} at frame {}: host pause {:?} round {:?} down {:?}, client pause {:?} round {:?} down {:?}, games paused {} {}",
                    n.f,
                    n.host().pause.holders(),
                    n.host().upgrade.active(),
                    n.host().respawn.downed_peers(),
                    n.client().pause.holders(),
                    n.client().upgrade.active(),
                    n.client().respawn.downed_peers(),
                    n.g[0].paused,
                    n.g[1].paused
                ))
            }
        };
        let quiet = |n: &Self| {
            n.g.iter().all(|g| !g.paused && g.screen.is_none())
                && !n.host().pause.is_paused()
                && !n.client().pause.is_paused()
        };
        self.until(30);
        self.g[0].paused = true;
        self.until(p);
        check(self, "the host's pause", quiet(self))?;
        self.g[1].paused = true;
        self.until(2 * p);
        check(self, "the client's pause", quiet(self))?;
        self.g[0].r.upgrade_opened();
        self.g[0].screen = Some(self.f);
        self.g[0].chose = false;
        self.until(3 * p);
        let rounds =
            |n: &Self| n.host().upgrade.active().is_none() && n.client().upgrade.active().is_none();
        check(self, "the upgrade round", quiet(self) && rounds(self))?;
        let t = self.t;
        assert_eq!(self.g[1].r.death(t), Death::Down);
        self.until(5 * p);
        let up = |n: &Self| {
            !n.client().is_down()
                && n.host().respawn.downed_peers().is_empty()
                && n.client().respawn.downed_peers().is_empty()
        };
        check(self, "the client's down", up(self) && quiet(self))?;
        let t = self.t;
        assert_eq!(self.g[1].r.death(t), Death::Down);
        self.until(5 * p + 60);
        let t = self.t;
        if self.g[0].r.death(t) == Death::Die {
            self.g[0].over = true;
        }
        self.until(6 * p);
        let over = |n: &Self| n.g[0].over && n.g[1].over;
        check(self, "the game over", over(self))
    }
}

#[test]
fn a_clean_session_adopts_nothing() {
    let mut n = Net::clean();
    n.session().unwrap();
    assert_eq!(n.adopted(), 0);
    assert!(n.n[0] > 10 && n.n[1] > 5, "{:?}", n.n);
    // The count line: the count first, then each link, with nothing left
    // to drop.
    let line = n.client().sync.count_line();
    assert!(
        line.starts_with("co-op rules: adopted from state: 0; P1: skip 0, link 1/1, sent "),
        "{line}"
    );
    let line = n.host().sync.count_line();
    assert!(
        line.starts_with("co-op rules: adopted from state: 0; P2: skip 0, link 1/1, sent "),
        "{line}"
    );
}

/// Every rules message of the clean session, by direction and order.
fn clean_messages() -> [usize; 2] {
    let mut n = Net::clean();
    n.session().unwrap();
    n.n
}

fn run_with(fate: FateFn) -> (Result<(), String>, u32) {
    let mut n = Net::new(fate);
    let r = n.session();
    (r, n.adopted())
}

/// One message at a time, each way, dropped, duplicated, delayed or reordered.
#[test]
fn each_message_dropped_duplicated_delayed_or_reordered_converges() {
    let total = clean_messages();
    let late = (GRACE.as_secs() * 60) + 60;
    let mut fails = Vec::new();
    for dir in 0..2 {
        for k in 0..total[dir] {
            for fate in [
                Fate::Drop,
                Fate::Dup,
                Fate::Late(60),
                Fate::Late(late),
                Fate::Swap,
            ] {
                let (r, _) = run_with(Box::new(
                    move |d, i, _| if d == dir && i == k { fate } else { Fate::Pass },
                ));
                if let Err(e) = r {
                    fails.push(format!(
                        "{} message {k} {fate:?}: {e}",
                        ["to the client", "to the host"][dir]
                    ));
                }
            }
        }
    }
    assert!(
        fails.is_empty(),
        "{} of them:\n{}",
        fails.len(),
        fails.join("\n")
    );
}

/// A delay inside the grace adopts nothing.
#[test]
fn a_message_late_inside_the_grace_adopts_nothing() {
    let total = clean_messages();
    for (dir, &count) in total.iter().enumerate() {
        for k in 0..count {
            let (r, adopted) = run_with(Box::new(move |d, i, _| {
                if d == dir && i == k {
                    Fate::Late(60)
                } else {
                    Fate::Pass
                }
            }));
            r.unwrap();
            assert_eq!(adopted, 0, "message {k} way {dir}");
        }
    }
}

/// Random loss of rules messages both ways (the lane's statements arrive).
#[test]
fn random_drops_converge() {
    let mut fails = Vec::new();
    for seed in 1..=40 {
        let mut rng = Rng::new(seed);
        let mut n = Net::new(Box::new(move |_, _, _| {
            if rng.next_f64() < 0.3 {
                Fate::Drop
            } else {
                Fate::Pass
            }
        }));
        // Room for several recoveries in each part.
        n.phase = 1500;
        if let Err(e) = n.session() {
            fails.push(format!("seed {seed}: {e}"));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// Every host message to the client lost from the round's open on. The joining game leaves its
/// held screen within the grace and one statement of the release, and the host's later pause and
/// resume reach it.
#[test]
fn a_lane_that_loses_everything_from_the_round_on_converges() {
    let mut cut = false;
    let mut n = Net::new(Box::new(move |d, _, m| {
        cut |= d == 0 && matches!(m, Message::UpgradeOpen { .. });
        if d == 0 && cut {
            Fate::Drop
        } else {
            Fate::Pass
        }
    }));
    n.until(30);
    n.g[1].r.upgrade_opened();
    n.g[1].screen = Some(n.f);
    n.g[1].chose = false;
    n.until(30 + 60 + 1);
    assert!(n.g[1].screen.is_some(), "held for the host");
    let chose = n.f;
    n.until(chose + 600);
    assert!(
        n.g.iter().all(|g| g.screen.is_none()),
        "both screens closed"
    );
    assert!(n.client().upgrade.active().is_none());
    assert!(n.client().adopted() >= 1);
    assert_eq!(
        n.client().upgrade.last_released(),
        n.host().upgrade.last_released()
    );
    // Then the host pauses (its menu open 2 s): its round waits for the
    // client, which never heard of it, and ends from the client's statement.
    n.g[0].paused = true;
    let paused_at = n.f;
    n.until(paused_at + 120 + GRACE_FRAMES + 2 * STATE_EVERY + 2);
    assert!(n.g.iter().all(|g| !g.paused), "the host's round ended");
    assert_eq!(n.host().pause.holders(), PeerSet::EMPTY);
    assert_eq!(n.client().pause.holders(), PeerSet::EMPTY);
    assert!(n.host().adopted() >= 1);
}

const GRACE_FRAMES: u64 = GRACE.as_secs() * 60;

/// A statement behind what this machine applied never overrides it.
#[test]
fn an_old_statement_never_overrides_a_newer_event() {
    let mut n = Net::clean();
    n.until(30);
    // The host's statement from before its pause (nobody paused).
    let old = n.stated(0);
    assert_eq!((old.seq, old.holders), (0, 0));
    // The host pauses and its relay reaches the client: both in the round.
    n.g[0].paused = true;
    n.until(40);
    assert!(n.client().pause.is_paused());
    // The old statement again and again past the grace: the client stays
    // in the round.
    for _ in 0..(4 * GRACE.as_secs()) {
        n.t += Duration::from_secs(1);
        let t = n.t;
        n.g[1]
            .r
            .on_message(HOST, &Message::RulesState(old.clone()), t);
    }
    assert!(n.client().pause.is_paused());
    assert_eq!(n.client().adopted(), 0);
}

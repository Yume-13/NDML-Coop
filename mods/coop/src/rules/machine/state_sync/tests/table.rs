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

//! Each adoption rule on a lane stalled 16 s, the cases where nothing may be
//! adopted, and the stalled lane's edges.

use super::*;

/// The slow lane: 16 s.
const STALL: u64 = 960;
/// A statement that differs is adopted within this.
const ADOPT: u64 = GRACE_FRAMES + 2 * STATE_EVERY + 2;

/// The lane to game `to` stalls from the first message `hit` matches.
fn stall(to: usize, hit: fn(&Message) -> bool) -> FateFn {
    let dir = 1 - to;
    let mut done = false;
    Box::new(move |d, _, m| {
        if d == dir && !done && hit(m) {
            done = true;
            Fate::Late(STALL)
        } else {
            Fate::Pass
        }
    })
}

fn set(p: PeerId) -> PeerSet {
    [p].into_iter().collect()
}

impl Net {
    /// Messages to game `to` still on their way.
    fn flying(&self, to: usize) -> usize {
        self.held.iter().filter(|h| h.1 == to).count()
    }

    /// Game `i` opens its upgrade screen.
    fn open(&mut self, i: usize) {
        self.g[i].r.upgrade_opened();
        self.g[i].screen = Some(self.f);
        self.g[i].chose = false;
    }

    fn down(&mut self, i: usize) -> Death {
        let t = self.t;
        let d = self.g[i].r.death(t);
        if d == Death::Die {
            self.g[i].over = true;
        }
        d
    }

    /// Statement `s` to game `to`, now.
    fn tell(&mut self, to: usize, s: RulesState) {
        let from = if to == 0 { CLIENT } else { HOST };
        let t = self.t;
        self.g[to].r.on_message(from, &Message::RulesState(s), t);
    }

    /// After the stall: every late message dropped, nothing more adopted.
    fn after_stall(&mut self, to: usize) {
        let adopted = self.adopted();
        let until = self.f + STALL + 5;
        self.until(until);
        assert_eq!(self.flying(to), 0);
        let from = if to == 0 { CLIENT } else { HOST };
        assert_eq!(
            self.g[to].r.sync.link(from).skip,
            0,
            "every covered message dropped"
        );
        assert_eq!(self.adopted(), adopted, "nothing more adopted");
    }
}

// ---- the joining game's rules --------------------------------------------

#[test]
fn client_takes_the_game_over() {
    let mut n = Net::new(stall(1, |m| {
        matches!(m, Message::GameOver | Message::Downed { peer: HOST, .. })
    }));
    n.until(30);
    assert_eq!(n.down(1), Death::Down);
    n.until(60);
    assert_eq!(n.down(0), Death::Die);
    n.until(60 + ADOPT);
    assert!(n.flying(1) > 0);
    assert!(n.client().game_over_seen);
    assert_eq!(n.client().adopted(), 1);
    n.after_stall(1);
}

#[test]
fn client_takes_a_release() {
    let mut n = Net::new(stall(1, |m| {
        matches!(m, Message::UpgradeDone { peer: HOST, .. })
    }));
    n.until(30);
    n.open(0);
    n.until(100);
    assert!(n.g[1].chose, "the client chose");
    // The client's choice is a press: the release waits for its grace.
    n.until(100 + PRESS_GRACE + ADOPT);
    assert!(n.flying(1) > 0);
    assert!(n.g[1].screen.is_none() && n.client().upgrade.active().is_none());
    assert_eq!(n.client().upgrade.last_released(), Some(1));
    assert_eq!(n.client().adopted(), 1);
    n.after_stall(1);
}

#[test]
fn client_takes_an_open_round_and_its_done_set() {
    let mut n = Net::new(stall(1, |m| matches!(m, Message::UpgradeOpen { .. })));
    n.until(30);
    n.open(0);
    n.until(30 + ADOPT);
    assert!(
        n.g[1].screen.is_some(),
        "the client's screen opened from the host's state"
    );
    // The host's choice came with the round.
    assert_eq!(n.client().upgrade.waiting_on(), set(CLIENT));
    assert_eq!(n.client().adopted(), 1);
    // The client chooses and the host releases; the relay of the choice is
    // stalled too, so the release follows from state after its press.
    // Nobody waited `UPGRADE_COUNT_AFTER`: the host's screen goes at the
    // release, with no count; the client's is held for the round's end,
    // told by state.
    n.until(30 + ADOPT + 70 + 20);
    assert!(n.g[0].screen.is_none() && n.g[1].screen.is_some());
    assert_eq!(n.host().upgrade.last_released(), Some(1));
    n.until(30 + ADOPT + 70 + COUNT_FRAMES + PRESS_GRACE + ADOPT);
    assert!(n.flying(1) > 0);
    assert_eq!(n.client().upgrade.last_released(), Some(1));
    assert_eq!(n.client().adopted(), 2);
    n.until(30 + ADOPT + 70 + 2 * COUNT_FRAMES + PRESS_GRACE + ADOPT);
    assert!(n.g[1].screen.is_none(), "the client's after its count");
    n.after_stall(1);
}

#[test]
fn client_takes_who_chose_in_the_open_round() {
    let mut n = Net::new(stall(1, |m| {
        matches!(m, Message::UpgradeDone { peer: HOST, .. })
    }));
    // The client's game cannot show the screen yet: the round is open
    // there, its screen idle (nothing of its own in flight).
    n.g[1].blind = true;
    n.until(30);
    n.open(0);
    n.until(91);
    assert!(n.g[0].chose);
    assert!(n.client().upgrade.waiting_on().contains(HOST));
    n.until(91 + ADOPT);
    assert!(n.flying(1) > 0);
    assert_eq!(n.client().upgrade.active(), Some(1));
    assert_eq!(n.client().upgrade.waiting_on(), set(CLIENT));
    assert_eq!(n.client().adopted(), 1);
    n.after_stall(1);
}

#[test]
fn client_takes_the_holders_but_never_drops_its_own_hold() {
    let mut n = Net::new(stall(1, |m| matches!(m, Message::Unpause { by: HOST })));
    n.g[1].menu_for = 100_000;
    n.until(30);
    n.g[0].paused = true;
    n.until(40);
    assert_eq!(n.client().pause.holders(), PeerSet::first(2));
    n.until(170);
    // The host resumed (its world stays frozen while the client holds).
    assert!(!n.host().pause.holds(HOST));
    n.until(170 + PRESS_GRACE + ADOPT);
    assert!(n.flying(1) > 0);
    assert_eq!(n.client().pause.holders(), set(CLIENT), "its own hold kept");
    assert!(n.g[1].paused, "still on its menu");
    assert_eq!(n.client().adopted(), 1);
    n.after_stall(1);
    // The client's resume then ends the round on both.
    n.g[1].menu_for = 0;
    let f = n.f + 30 + COUNT_FRAMES;
    n.until(f);
    assert!(n.g.iter().all(|g| !g.paused));
    assert!(!n.host().pause.is_paused() && !n.client().pause.is_paused());
}

#[test]
fn client_takes_the_refusal_of_its_pause() {
    let mut n = Net::new(stall(1, |m| matches!(m, Message::UpgradeOpen { .. })));
    n.g[1].menu_for = 100_000;
    n.until(30);
    n.open(0);
    n.until(32);
    n.g[1].paused = true;
    n.until(40);
    assert!(n.client().pause_request.is_some(), "its request on its way");
    n.until(40 + PRESS_GRACE + ADOPT);
    assert!(n.flying(1) > 0);
    assert!(
        n.client().pause_request.is_none(),
        "turned down from the host's state"
    );
    assert!(!n.client().pause.is_paused());
    assert!(n.client().adopted() >= 1);
}

#[test]
fn client_takes_a_down() {
    let mut n = Net::new(stall(1, |m| {
        matches!(m, Message::Downed { peer: HOST, .. })
    }));
    n.until(30);
    assert_eq!(n.down(0), Death::Down);
    n.until(30 + ADOPT);
    assert!(n.flying(1) > 0);
    assert!(n.client().respawn.is_down(HOST));
    assert_eq!(n.client().adopted(), 1);
}

#[test]
fn client_takes_a_respawn() {
    let mut n = Net::new(stall(1, |m| matches!(m, Message::Respawned { peer: HOST })));
    n.until(30);
    assert_eq!(n.down(0), Death::Down);
    n.until(40);
    assert!(n.client().respawn.is_down(HOST));
    while n.host().respawn.is_down(HOST) {
        n.frame();
        assert!(n.f < 3000, "the host never came back");
    }
    let back = n.f;
    n.until(back + ADOPT);
    assert!(n.flying(1) > 0);
    assert!(!n.client().respawn.is_down(HOST));
    assert_eq!(n.client().adopted(), 1);
    n.after_stall(1);
}

// ---- the host's rules ----------------------------------------------------

#[test]
fn host_takes_the_clients_hold() {
    let mut n = Net::new(stall(0, |m| matches!(m, Message::Pause { by: CLIENT })));
    n.g[1].menu_for = 100_000;
    n.until(30);
    n.g[1].paused = true;
    n.until(30 + ADOPT);
    assert!(n.flying(0) > 0);
    assert!(n.host().pause.holds(CLIENT));
    assert!(n.g[0].paused, "the host's world paused with the client");
    assert_eq!(n.host().adopted(), 1);
    n.after_stall(0);
}

/// The host's `Pause` never reaches the client: the host takes it out of the
/// round from its state and stays paused until it resumes itself.
#[test]
fn host_takes_the_clients_resume_and_stays_on_its_own_menu() {
    let mut n = Net::new(stall(1, |m| matches!(m, Message::Pause { by: HOST })));
    n.g[0].menu_for = 1000;
    n.until(30);
    n.g[0].paused = true;
    n.until(40);
    assert_eq!(n.host().pause.holders(), PeerSet::first(2));
    n.until(30 + ADOPT);
    assert!(n.flying(1) > 0);
    assert_eq!(n.host().adopted(), 1);
    assert_eq!(
        n.host().pause.holders(),
        set(HOST),
        "the client is out of the round"
    );
    assert!(
        n.g[0].paused && n.host().pause.is_paused(),
        "the host still on its menu"
    );
    // The client takes the host's pause from its state meanwhile (its world
    // freezes with the host's).
    assert!(n.client().pause.holds(HOST) && !n.client().pause.holds(CLIENT));
    n.until(1000);
    assert!(
        n.g[0].paused && n.host().pause.is_paused(),
        "still, until the host resumes"
    );
    n.until(1030 + ADOPT);
    assert!(n.g.iter().all(|g| !g.paused));
    assert!(!n.host().pause.is_paused() && !n.client().pause.is_paused());
}

#[test]
fn host_takes_the_clients_open() {
    let mut n = Net::new(stall(0, |m| matches!(m, Message::UpgradeOpen { .. })));
    n.g[1].choose_after = 100_000;
    n.until(30);
    n.open(1);
    n.until(30 + ADOPT);
    assert!(n.flying(0) > 0);
    assert_eq!(n.host().upgrade.active(), Some(1));
    assert_eq!(n.host().adopted(), 1);
    assert_eq!(n.client().adopted(), 0, "the client is choosing");
}

/// The host has the client down while it flies: it takes "up" from state,
/// with no second revive.
#[test]
fn host_takes_the_clients_ship_up_and_it_is_not_revived_again() {
    let mut n = Net::clean();
    n.until(30);
    assert_eq!(n.down(1), Death::Down);
    n.until(40);
    assert!(n.host().respawn.is_down(CLIENT));
    n.g[1].r.down = super::super::super::Ship::Up;
    n.until(40 + ADOPT);
    assert!(!n.host().respawn.is_down(CLIENT));
    assert_eq!(n.host().adopted(), 1);
    assert_eq!(
        n.client().powers_action(),
        PowersAct::None,
        "no second revive"
    );
    assert!(!n.client().is_down());
    assert_eq!(n.down(0), Death::Down, "not a game over");
}

#[test]
fn host_takes_the_clients_choice() {
    let mut n = Net::new(stall(0, |m| {
        matches!(m, Message::UpgradeDone { peer: CLIENT, .. })
    }));
    n.until(30);
    n.open(0);
    n.until(100);
    assert!(n.g[1].chose);
    n.until(100 + ADOPT + COUNT_FRAMES);
    assert!(n.flying(0) > 0);
    assert_eq!(n.host().upgrade.last_released(), Some(1));
    assert!(n.g.iter().all(|g| g.screen.is_none()));
    assert_eq!(n.host().adopted(), 1);
    n.after_stall(0);
}

// ---- never under the joining player's hands -------------------------------

/// The host's statement, stating the host down.
fn host_down_stated(n: &mut Net, over: bool) -> RulesState {
    let mut s = n.stated(0);
    s.downs = vec![(HOST, 5000)];
    if over {
        s.flags |= RulesState::GAME_OVER;
    }
    s
}

#[test]
fn nothing_adopted_while_the_clients_press_is_on_its_way() {
    let mut n = Net::clean();
    n.g[1].deaf = true;
    n.g[1].menu_for = 100_000;
    let press = 100;
    for f in 30..(press + PRESS_GRACE + ADOPT) {
        if f == press {
            n.g[1].paused = true;
        }
        if f % STATE_EVERY == 0 {
            let s = host_down_stated(&mut n, false);
            n.tell(1, s);
        }
        n.frame();
        if f <= press + PRESS_GRACE {
            // The grace ran out at 30 + GRACE_FRAMES; the press holds it.
            assert_eq!(n.client().adopted(), 0, "frame {f}");
        }
    }
    assert_eq!(n.client().adopted(), 1, "then taken");
    assert!(n.client().respawn.is_down(HOST));
}

/// `Screen::Choosing` blocks every adoption, the game over included, until
/// the client chooses.
#[test]
fn nothing_adopted_while_the_client_chooses() {
    let mut n = Net::clean();
    n.g[1].deaf = true;
    n.g[1].choose_after = 100_000;
    n.until(30);
    n.open(0);
    for f in 31..(31 + 3 * ADOPT) {
        if f % STATE_EVERY == 0 {
            let s = host_down_stated(&mut n, true);
            n.tell(1, s);
        }
        n.frame();
    }
    assert!(n.client().upgrade_choosing());
    assert_eq!(n.client().adopted(), 0);
    assert!(!n.client().game_over_seen);
    // The client chooses: the round ends, and the game over is taken.
    n.g[1].choose_after = 0;
    let f0 = n.f;
    for f in f0..(f0 + ADOPT + 10) {
        if f % STATE_EVERY == 0 {
            let s = host_down_stated(&mut n, true);
            n.tell(1, s);
        }
        n.frame();
    }
    assert!(n.client().game_over_seen);
    assert_eq!(n.client().adopted(), 1);
}

/// Never adopted on the first statement after a start over.
#[test]
fn nothing_adopted_on_the_first_statement_after_a_start_over() {
    let mut n = Net::clean();
    n.g[1].deaf = true;
    n.until(30);
    for _ in 0..(GRACE_FRAMES / STATE_EVERY - 1) {
        let s = host_down_stated(&mut n, false);
        n.tell(1, s);
        let f = n.f + STATE_EVERY;
        n.until(f);
    }
    assert_eq!(n.client().adopted(), 0);
    n.g[1].r.sync_link_lost(HOST);
    // The client's next statement reaches the host: the host's next counts
    // on the client's new link.
    let f = n.f + STATE_EVERY + 1;
    n.until(f);
    let s = host_down_stated(&mut n, false);
    assert_eq!(s.echo, n.g[1].r.sync.link(HOST).id);
    n.t += GRACE * 2;
    n.tell(1, s);
    assert_eq!(n.client().adopted(), 0, "the first: never");
    n.t += GRACE;
    let s = host_down_stated(&mut n, false);
    n.tell(1, s);
    assert_eq!(n.client().adopted(), 1, "one a grace later: taken");
}

// ---- the slow lane's edges -----------------------------------------------

/// A release adopted from state, then a new pause behind the late messages
/// arrives by message: nothing more adopted, nothing dropped.
#[test]
fn a_late_release_then_a_new_event_behind_it() {
    let mut n = Net::new(stall(1, |m| {
        matches!(m, Message::UpgradeDone { peer: HOST, .. })
    }));
    n.until(30);
    n.open(0);
    n.until(100 + PRESS_GRACE + ADOPT);
    assert_eq!(n.client().upgrade.last_released(), Some(1));
    assert_eq!(n.client().adopted(), 1);
    let ends = n.held.iter().map(|h| h.0).max().unwrap_or(0);
    // The host pauses a second before the late messages arrive.
    n.until(ends - 60);
    n.g[0].paused = true;
    n.until(ends - 50);
    assert!(n.flying(1) > 0);
    assert!(
        !n.client().pause.is_paused(),
        "its pause waits behind the stall"
    );
    n.until(ends + 1);
    assert_eq!(n.flying(1), 0);
    assert!(
        n.client().pause.holds(HOST),
        "the host's pause arrived and was applied"
    );
    assert_eq!(n.g[1].r.sync.link(HOST).skip, 0);
    assert_eq!(n.client().adopted(), 1);
}

/// A covered message in the adoption's frame, either order.
#[test]
fn a_covered_message_in_the_adoptions_frame() {
    for statement_first in [true, false] {
        let mut n = Net::new(stall(1, |m| {
            matches!(m, Message::Downed { peer: HOST, .. })
        }));
        n.g[1].deaf = true;
        n.until(30);
        assert_eq!(n.down(0), Death::Down);
        n.until(31);
        let s = n.stated(0);
        n.tell(1, s);
        n.until(31 + GRACE_FRAMES + 5);
        let s = n.stated(0);
        let late: Vec<Held> = n.held.extract_if(.., |h| h.1 == 1).collect();
        assert_eq!(late.len(), 1);
        if statement_first {
            n.tell(1, s);
            n.hand(1, &late[0].3);
            assert_eq!(n.client().adopted(), 1);
        } else {
            n.hand(1, &late[0].3);
            n.tell(1, s);
            assert_eq!(n.client().adopted(), 0);
        }
        assert!(n.client().respawn.is_down(HOST));
        assert_eq!(n.g[1].r.sync.link(HOST).skip, 0);
        while n.host().respawn.is_down(HOST) {
            n.frame();
            assert!(n.f < 3000);
        }
        n.frame();
        assert!(
            !n.client().respawn.is_down(HOST),
            "the respawn after it applied"
        );
    }
}

/// A lost link between the adoption and the late messages drops nothing new.
#[test]
fn a_lost_link_between_the_adoption_and_the_late_messages() {
    let mut n = Net::new(stall(1, |m| {
        matches!(m, Message::Downed { peer: HOST, .. })
    }));
    n.until(30);
    assert_eq!(n.down(0), Death::Down);
    n.until(30 + ADOPT);
    assert!(n.client().respawn.is_down(HOST));
    assert_eq!(n.client().adopted(), 1);
    assert_eq!(n.g[1].r.sync.link(HOST).skip, 1, "the late Downed covered");
    // Lost: what was in flight goes with the connection; each side sees it.
    n.held.clear();
    let t = n.t;
    n.g[0].r.peer_left(CLIENT, true, t);
    n.until(n.f + 20);
    let t = n.t;
    n.g[1].r.peer_left(HOST, true, t);
    assert_eq!(n.g[1].r.sync.link(HOST).skip, 0, "skip starts over");
    n.until(n.f + 60);
    let t = n.t;
    n.g[0].r.peer_joined(CLIENT, t);
    n.g[1].r.peer_joined(HOST, t);
    // The host's own down, told again on the new link, and its respawn:
    // applied by message.
    n.until(n.f + 10);
    assert!(n.client().respawn.is_down(HOST));
    while n.host().respawn.is_down(HOST) {
        n.frame();
        assert!(n.f < 3000);
    }
    n.frame();
    assert!(!n.client().respawn.is_down(HOST));
    assert_eq!(n.adopted(), 1, "nothing more adopted");
}

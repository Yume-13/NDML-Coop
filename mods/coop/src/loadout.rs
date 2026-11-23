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

//! The host's run settings apply to both games of a linked run; the joining
//! player sees them read-only.
//!
//! * The 12 `gameMod*` flags and the daily picks (`dailyChoice`, `dailyModChoice`,
//!   `dailyModChoice2`). Locked modes are played anyway; nothing is unlocked.
//! * The game builds the run from the flags at Start (`confirmLoadout`) and in the run's
//!   first Create events, so the host sends its picks ([`Message::RunRules`]) whenever
//!   they change and when it starts.
//! * The joining game holds the host's picks with the rows locked; its Start means
//!   "ready"; Resume Run is not offered. Its own picks ([`Joiner::held`]) are never saved
//!   and come back at the first menu frame, or at once on the loadout screen when the
//!   link goes; the resume file is not written meanwhile.
//! * Fails closed: on only if every hook in [`NEEDED_HOOKS`] registered and every
//!   variable and script was found ([`rule_off_because`]).
//! * From a Start to the menu nothing is held or given back.
//!
//! [`Message::RunRules`]: ndml_net::Message::RunRules

#![forbid(unsafe_code)]

use ndml_net::msg::DailyPick;

use crate::names;

/// The `gameMod*` flags carried: the first 12 of `names::GLOBAL_MODE`, one
/// bit each in that order.
pub const MODE_FLAGS: usize = 12;
/// `gameModDailyChallenge`'s bit.
pub const DAILY_BIT: usize = 1;

/// A run's settings: the modes and, with the daily on, the daily's picks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub mods: u16,
    pub daily: Option<DailyPick>,
}

/// The 12 flags as bits (`None` = missing, off).
#[must_use]
pub fn mode_bits(flags: &[Option<f64>]) -> u16 {
    let mut mods = 0u16;
    for (i, f) in flags.iter().take(MODE_FLAGS).enumerate() {
        if f.is_some_and(|v| v > 0.5) {
            mods |= 1 << i;
        }
    }
    mods
}

impl Rules {
    /// From the flags (`names::GLOBAL_MODE` order) and the loadout's daily
    /// picks (kept only with the daily on).
    #[must_use]
    pub fn read(flags: &[Option<f64>], daily: Option<DailyPick>) -> Self {
        let mods = mode_bits(flags);
        let daily = daily.filter(|_| mods & (1 << DAILY_BIT) != 0);
        Self { mods, daily }
    }

    /// Whether mode `i` is on.
    #[must_use]
    pub fn on(&self, i: usize) -> bool {
        i < MODE_FLAGS && self.mods & (1 << i) != 0
    }

    /// In player words: the modes' titles, the daily as
    /// `"<Daily Challenge>: <name>"`; "no game modes" when nothing is on.
    #[must_use]
    pub fn describe(&self, title: &dyn Fn(usize) -> String) -> String {
        let mut parts = Vec::new();
        for i in 0..MODE_FLAGS {
            if !self.on(i) {
                continue;
            }
            let t = title(i);
            match (&self.daily, i == DAILY_BIT) {
                (Some(d), true) if !d.name.trim().is_empty() => {
                    parts.push(format!("{t}: {}", d.name.trim()));
                }
                _ => parts.push(t),
            }
        }
        if parts.is_empty() {
            "no game modes".into()
        } else {
            parts.join(", ")
        }
    }
}

/// Cut a daily's name to [`DailyPick::NAME_MAX`] bytes at a character
/// boundary.
#[must_use]
pub fn clip_name(name: &str) -> String {
    let mut end = name.len().min(DailyPick::NAME_MAX);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// This player's own picks, kept while the host's are held: the 12 flags and
/// the loadout screen's daily picks (`None` off the loadout screen).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Own {
    pub mods: u16,
    pub loadout_daily: Option<DailyPick>,
}

/// What a loadout frame of the joining game does ([`Joiner::loadout_frame`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// Put these back first (the joining player's own picks).
    pub restore: Option<Own>,
    /// Hold these in the flags and the loadout's daily picks (the host's).
    pub show: Option<Rules>,
    /// The bar line.
    pub note: Note,
}

/// The loadout screen's bar line for the joining player.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Note {
    #[default]
    None,
    /// The host's picks ([`bar_line`]).
    Picks(Rules),
    /// The host has started and its start is kept here, Start not pressed.
    HostStarted,
    /// Start pressed, waiting for the host.
    Ready,
    /// Resume Run pressed: not while playing together.
    NoResume,
}

/// The line for a host that has started and waits at its start gate.
#[must_use]
pub fn host_waiting(partner: &str) -> String {
    format!("{partner} is Ready.")
}

/// The Co-op page's line for a linked joining game: the host's run in words,
/// or that it has started and waits.
#[must_use]
pub fn page_line(
    host: Option<&Rules>,
    host_started: bool,
    partner: &str,
    describe: &dyn Fn(&Rules) -> String,
) -> Option<String> {
    let r = host?;
    Some(if host_started {
        host_waiting(partner)
    } else {
        format!("{partner}'s run: {}.", describe(r))
    })
}

/// The longest picks line (characters) the loadout bar shows in full; past it
/// the bar would overlap the screen's Score Bonus line, and the line gives the
/// count of modes instead.
pub const PICKS_LINE_MAX: usize = 100;

/// The joining player's bar line for `note`. Until the host is on its loadout
/// screen (`host_on_loadout`) the picks line says the host is still choosing.
/// `start_word` is what the Start row reads.
#[must_use]
pub fn bar_line(
    note: &Note,
    host_on_loadout: bool,
    partner: &str,
    start_word: &str,
    describe: &dyn Fn(&Rules) -> String,
) -> Option<String> {
    match note {
        Note::None => None,
        Note::Picks(_) if !host_on_loadout => Some(format!("{partner} is configuring lobby...")),
        Note::Picks(r) => {
            let full = format!(
                "{partner} picks the run: {}. Press {start_word} to begin.",
                describe(r)
            );
            if full.chars().count() <= PICKS_LINE_MAX {
                return Some(full);
            }
            let n = (0..MODE_FLAGS).filter(|&i| r.on(i)).count();
            let modes = if n == 1 { "game mode" } else { "game modes" };
            Some(format!(
                "{partner} picks the run: {n} {modes}. Press {start_word} to begin."
            ))
        }
        Note::HostStarted => Some(format!(
            "{} Press {start_word} to join.",
            host_waiting(partner)
        )),
        Note::Ready => Some(format!(
            "Ready. The run starts when {partner} starts it. Press Esc to cancel."
        )),
        Note::NoResume => Some(format!(
            "Resume Run is not available while you play with {partner}."
        )),
    }
}

/// What `confirmLoadout` does ([`Joiner::confirm`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// The game's own (not a linked joining game, nothing held).
    Original,
    /// A mode row on a linked joining game: skipped.
    Locked,
    /// Resume Run on a linked game (joining or host): skipped.
    ResumeBlocked,
    /// Start before the host started: skipped; the start waits ("ready").
    Wait,
    /// Start with the host's run: these are written, then the game's own
    /// start runs with its settings writes skipped.
    Apply(Rules),
    /// A press during the fade after an applied start: skipped (the game's
    /// own `confirmLoadout` does nothing then either).
    Starting,
}

/// The game functions the rule needs hooked; without any one of them the
/// host's settings are never held ([`missing_hooks`]).
pub const NEEDED_HOOKS: [&str; 10] = [
    names::CONFIRM_LOADOUT,
    names::INI_WRITE_BOOL,
    names::RESET_PROGRESS,
    names::RESET_DAILY,
    names::RESUME_RUN,
    names::LOADOUT_STEP,
    names::MENU_STEP,
    names::CONTROLLER_STEP,
    names::TRIGGER_SAVE_RUN,
    names::DELETE_RUN_FILES,
];

/// The needed hooks among `failed` (every hook the mod could not register).
#[must_use]
pub fn missing_hooks<'a>(needed: &[&'a str], failed: &[String]) -> Vec<&'a str> {
    needed
        .iter()
        .copied()
        .filter(|n| failed.iter().any(|f| f == n))
        .collect()
}

/// Why the rule cannot be on in this game: hooks that did not register and
/// game names (`handles`: name, found) that were not found. `None`: all there.
#[must_use]
pub fn rule_off_because(missing_hooks: &[&str], handles: &[(&str, bool)]) -> Option<String> {
    let not_found: Vec<&str> = handles
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| *n)
        .collect();
    let mut why = Vec::new();
    if !missing_hooks.is_empty() {
        why.push(format!("not hooked: {}", missing_hooks.join(", ")));
    }
    if !not_found.is_empty() {
        why.push(format!("not found: {}", not_found.join(", ")));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// The joining game's side of the rule, and the host's last sent picks.
/// Pure; the game side is `loadout_glue`.
#[allow(clippy::struct_excessive_bools)] // independent states of the screen, each read alone
#[derive(Debug, Clone, Default)]
pub struct Joiner {
    /// Every hook the rule needs is live ([`Self::enable`]); nothing is held
    /// while off.
    enabled: bool,
    /// An applied start is under way: the host's start stays held until the
    /// menu, whatever the link does.
    starting: bool,
    /// The host's current picks (any `RunRules`).
    host: Option<Rules>,
    /// The host's last `RunRules` came from its loadout screen.
    host_on_loadout: bool,
    /// The host started a run with these, not taken yet.
    host_start: Option<Rules>,
    /// Start pressed, waiting for the host's start.
    ready: bool,
    /// The last bar line was about Resume Run (until the next press).
    no_resume: bool,
    /// The joining player's own picks while the host's may be in the flags,
    /// kept until written back ([`Self::restored`]).
    own: Option<Own>,
    /// What is held in the flags now.
    held: Option<Rules>,
    /// The host's start is being applied: `resetDailyChallenges` gets these
    /// back, `ini_write_bool` is skipped.
    applying: Option<Rules>,
}

impl Joiner {
    /// A joiner whose hooks are all live (tests).
    #[must_use]
    pub fn on() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Every hook the rule needs registered: the rule may hold the host's
    /// settings from now on.
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// The rule is on in this game.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The host's `RunRules` (ignored while the rule is off).
    pub fn on_rules(&mut self, start: bool, rules: Rules) {
        if !self.enabled {
            return;
        }
        self.host_start = start.then(|| rules.clone());
        self.host = Some(rules);
    }

    /// Whether the host is on its loadout screen; only the bar line reads it.
    pub fn host_screen(&mut self, on_loadout: bool) {
        if self.enabled {
            self.host_on_loadout = on_loadout;
        }
    }

    /// The host's last `RunRules` came from its loadout screen.
    #[must_use]
    pub fn host_on_loadout(&self) -> bool {
        self.host_on_loadout
    }

    /// The host (or the link) is gone: what it sent is forgotten. The joining
    /// player's own picks stay kept until the next restore point.
    pub fn host_gone(&mut self) {
        self.host = None;
        self.host_on_loadout = false;
        self.host_start = None;
        self.ready = false;
        self.no_resume = false;
    }

    /// The host has sent its picks in this link.
    #[must_use]
    pub fn shares(&self) -> bool {
        self.host.is_some()
    }

    /// The host's settings may be in the flags (the player's own are kept
    /// here). The resume file is not written while this holds.
    #[must_use]
    pub fn held(&self) -> bool {
        self.own.is_some()
    }

    /// What the flags hold now, if the host's.
    #[must_use]
    pub fn held_rules(&self) -> Option<&Rules> {
        self.held.as_ref()
    }

    /// The host's current picks (for the Co-op page).
    #[must_use]
    pub fn host_picks(&self) -> Option<&Rules> {
        self.host.as_ref()
    }

    /// The host has started a run that waits for the joining player.
    #[must_use]
    pub fn host_started(&self) -> bool {
        self.host_start.is_some()
    }

    /// Start pressed and waiting.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// The rules being applied by its own start call.
    #[must_use]
    pub fn applying(&self) -> Option<&Rules> {
        self.applying.as_ref()
    }

    /// `confirmLoadout`'s settings writes are skipped now.
    #[must_use]
    pub fn skip_writes(&self) -> bool {
        self.applying.is_some()
    }

    /// The game's own `confirmLoadout` may run now: nothing of the host's is
    /// held and no locked screen is up (the fallback when the hook fails).
    #[must_use]
    pub fn original_safe(&self, linked: bool) -> bool {
        !(self.held() || (self.enabled && linked && self.shares()))
    }

    /// The joining player's loadout screen shows the host's picks, read only.
    #[must_use]
    pub fn locked(&self, linked_joiner: bool) -> bool {
        self.enabled && linked_joiner && self.shares() && !self.starting
    }

    fn hold(&mut self, own_now: impl FnOnce() -> Own, rules: &Rules) {
        if self.own.is_none() {
            self.own = Some(own_now());
        }
        self.held = Some(rules.clone());
    }

    /// The player's own picks to write back (kept until [`Self::restored`]).
    pub fn release(&mut self) -> Option<Own> {
        self.ready = false;
        self.no_resume = false;
        self.starting = false;
        self.own.clone()
    }

    /// The player's own picks were written back: nothing is held now.
    pub fn restored(&mut self) {
        self.own = None;
        self.held = None;
    }

    /// Any game frame: clears a write skip left by a `confirmLoadout` that
    /// never returned. `true` = one was cleared.
    pub fn frame_tick(&mut self) -> bool {
        self.applying.take().is_some()
    }

    /// A frame of the loadout screen. `linked_joiner`: linked, and not the
    /// host. `own_now` reads the player's picks (called only when taken).
    pub fn loadout_frame(&mut self, linked_joiner: bool, own_now: impl FnOnce() -> Own) -> Frame {
        if self.starting {
            // The fade after an applied start: nothing changes until the menu.
            return Frame::default();
        }
        if !(linked_joiner && self.enabled) {
            self.ready = false;
            self.no_resume = false;
            return Frame {
                restore: self.release(),
                ..Frame::default()
            };
        }
        let Some(host) = self.host.clone() else {
            // The host's picks not here yet: the game's own screen.
            return Frame::default();
        };
        self.hold(own_now, &host);
        let note = if self.no_resume {
            Note::NoResume
        } else if self.ready {
            Note::Ready
        } else if self.host_start.is_some() {
            Note::HostStarted
        } else {
            Note::Picks(host.clone())
        };
        Frame {
            restore: None,
            show: Some(host),
            note,
        }
    }

    /// A main menu frame: the player's own picks come back. The menu writes
    /// no flag, so holding again on the loadout screen is safe.
    pub fn menu_frame(&mut self) -> Option<Own> {
        self.release()
    }

    /// `confirmLoadout` on a linked host's loadout screen: a Start that
    /// resumes a saved run is refused; every other press is the game's own.
    pub fn host_confirm(&mut self, pos0: bool, resumed: bool) -> Confirm {
        self.no_resume = self.enabled && pos0 && resumed;
        if self.no_resume {
            Confirm::ResumeBlocked
        } else {
            Confirm::Original
        }
    }

    /// A linked host's bar line ([`Note::NoResume`] after a refused Resume
    /// Run, until its next press or the menu).
    #[must_use]
    pub fn host_note(&self) -> Note {
        if self.enabled && self.no_resume {
            Note::NoResume
        } else {
            Note::None
        }
    }

    /// `confirmLoadout` was called. `pos0`: the Start row; `resumed`: the
    /// Start resumes a saved run. The glue writes `restore` first and skips
    /// the press if that fails.
    pub fn confirm(
        &mut self,
        linked_joiner: bool,
        pos0: bool,
        resumed: bool,
        own_now: impl FnOnce() -> Own,
    ) -> (Option<Own>, Confirm) {
        if self.starting {
            return (None, Confirm::Starting);
        }
        if !(linked_joiner && self.enabled && self.shares()) {
            // The game's own press; anything still held goes back first.
            let restore = self.release();
            return (restore, Confirm::Original);
        }
        self.no_resume = false;
        if !pos0 {
            return (None, Confirm::Locked);
        }
        if resumed {
            self.no_resume = true;
            return (None, Confirm::ResumeBlocked);
        }
        let Some(r) = self.host_start.take() else {
            self.ready = true;
            return (None, Confirm::Wait);
        };
        self.hold(own_now, &r);
        self.ready = false;
        self.starting = true;
        self.applying = Some(r.clone());
        (None, Confirm::Apply(r))
    }

    /// The game's own Start began its fade: until the menu nothing is held or
    /// given back. Only with the rule on, so a solo player's presses are never
    /// latched.
    pub fn started_own(&mut self) {
        if self.enabled {
            self.starting = true;
        }
    }

    /// The game did not take an applied start (its fade did not begin): the
    /// host's start is kept again for the next Start and the latch let go.
    pub fn start_not_taken(&mut self) {
        if let Some(r) = self.applying.take() {
            if self.host_start.is_none() {
                self.host_start = Some(r);
            }
            self.starting = false;
        }
    }

    /// `confirmLoadout` returned.
    pub fn confirm_done(&mut self) {
        self.applying = None;
    }

    /// The host has started and a start is waiting: make it now (through
    /// `confirmLoadout`, which applies it).
    #[must_use]
    pub fn start_due(&self) -> bool {
        self.ready && self.host_start.is_some() && !self.starting
    }

    /// Back from the loadout screen: nothing waits.
    pub fn cancel(&mut self) {
        self.ready = false;
        self.no_resume = false;
    }

    /// `resetProgress` ran and left `mods` in the flags: while held, those
    /// become the player's own picks, and what was held is to be written back.
    pub fn reset_progress(&mut self, mods: u16) -> Option<Rules> {
        let own = self.own.as_mut()?;
        own.mods = mods;
        self.held.clone()
    }
}

/// The host's side: what it last sent, so it sends on change. Nothing between
/// its start and its next menu frame: a `start` false then would take back a
/// start the joining game has not used yet.
#[derive(Debug, Clone, Default)]
pub struct HostSent {
    /// The picks and whether they came from the loadout screen.
    last: Option<(Rules, bool)>,
    started: bool,
}

impl HostSent {
    /// Its picks this frame (`on_loadout`: the loadout screen, else the
    /// menu): `Some` = send them. A move between the two sends too.
    pub fn picks(&mut self, now: Rules, on_loadout: bool) -> Option<Rules> {
        let now = (now, on_loadout);
        if self.started || self.last.as_ref() == Some(&now) {
            return None;
        }
        self.last = Some(now.clone());
        Some(now.0)
    }

    /// Its run's start went out (`start` true).
    pub fn started(&mut self) {
        self.started = true;
    }

    /// A main menu frame after a run: its picks go out again (telling the
    /// joining game the start is spent).
    pub fn menu(&mut self) {
        if std::mem::take(&mut self.started) {
            self.last = None;
        }
    }

    /// Send again at the next frame (a new link).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picks a test says must not be read: a read shows up as these.
    fn sentinel() -> Own {
        Own {
            mods: 0xF000,
            loadout_daily: None,
        }
    }

    fn pick(choice: i16) -> DailyPick {
        DailyPick {
            choice,
            modifier: 2,
            modifier2: 5,
            name: format!("Daily {choice}"),
        }
    }

    fn flags(on: &[usize]) -> Vec<Option<f64>> {
        (0..15)
            .map(|i| Some(if on.contains(&i) { 1.0 } else { 0.0 }))
            .collect()
    }

    fn rules(mods: u16) -> Rules {
        Rules { mods, daily: None }
    }

    #[test]
    fn rules_read_the_flags_and_keep_the_daily_only_with_it_on() {
        let r = Rules::read(&flags(&[4, 9]), Some(pick(3)));
        assert_eq!(r.mods, (1 << 4) | (1 << 9));
        assert_eq!(
            r.daily, None,
            "no daily flag: the loadout's picks are not this run's"
        );
        let r = Rules::read(&flags(&[DAILY_BIT]), Some(pick(3)));
        assert_eq!(r.daily, Some(pick(3)));
        // The base difficulty factors after the 12 flags are not modes.
        let r = Rules::read(&flags(&[12, 13, 14]), None);
        assert_eq!(r.mods, 0);
        assert_eq!(mode_bits(&[None, Some(1.0), Some(0.2)]), 0b10);
    }

    #[test]
    fn described_in_player_words() {
        let title = |i: usize| format!("Mode{i}");
        assert_eq!(Rules::default().describe(&title), "no game modes");
        let r = Rules::read(&flags(&[DAILY_BIT, 4]), Some(pick(3)));
        assert_eq!(r.describe(&title), "Mode1: Daily 3, Mode4");
    }

    #[test]
    fn a_name_is_cut_at_a_character_boundary() {
        let long = "é".repeat(40);
        let c = clip_name(&long);
        assert!(c.len() <= DailyPick::NAME_MAX && long.starts_with(&c));
    }

    #[test]
    fn the_host_sends_on_change_and_again_after_a_reset() {
        let mut h = HostSent::default();
        assert_eq!(h.picks(rules(1), false), Some(rules(1)));
        assert_eq!(h.picks(rules(1), false), None);
        assert_eq!(h.picks(rules(1), true), Some(rules(1)), "on its loadout");
        assert_eq!(h.picks(rules(1), true), None);
        assert_eq!(h.picks(rules(3), true), Some(rules(3)));
        h.reset();
        assert_eq!(h.picks(rules(3), true), Some(rules(3)));
        // From its start to its next menu frame: nothing.
        h.started();
        h.reset();
        assert_eq!(h.picks(rules(3), true), None);
        assert_eq!(h.picks(rules(5), true), None);
        h.menu();
        assert_eq!(h.picks(rules(5), false), Some(rules(5)), "the run is over");
        h.menu();
        assert_eq!(h.picks(rules(5), false), None, "once");
        assert_eq!(
            h.picks(rules(5), true),
            Some(rules(5)),
            "back on its loadout"
        );
    }

    #[test]
    fn a_long_picks_line_gives_the_count() {
        let name = "Sixteen Chars Nm"; // the longest name drawn (`NAME_MAX`)
        assert_eq!(name.chars().count(), crate::ui::text::NAME_MAX);
        let title = |i: usize| {
            ["DANGER ZONE", "MAYHEM", "ELITE ENEMY", "HOSTILE UNIVERSE"][i % 4].to_owned()
        };
        let describe = |r: &Rules| r.describe(&title);
        let bar = |mods: u16| {
            bar_line(&Note::Picks(rules(mods)), true, name, "Ready", &describe).unwrap()
        };
        // Two modes: the list, within the limit.
        let two = bar(0b11);
        assert!(two.chars().count() <= PICKS_LINE_MAX);
        assert_eq!(
            two,
            format!("{name} picks the run: DANGER ZONE, MAYHEM. Press Ready to begin.")
        );
        // Four: past it, the count.
        let full = format!(
            "{name} picks the run: {}. Press Ready to begin.",
            describe(&rules(0b1111))
        );
        assert!(full.chars().count() > PICKS_LINE_MAX);
        assert_eq!(
            bar(0b1111),
            format!("{name} picks the run: 4 game modes. Press Ready to begin.")
        );
        // The Co-op page keeps the list.
        assert_eq!(
            page_line(Some(&rules(0b1111)), false, name, &describe),
            Some(format!("{name}'s run: {}.", describe(&rules(0b1111))))
        );
    }

    #[test]
    fn the_joining_bar_says_the_host_is_choosing_until_its_loadout_screen_speaks() {
        let describe = |r: &Rules| r.describe(&|i| format!("Mode{i}"));
        let bar = |j: &Joiner, note: &Note| {
            bar_line(note, j.host_on_loadout(), "Sam", "Ready", &describe)
        };
        let mut j = Joiner::on();
        // The host still in its menu.
        j.host_screen(false);
        j.on_rules(false, rules(0b10));
        assert_eq!(
            bar(&j, &Note::Picks(rules(0b10))).as_deref(),
            Some("Sam is configuring lobby...")
        );
        // On its loadout screen, nothing picked.
        j.host_screen(true);
        j.on_rules(false, rules(0));
        assert_eq!(
            bar(&j, &Note::Picks(rules(0))).as_deref(),
            Some("Sam picks the run: no game modes. Press Ready to begin.")
        );
        // On it, with modes.
        j.on_rules(false, rules(0b110));
        assert_eq!(
            bar(&j, &Note::Picks(rules(0b110))).as_deref(),
            Some("Sam picks the run: Mode1, Mode2. Press Ready to begin.")
        );
        // Only the sentence: the lock and the after-press line do not read it.
        j.host_screen(false);
        assert!(j.locked(true));
        assert_eq!(
            bar(&j, &Note::Ready).as_deref(),
            Some("Ready. The run starts when Sam starts it. Press Esc to cancel.")
        );
        assert_eq!(bar(&j, &Note::None), None);
        // The host started before the joining player pressed: its start is
        // kept, the bar and the page say it waits.
        let page = |j: &Joiner| page_line(j.host_picks(), j.host_started(), "Sam", &describe);
        j.host_screen(true);
        assert_eq!(page(&j).as_deref(), Some("Sam's run: Mode1, Mode2."));
        j.on_rules(true, rules(0b110));
        let f = j.loadout_frame(true, Own::default);
        assert_eq!(
            (f.note.clone(), f.show),
            (Note::HostStarted, Some(rules(0b110)))
        );
        assert_eq!(
            bar(&j, &f.note).as_deref(),
            Some("Sam is Ready. Press Ready to join.")
        );
        assert_eq!(page(&j).as_deref(), Some("Sam is Ready."));
        let _ = j.menu_frame();
        j.restored();
        assert!(j.host_started(), "kept through the joining menu");
        // The host left its gate: its picks again without a start.
        j.on_rules(false, rules(0b110));
        assert_eq!(
            j.loadout_frame(true, Own::default).note,
            Note::Picks(rules(0b110))
        );
        assert_eq!(page(&j).as_deref(), Some("Sam's run: Mode1, Mode2."));
        // Started again, and the press takes it.
        j.on_rules(true, rules(0b110));
        let (_, c) = j.confirm(true, true, false, Own::default);
        assert_eq!(c, Confirm::Apply(rules(0b110)));
        assert!(!j.host_started());
        assert_eq!(
            bar(&j, &Note::HostStarted)
                .as_deref()
                .map(|s| s.contains("to join")),
            Some(true)
        );
        assert_eq!(
            page_line(None, true, "Sam", &describe),
            None,
            "no host here"
        );
        let _ = j.menu_frame();
        j.restored();
        j.host_screen(true);
        j.host_gone();
        assert!(!j.host_on_loadout(), "forgotten with the host");
        let mut off = Joiner::default();
        off.host_screen(true);
        assert!(!off.host_on_loadout(), "the rule off");
    }

    #[test]
    fn a_linked_loadout_holds_the_hosts_picks_and_locks_the_rows() {
        let mut j = Joiner::on();
        let mine = Own {
            mods: 0b1,
            loadout_daily: Some(pick(1)),
        };
        // Linked, nothing from the host yet: the game's own screen.
        let f = j.loadout_frame(true, || mine.clone());
        assert_eq!(f, Frame::default());
        assert!(!j.held());
        assert_eq!(
            j.confirm(true, false, false, || mine.clone()).1,
            Confirm::Original
        );
        j.on_rules(false, rules(0b100));
        let f = j.loadout_frame(true, || mine.clone());
        assert_eq!(f.show, Some(rules(0b100)));
        assert_eq!(f.note, Note::Picks(rules(0b100)));
        assert!(j.held());
        // The own picks are read once.
        let f = j.loadout_frame(true, sentinel);
        assert_eq!(f.show, Some(rules(0b100)));
        assert_eq!(j.confirm(true, false, false, sentinel).1, Confirm::Locked);
        // Start before the host: ready, waiting.
        assert_eq!(j.confirm(true, true, false, sentinel).1, Confirm::Wait);
        assert_eq!(j.loadout_frame(true, sentinel).note, Note::Ready);
        assert!(!j.start_due());
        j.on_rules(true, rules(0b110));
        assert!(j.start_due());
        let (restore, c) = j.confirm(true, true, false, sentinel);
        assert_eq!((restore, c), (None, Confirm::Apply(rules(0b110))));
        assert!(j.skip_writes());
        assert_eq!(j.applying(), Some(&rules(0b110)));
        j.confirm_done();
        assert!(!j.skip_writes() && !j.start_due() && !j.ready());
        assert_eq!(j.held_rules(), Some(&rules(0b110)));
        // The run's end: the own picks back, offered again until written.
        assert_eq!(j.menu_frame(), Some(mine.clone()));
        assert!(j.held(), "kept until the write is known to have happened");
        assert_eq!(j.menu_frame(), Some(mine), "the write failed: again");
        j.restored();
        assert!(!j.held());
        assert_eq!(j.menu_frame(), None, "once written");
    }

    #[test]
    fn the_start_row_reads_ready_only_while_the_joining_screen_is_the_hosts() {
        let mut j = Joiner::on();
        assert!(!j.locked(true), "nothing from the host yet");
        j.on_rules(false, rules(0b1));
        assert!(j.locked(true));
        assert!(!j.locked(false), "unlinked, or the host");
        j.on_rules(true, rules(0b1));
        let _ = j.confirm(true, true, false, Own::default);
        assert!(!j.locked(true), "a start under way");
        let _ = j.menu_frame();
        j.restored();
        assert!(j.locked(true), "the next visit");
        j.host_gone();
        assert!(!j.locked(true), "the host gone");
        let mut off = Joiner::default();
        off.on_rules(false, rules(0b1));
        assert!(!off.locked(true), "the rule off");
    }

    #[test]
    fn a_joiner_without_every_hook_never_holds() {
        let mut j = Joiner::default();
        assert!(!j.enabled(), "off until the glue enables it");
        j.on_rules(false, rules(0b100));
        j.on_rules(true, rules(0b100));
        assert!(!j.shares());
        assert_eq!(j.loadout_frame(true, sentinel), Frame::default());
        assert_eq!(j.confirm(true, true, false, sentinel).1, Confirm::Original);
        assert_eq!(j.confirm(true, false, false, sentinel).1, Confirm::Original);
        assert!(!j.held() && j.original_safe(true));
        let failed = vec!["gml_Script_ini_write_bool".to_owned(), "other".to_owned()];
        assert_eq!(
            missing_hooks(&NEEDED_HOOKS, &failed),
            vec!["gml_Script_ini_write_bool"]
        );
        assert_eq!(
            missing_hooks(&NEEDED_HOOKS, &["other".to_owned()]),
            Vec::<&str>::new()
        );
    }

    /// An applied Start the game did not take (its fade back to the menu was
    /// running, or the call failed): the host's start stays to be taken.
    #[test]
    fn an_applied_start_the_game_did_not_take_is_not_lost() {
        let mut j = Joiner::on();
        let r = rules(0b100);
        j.on_rules(true, r.clone());
        assert_eq!(j.loadout_frame(true, Own::default).note, Note::HostStarted);
        let (_, d) = j.confirm(true, true, false, Own::default);
        assert_eq!(d, Confirm::Apply(r.clone()));
        assert!(!j.host_started() && j.skip_writes());
        // The after-hook: the fade did not begin.
        j.start_not_taken();
        j.confirm_done();
        assert!(j.host_started() && !j.skip_writes() && !j.ready());
        assert!(
            j.locked(true),
            "no start under way: the joining screen is the host's again"
        );
        // alarm[1]: the menu; the own picks back.
        assert!(j.menu_frame().is_some());
        j.restored();
        let f = j.loadout_frame(true, Own::default);
        assert_eq!(
            f.note,
            Note::HostStarted,
            "the host still waits at its gate"
        );
        let (_, d) = j.confirm(true, true, false, Own::default);
        assert_eq!(d, Confirm::Apply(r), "the Start still joins the host's run");
        // Taken this time: nothing to put back (and a second call is harmless).
        j.confirm_done();
        j.start_not_taken();
        assert!(!j.host_started());
    }

    /// A missing game variable or script turns the rule off like a missing
    /// hook, with one line naming everything missing.
    #[test]
    fn a_missing_handle_turns_the_rule_off_like_a_missing_hook() {
        let all = [
            ("oLoadout.pos", true),
            ("oLoadout.fade", true),
            ("gml_Script_confirmLoadout (script)", true),
            ("gameModMayhem", true),
        ];
        assert_eq!(rule_off_because(&[], &all), None, "everything there: on");
        let mut no_pos = all;
        no_pos[0].1 = false;
        no_pos[3].1 = false;
        assert_eq!(
            rule_off_because(&[], &no_pos).as_deref(),
            Some("not found: oLoadout.pos, gameModMayhem")
        );
        assert_eq!(
            rule_off_because(&[names::MENU_STEP], &no_pos).as_deref(),
            Some(
                format!(
                    "not hooked: {}; not found: oLoadout.pos, gameModMayhem",
                    names::MENU_STEP
                )
                .as_str()
            )
        );
        assert_eq!(
            rule_off_because(&[names::MENU_STEP], &all).as_deref(),
            Some(format!("not hooked: {}", names::MENU_STEP).as_str())
        );
    }

    /// The rule off and no menu frame ever coming: a solo player's later runs
    /// still start.
    #[test]
    fn rule_off_and_no_menu_frame_a_second_run_can_still_be_started() {
        let mut j = Joiner::default();
        assert_eq!(
            j.confirm(false, true, false, sentinel),
            (None, Confirm::Original)
        );
        j.started_own(); // the fade began
        let f = j.loadout_frame(false, sentinel);
        assert_eq!(f.restore, None);
        for _ in 0..3 {
            let (_, row) = j.confirm(false, false, false, sentinel);
            let (_, start) = j.confirm(false, true, false, sentinel);
            assert_eq!((row, start), (Confirm::Original, Confirm::Original));
            j.started_own();
        }
        // On, the latch holds until the menu.
        let mut on = Joiner::on();
        let _ = on.confirm(false, true, false, sentinel);
        on.started_own();
        assert_eq!(
            on.confirm(false, true, false, sentinel).1,
            Confirm::Starting
        );
        let _ = on.menu_frame();
        assert_eq!(
            on.confirm(false, true, false, sentinel).1,
            Confirm::Original
        );
    }

    #[test]
    fn the_original_press_is_unsafe_while_anything_is_held_or_locked() {
        let mut j = Joiner::on();
        assert!(j.original_safe(true), "linked, nothing from the host yet");
        j.on_rules(false, rules(0b1));
        assert!(!j.original_safe(true), "the locked screen");
        assert!(j.original_safe(false));
        let _ = j.loadout_frame(true, Own::default);
        assert!(!j.original_safe(false), "unlinked with the host's held");
        // A role change to host: the own picks are written back first.
        assert_eq!(j.release(), Some(Own::default()));
        j.restored();
        assert!(j.original_safe(false));
    }

    #[test]
    fn a_stuck_write_skip_is_cleared_at_the_next_frame() {
        let mut j = Joiner::on();
        j.on_rules(true, rules(0b11));
        let _ = j.confirm(true, true, false, Own::default);
        assert!(j.skip_writes());
        // `confirmLoadout` never returned (a GML error): no after-hook.
        assert!(j.frame_tick());
        assert!(!j.skip_writes());
        assert!(!j.frame_tick(), "once");
    }

    #[test]
    fn a_link_lost_during_the_fade_keeps_the_hosts_start_until_the_menu() {
        let mut j = Joiner::on();
        let mine = Own {
            mods: 0b1000,
            loadout_daily: Some(pick(4)),
        };
        j.on_rules(true, rules(0b11));
        let _ = j.loadout_frame(true, || mine.clone());
        assert_eq!(
            j.confirm(true, true, false, sentinel).1,
            Confirm::Apply(rules(0b11))
        );
        j.confirm_done();
        j.host_gone();
        // The loadout screen still steps for 10 to 20 frames: the own picks
        // stay out of the run being made, and a press is skipped.
        assert_eq!(j.loadout_frame(false, sentinel), Frame::default());
        assert_eq!(j.confirm(false, true, false, sentinel).1, Confirm::Starting);
        assert_eq!(j.held_rules(), Some(&rules(0b11)));
        assert!(j.held(), "the resume file stays off");
        assert_eq!(j.menu_frame(), Some(mine));
    }

    /// No Resume Run while playing together, for the host too; unlinked, or
    /// with the rule off, the game's own.
    #[test]
    fn resume_run_is_refused_for_a_linked_host_and_joiner_and_taken_unlinked() {
        let describe = |r: &Rules| r.describe(&|i| format!("Mode{i}"));
        let line = "Resume Run is not available while you play with Sam.";
        // The linked host (the glue routes its presses to `host_confirm`).
        let mut host = Joiner::on();
        assert_eq!(host.host_confirm(true, true), Confirm::ResumeBlocked);
        assert_eq!(host.host_note(), Note::NoResume);
        assert_eq!(
            bar_line(&host.host_note(), true, "Sam", "Start", &describe).as_deref(),
            Some(line)
        );
        assert_eq!(
            host.host_confirm(false, true),
            Confirm::Original,
            "a mode row"
        );
        assert_eq!(host.host_note(), Note::None, "until the next press");
        assert_eq!(
            host.host_confirm(true, false),
            Confirm::Original,
            "a new run"
        );
        let _ = host.host_confirm(true, true);
        host.cancel();
        assert_eq!(host.host_note(), Note::None, "back from the screen");
        // The rule off: the game's own Resume Run.
        let mut off = Joiner::default();
        assert_eq!(off.host_confirm(true, true), Confirm::Original);
        assert_eq!(off.host_note(), Note::None);
        // The linked joiner: the same refusal, the same line.
        let mut joiner = Joiner::on();
        joiner.on_rules(false, rules(0));
        assert_eq!(
            joiner.confirm(true, true, true, Own::default).1,
            Confirm::ResumeBlocked
        );
        let note = joiner.loadout_frame(true, Own::default).note;
        assert_eq!(
            bar_line(&note, true, "Sam", "Ready", &describe).as_deref(),
            Some(line)
        );
        // Unlinked: taken.
        let mut solo = Joiner::on();
        assert_eq!(
            solo.confirm(false, true, true, Own::default).1,
            Confirm::Original
        );
    }

    #[test]
    fn resume_is_not_offered_and_back_lets_a_wait_go() {
        let mut j = Joiner::on();
        j.on_rules(false, rules(0));
        let _ = j.loadout_frame(true, Own::default);
        assert_eq!(
            j.confirm(true, true, true, Own::default).1,
            Confirm::ResumeBlocked
        );
        assert_eq!(j.loadout_frame(true, Own::default).note, Note::NoResume);
        assert_eq!(j.confirm(true, true, false, Own::default).1, Confirm::Wait);
        assert_eq!(j.loadout_frame(true, Own::default).note, Note::Ready);
        j.cancel();
        assert_eq!(
            j.loadout_frame(true, Own::default).note,
            Note::Picks(rules(0))
        );
    }

    #[test]
    fn a_lost_host_on_the_loadout_gives_the_own_picks_back_at_once() {
        let mut j = Joiner::on();
        let mine = Own {
            mods: 0b1000,
            loadout_daily: None,
        };
        j.on_rules(false, rules(0b1));
        let _ = j.loadout_frame(true, || mine.clone());
        j.host_gone();
        assert!(j.held(), "kept until the next frame");
        let f = j.loadout_frame(false, sentinel);
        assert_eq!(f.restore, Some(mine.clone()));
        assert_eq!(f.show, None);
        j.restored();
        // Or a press in that same frame, before the frame's restore.
        j.on_rules(false, rules(0b1));
        let _ = j.loadout_frame(true, || mine.clone());
        let (restore, c) = j.confirm(false, false, false, sentinel);
        assert_eq!((restore, c), (Some(mine), Confirm::Original));
    }

    #[test]
    fn a_lost_link_mid_run_keeps_the_run_settings_until_the_menu() {
        let mut j = Joiner::on();
        let mine = Own::default();
        j.on_rules(true, rules(0b11));
        let _ = j.confirm(true, true, false, || mine.clone());
        j.confirm_done();
        j.host_gone();
        assert_eq!(
            j.held_rules(),
            Some(&rules(0b11)),
            "the run goes on with them"
        );
        assert_eq!(j.menu_frame(), Some(mine));
    }

    #[test]
    fn reset_progress_while_held_becomes_the_own_picks() {
        let mut j = Joiner::on();
        assert_eq!(j.reset_progress(0), None, "not held");
        j.on_rules(true, rules(0b11));
        let _ = j.confirm(true, true, false, || Own {
            mods: 0b1111,
            loadout_daily: Some(pick(2)),
        });
        j.confirm_done();
        assert_eq!(
            j.reset_progress(0),
            Some(rules(0b11)),
            "the run's settings go back in"
        );
        assert_eq!(
            j.menu_frame(),
            Some(Own {
                mods: 0,
                loadout_daily: Some(pick(2))
            })
        );
    }

    /// A model of the game as the rule sees it: flags in memory, the save, the
    /// resume file, the current screen, and the host. Every `confirmLoadout`
    /// write goes through [`Sim::confirm`], every resume-file write through
    /// [`Sim::save_run`].
    #[allow(clippy::struct_excessive_bools)] // the model's independent facts
    #[derive(Debug, Clone)]
    struct Sim {
        j: Joiner,
        mem: u16,
        disk: u16,
        /// What the player chose (the last own toggle, or a reset).
        chosen: u16,
        linked: bool,
        room: Room,
        host: u16,
        host_started: bool,
        /// The game made its run from the host's current start.
        used_start: bool,
        /// The flags `configureDifficulty` read at the Start.
        start_rules: Option<u16>,
        /// The flags the run's objects read when the fade ended.
        run_rules: Option<u16>,
        /// The co-op run latch (`save_protect`): set at the run's first
        /// frame when linked, cleared at its end; no grace period.
        latch: bool,
        /// A resume-file write carried flags the player did not choose.
        resume_bad: bool,
        /// A run's objects read other flags than its Start did.
        mixed: bool,
        /// The next write of the own picks back fails.
        fail_restore: bool,
        /// The last write of the own picks back failed.
        restore_failed: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Room {
        Menu,
        Loadout,
        /// The loadout screen's 10 to 20 frames after a Start.
        Fade,
        /// The loadout screen's 10 frames back to the menu (`alarm[1]`):
        /// the game's own `confirmLoadout` does nothing.
        Leaving,
        Run,
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Link,
        Unlink,
        HostPicks(u16),
        HostStart,
        HostMenu,
        ToLoadout,
        /// Back from the loadout screen: its fade to the menu begins.
        Back,
        ToMenu,
        Row(u8),
        Start,
        FadeEnds,
        /// `triggerSaveRun` (a wave, a pause, a focus loss, a timer).
        SaveRun,
        RunEnds,
        Reset,
        /// Game End (`triggerSaveRun`), then the game is gone.
        Close,
        FailRestore,
    }

    const OPS: [Op; 17] = [
        Op::Link,
        Op::Unlink,
        Op::HostPicks(0b1010),
        Op::HostStart,
        Op::HostMenu,
        Op::ToLoadout,
        Op::Back,
        Op::ToMenu,
        Op::Row(1),
        Op::Row(4),
        Op::Start,
        Op::FadeEnds,
        Op::SaveRun,
        Op::RunEnds,
        Op::Reset,
        Op::Close,
        Op::FailRestore,
    ];

    impl Sim {
        fn new() -> Self {
            Self {
                j: Joiner::on(),
                mem: 0b1,
                disk: 0b1,
                chosen: 0b1,
                linked: false,
                room: Room::Menu,
                host: 0b0110,
                host_started: false,
                used_start: false,
                start_rules: None,
                run_rules: None,
                latch: false,
                resume_bad: false,
                mixed: false,
                fail_restore: false,
                restore_failed: false,
            }
        }

        fn own(&self) -> Own {
            Own {
                mods: self.mem,
                loadout_daily: None,
            }
        }

        /// The own picks written back, as the glue does it: `false` = the
        /// write failed and the copy stays kept.
        fn restore(&mut self, o: Option<Own>) -> bool {
            let Some(o) = o else { return true };
            self.restore_failed = std::mem::take(&mut self.fail_restore);
            if self.restore_failed {
                return false;
            }
            self.mem = o.mods;
            self.j.restored();
            true
        }

        /// The loadout screen's or the menu's frame, as the before-hooks
        /// run them.
        fn frame(&mut self) {
            match self.room {
                Room::Loadout | Room::Fade | Room::Leaving => {
                    let _ = self.j.frame_tick();
                    let own = self.own();
                    let f = self.j.loadout_frame(self.linked, || own);
                    self.restore(f.restore);
                    if let Some(r) = f.show {
                        self.mem = r.mods;
                    }
                    if self.j.start_due() {
                        self.confirm(0);
                    }
                }
                Room::Menu => {
                    let _ = self.j.frame_tick();
                    let o = self.j.menu_frame();
                    self.restore(o);
                }
                Room::Run => {}
            }
        }

        /// `confirmLoadout` and its writes.
        fn confirm(&mut self, pos: u8) {
            let pos0 = pos == 0;
            let own = self.own();
            let (restore, c) = self.j.confirm(self.linked, pos0, false, || own);
            if !self.restore(restore) {
                // The own picks could not be written back: the press is skipped.
                self.j.confirm_done();
                return;
            }
            match c {
                // The game's own does nothing during the fade (`alarm[2]`)
                // or the fade back to the menu (`alarm[1]`).
                Confirm::Original if matches!(self.room, Room::Fade | Room::Leaving) => {}
                // The host's are written, the game's own does nothing: the
                // after-hook puts the host's start back.
                Confirm::Apply(r) if self.room == Room::Leaving => {
                    self.mem = r.mods;
                    self.j.start_not_taken();
                }
                Confirm::Original => {
                    if pos0 {
                        self.room = Room::Fade;
                        self.start_rules = Some(self.mem);
                        // The after-hook saw the fade start.
                        self.j.started_own();
                    } else {
                        self.mem ^= 1 << (pos - 1);
                        self.chosen = self.mem;
                    }
                    // Every press writes the 12 picks.
                    if !self.j.skip_writes() {
                        self.disk = self.mem;
                    }
                }
                Confirm::Apply(r) => {
                    self.mem = r.mods;
                    self.room = Room::Fade;
                    self.used_start = true;
                    self.start_rules = Some(self.mem);
                    if !self.j.skip_writes() {
                        self.disk = self.mem;
                    }
                }
                Confirm::Locked | Confirm::ResumeBlocked | Confirm::Wait | Confirm::Starting => {}
            }
            self.j.confirm_done();
        }

        /// `triggerSaveRun`: skipped while latched or while the host's are
        /// held; the game's own guard writes only during a run.
        fn save_run(&mut self) {
            if self.latch || self.j.held() {
                return;
            }
            if self.room == Room::Run && self.mem != self.chosen {
                self.resume_bad = true;
            }
        }

        /// The link and the host's messages.
        fn host_op(&mut self, op: Op) {
            match op {
                Op::Link if !self.linked && self.room != Room::Run => {
                    self.linked = true;
                    self.host_started = false;
                    // The host sends its picks at a link.
                    self.j.on_rules(false, rules(self.host));
                }
                Op::Unlink if self.linked => {
                    self.linked = false;
                    self.host_started = false;
                    self.j.host_gone();
                }
                Op::HostPicks(m) if self.linked && !self.host_started => {
                    self.host ^= m;
                    self.j.on_rules(false, rules(self.host));
                }
                Op::HostStart if self.linked && !self.host_started => {
                    self.host_started = true;
                    self.used_start = false;
                    self.j.on_rules(true, rules(self.host));
                }
                Op::HostMenu if self.linked && self.host_started => {
                    self.host_started = false;
                    self.used_start = false;
                    self.j.on_rules(false, rules(self.host));
                }
                _ => {}
            }
        }

        fn apply(&mut self, op: Op) {
            match op {
                Op::Link | Op::Unlink | Op::HostPicks(_) | Op::HostStart | Op::HostMenu => {
                    self.host_op(op);
                }
                Op::ToLoadout => {
                    if self.room == Room::Menu {
                        self.room = Room::Loadout;
                    }
                }
                Op::Back => {
                    if self.room == Room::Loadout {
                        // `leaveLoadout`: nothing waits.
                        self.j.cancel();
                        self.room = Room::Leaving;
                    }
                }
                Op::ToMenu => {
                    if self.room == Room::Loadout {
                        self.j.cancel();
                        self.room = Room::Menu;
                    } else if self.room == Room::Leaving {
                        self.room = Room::Menu;
                    }
                }
                Op::Row(i) => {
                    if matches!(self.room, Room::Loadout | Room::Fade | Room::Leaving) {
                        self.confirm(i);
                    }
                }
                Op::Start => {
                    if matches!(self.room, Room::Loadout | Room::Fade | Room::Leaving) {
                        self.confirm(0);
                    }
                }
                Op::FadeEnds => {
                    if self.room == Room::Fade {
                        self.room = Room::Run;
                        self.run_rules = Some(self.mem);
                        self.latch = self.linked;
                        if self.start_rules != self.run_rules {
                            self.mixed = true;
                        }
                    }
                }
                Op::SaveRun => self.save_run(),
                Op::RunEnds => {
                    if self.room == Room::Run {
                        // The end screen writes records, not settings.
                        self.room = Room::Menu;
                        self.start_rules = None;
                        self.run_rules = None;
                        self.latch = false;
                    }
                }
                // resetProgress (from the menu, or the pause screen in a
                // run): defaults to memory and the save.
                Op::Reset if matches!(self.room, Room::Menu | Room::Run) => {
                    self.mem = 0;
                    self.disk = 0;
                    self.chosen = 0;
                    if let Some(r) = self.j.reset_progress(self.mem) {
                        self.mem = r.mods;
                    }
                }
                Op::Close => {
                    // Game End saves the run first; the next boot reads the save.
                    self.save_run();
                    let (disk, chosen, bad) = (self.disk, self.chosen, self.resume_bad);
                    *self = Self::new();
                    self.mem = disk;
                    self.disk = disk;
                    self.chosen = chosen;
                    self.resume_bad = bad;
                }
                Op::Reset => {}
                Op::FailRestore => self.fail_restore = true,
            }
            self.frame();
        }

        fn check(&self, trail: &[Op]) {
            assert_eq!(
                self.disk, self.chosen,
                "the save holds only what the player chose: {trail:?}"
            );
            assert!(
                !self.resume_bad,
                "the resume file holds only what the player chose: {trail:?}"
            );
            assert!(
                !self.mixed,
                "a run is made from one set of settings: {trail:?}"
            );
            if !self.j.held() {
                assert_eq!(
                    self.mem, self.chosen,
                    "nothing held: the own picks in memory: {trail:?}"
                );
            }
            if self.room == Room::Menu && !self.restore_failed {
                assert!(
                    !self.j.held(),
                    "the menu gives the own picks back: {trail:?}"
                );
            }
            if self.room == Room::Loadout && !self.linked && !self.restore_failed {
                assert!(!self.j.held(), "unlinked loadout: the own picks: {trail:?}");
            }
            if self.room == Room::Fade && self.j.held() {
                assert_eq!(
                    Some(self.mem),
                    self.start_rules,
                    "the fade keeps the start's settings: {trail:?}"
                );
            }
            if self.room == Room::Run && self.j.held() {
                assert_eq!(
                    Some(self.mem),
                    self.run_rules,
                    "a run keeps the settings it was made from: {trail:?}"
                );
            }
            if self.linked
                && self.host_started
                && !self.used_start
                && matches!(self.room, Room::Menu | Room::Loadout | Room::Leaving)
            {
                assert!(
                    self.j.host_started(),
                    "the host's start waits until it is taken: {trail:?}"
                );
            }
        }
    }

    /// Every sequence of 6 steps (17^6), each followed by the frame that
    /// runs after it.
    #[test]
    fn no_sequence_leaves_a_flag_in_the_save_the_player_did_not_choose() {
        fn walk(sim: &Sim, trail: &mut Vec<Op>, depth: usize, n: &mut u64) {
            if depth == 0 {
                *n += 1;
                return;
            }
            for op in OPS {
                let mut s = sim.clone();
                trail.push(op);
                s.apply(op);
                s.check(trail);
                walk(&s, trail, depth - 1, n);
                trail.pop();
            }
        }
        let mut n = 0;
        walk(&Sim::new(), &mut Vec::new(), 6, &mut n);
        assert_eq!(n, 17u64.pow(6));
    }

    /// Long random sequences (a fixed LCG, reproducible).
    #[test]
    fn long_random_sequences_keep_the_rule() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..4000 {
            let mut s = Sim::new();
            let mut trail = Vec::new();
            for _ in 0..80 {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let op = OPS[usize::try_from(x >> 59).unwrap_or(0) % OPS.len()];
                trail.push(op);
                s.apply(op);
                s.check(&trail);
            }
        }
    }

    fn run_ops(ops: &[Op]) -> Sim {
        let mut s = Sim::new();
        for &op in ops {
            s.apply(op);
            s.check(ops);
        }
        s
    }

    #[test]
    fn a_run_from_the_host_start_holds_its_settings_whatever_the_joiner_picked() {
        let mut s = run_ops(&[
            Op::Link,
            Op::HostPicks(0b1010),
            Op::ToLoadout,
            Op::Row(1),
            Op::Start,
            Op::HostStart,
            Op::FadeEnds,
        ]);
        assert_eq!(s.room, Room::Run);
        assert_eq!(s.mem, 0b1100, "the host's");
        assert_eq!(s.disk, 0b1, "the own picks untouched (the row was locked)");
        s.apply(Op::Close);
        assert_eq!(
            s.mem, 0b1,
            "closed mid-run: the next boot has the own picks"
        );
    }

    /// A run started with the host's held and the link lost before the first
    /// run frame (no latch): the resume file stays off all the same.
    #[test]
    fn a_held_run_without_the_latch_writes_no_resume_file() {
        let s = run_ops(&[
            Op::Link,
            Op::ToLoadout,
            Op::HostStart,
            Op::Start,
            Op::Unlink,
            Op::FadeEnds,
            Op::SaveRun,
            Op::Close,
        ]);
        assert!(!s.resume_bad);
        let s = run_ops(&[
            Op::Link,
            Op::ToLoadout,
            Op::HostStart,
            Op::Start,
            Op::Unlink,
            Op::FadeEnds,
        ]);
        assert!(!s.latch && s.j.held(), "the window the latch misses");
    }

    /// A failed write-back skips the next press and is tried again at the
    /// next frame.
    #[test]
    fn a_failed_write_back_keeps_the_own_copy_and_skips_the_press() {
        let mut s = run_ops(&[Op::Link, Op::ToLoadout, Op::Unlink]);
        assert!(!s.j.held(), "written back at the unlinked frame");
        s = run_ops(&[Op::Link, Op::ToLoadout]);
        assert!(s.j.held());
        s.linked = false;
        s.j.host_gone();
        s.fail_restore = true;
        s.confirm(1);
        assert_eq!(s.disk, 0b1, "the press was skipped");
        assert!(s.j.held(), "the own copy kept");
        s.frame();
        assert!(!s.j.held());
        assert_eq!(s.mem, 0b1);
    }
}

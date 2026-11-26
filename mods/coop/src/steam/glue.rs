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

//! Game calls for the Steam connector: the per-frame pump, the end-of-frame
//! flush, and the hidden async listener and its leaderboard guards. The pure
//! modules ([`super::lobby`], [`super::pump`]) decide.
//!
//! A GML runtime error inside a mod's call ends the game, so: no Steam call
//! before `steam_initialised()` and a nonzero own id; `instance_exists` and
//! `buffer_exists` before id-based calls; no lock held across a game call.
//! Steam ids, lobby ids and persona names are never logged at info or above.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ndml_net::lobby::{
    ListenerState, SteamFileAction, SteamFileRequest, SteamMember, SteamRequest, SteamStatus,
};
use ndml_net::packet::MAX_DATAGRAM;
use ndml_net::steam::SteamBridge;
use ndml_net::worker::NetHandle;
use ndml_sdk::{Builtin, Call, ModContext, Script, Value, Variable};

use super::events::{self, SteamEvent};
use super::lobby::{
    Cmd, KEY_BUILD, KEY_FILES, KEY_MOD, KEY_STATE, ListPoll, LobbyData, Machine, MachineConfig,
    NetCmd, Outcome, Poll, link_of,
};
use super::names::{self, builtin_fn as bf, steam_fn as sf};
use super::pump::{self, Availability, ListenerPolicy, ListenerStep, Probe, PumpStats};
use super::settings::SteamSettings;
use super::text;
use super::view::{MemberView, Offers, SteamAction, SteamPage, SteamRole, SteamView, Visibility};
use crate::game::{call_b, info_once, warn_once};

const AVAIL_UNKNOWN: u8 = 0;
const AVAIL_YES: u8 = 1;
const AVAIL_NO: u8 = 2;

/// How often the link status is read while active (ticks).
const STATUS_EVERY: u64 = 6;
/// How often the pump's cost is logged (debug).
const STATS_EVERY: Duration = Duration::from_secs(30);
/// Most async events kept between two ticks.
const INBOX_MAX: usize = 64;

/// Builtins, scripts and variables, resolved at init (each optional).
#[derive(Debug, Clone)]
struct Api {
    initialised: Option<Builtin>,
    my_id: Option<Builtin>,
    update: Option<Builtin>,
    overlay_enabled: Option<Builtin>,
    lobby_create: Option<Builtin>,
    lobby_join: Option<Builtin>,
    lobby_leave: Option<Builtin>,
    lobby_set_type: Option<Builtin>,
    lobby_set_owner: Option<Builtin>,
    lobby_id: Option<Builtin>,
    lobby_owner: Option<Builtin>,
    lobby_members: Option<Builtin>,
    lobby_member: Option<Builtin>,
    lobby_set_data: Option<Builtin>,
    lobby_get_data: Option<Builtin>,
    lobby_invite: Option<Builtin>,
    list_filter: Option<Builtin>,
    list_distance: Option<Builtin>,
    list_request: Option<Builtin>,
    list_loading: Option<Builtin>,
    list_count: Option<Builtin>,
    list_join: Option<Builtin>,
    send: Option<Builtin>,
    receive: Option<Builtin>,
    size: Option<Builtin>,
    data: Option<Builtin>,
    sender: Option<Builtin>,
    persona: Option<Builtin>,
    buffer_create: Option<Builtin>,
    buffer_exists: Option<Builtin>,
    b64_decode_ext: Option<Builtin>,
    b64_encode: Option<Builtin>,
    exists: Option<Builtin>,
    activate: Option<Builtin>,
    destroy: Option<Builtin>,
    asset: Option<Builtin>,
    map_value: Option<Builtin>,
    instance_create: Option<Script>,
    gog: Option<Variable>,
}

impl Api {
    fn resolve(ctx: &ModContext) -> Self {
        let b = |n: &str| {
            ctx.builtin(n)
                .map_err(|e| log::warn!("co-op steam: builtin {n}: {e}"))
                .ok()
        };
        Self {
            initialised: b(sf::INITIALISED),
            my_id: b(sf::MY_ID),
            update: b(sf::UPDATE),
            overlay_enabled: b(sf::OVERLAY_ENABLED),
            lobby_create: b(sf::LOBBY_CREATE),
            lobby_join: b(sf::LOBBY_JOIN),
            lobby_leave: b(sf::LOBBY_LEAVE),
            lobby_set_type: b(sf::LOBBY_SET_TYPE),
            lobby_set_owner: b(sf::LOBBY_SET_OWNER),
            lobby_id: b(sf::LOBBY_ID),
            lobby_owner: b(sf::LOBBY_OWNER),
            lobby_members: b(sf::LOBBY_MEMBERS),
            lobby_member: b(sf::LOBBY_MEMBER),
            lobby_set_data: b(sf::LOBBY_SET_DATA),
            lobby_get_data: b(sf::LOBBY_GET_DATA),
            lobby_invite: b(sf::LOBBY_INVITE),
            list_filter: b(sf::LIST_STRING_FILTER),
            list_distance: b(sf::LIST_DISTANCE_FILTER),
            list_request: b(sf::LIST_REQUEST),
            list_loading: b(sf::LIST_LOADING),
            list_count: b(sf::LIST_COUNT),
            list_join: b(sf::LIST_JOIN),
            send: b(sf::PACKET_SEND),
            receive: b(sf::PACKET_RECEIVE),
            size: b(sf::PACKET_SIZE),
            data: b(sf::PACKET_DATA),
            sender: b(sf::PACKET_SENDER),
            persona: b(sf::PERSONA_NAME),
            buffer_create: b(bf::BUFFER_CREATE),
            buffer_exists: b(bf::BUFFER_EXISTS),
            b64_decode_ext: b(bf::BUFFER_B64_DECODE_EXT),
            b64_encode: b(bf::BUFFER_B64_ENCODE),
            exists: b(bf::INSTANCE_EXISTS),
            activate: b(bf::INSTANCE_ACTIVATE_OBJECT),
            destroy: b(bf::INSTANCE_DESTROY),
            asset: b(bf::ASSET_GET_INDEX),
            map_value: b(bf::DS_MAP_FIND_VALUE),
            instance_create: ctx
                .script(names::INSTANCE_CREATE)
                .map_err(|e| log::warn!("co-op steam: unable to create the invite listener ({e})"))
                .ok(),
            gog: ctx.variable(names::GLOBAL_GOG).ok(),
        }
    }
}

/// Objects and rooms by name (`asset_get_index`), resolved once.
#[derive(Debug, Clone, Copy, Default)]
struct Assets {
    listener: Option<u32>,
    leaderboard: [Option<u32>; 2],
    room_game: Option<i64>,
    room_leaderboard: Option<i64>,
}

#[derive(Debug)]
struct State {
    bridge: SteamBridge,
    machine: Machine,
    me: u64,
    tries: u32,
    names: BTreeMap<u64, String>,
    bufs: Option<(Value, Value)>,
    listener: ListenerPolicy,
    listener_id: Option<Value>,
    assets: Option<Assets>,
    launch_lobby: Option<u64>,
    actions: VecDeque<SteamAction>,
    sent_status: Option<SteamStatus>,
    overlay: Option<bool>,
    room: Option<i64>,
    room_ticks: u64,
    stats: PumpStats,
    stats_since: Option<Instant>,
    /// The last active pump that moved packets ([`pump::backlog_is_stale`]).
    last_move: Option<Instant>,
}

/// The Steam connector, shared by its hooks and the UI ([`super::view`]).
#[derive(Debug)]
pub struct SteamCoop {
    settings: SteamSettings,
    api: Api,
    net: NetHandle,
    tick: AtomicU64,
    avail: AtomicU8,
    active: AtomicBool,
    linger_until: AtomicU64,
    creating: AtomicBool,
    listener_key: AtomicUsize,
    /// Sends Steam refused this session (the link line, `crate::link_log`).
    refused: AtomicU64,
    inbox: Mutex<Vec<SteamEvent>>,
    st: Mutex<State>,
}

/// The [`files_id`](super::lobby::files_id) of this mod's own DLL, or empty
/// when it can't be read (the lobby then does not compare files).
fn own_files_id(ctx: &ModContext) -> String {
    let file = format!("{}.dll", ctx.name());
    let mut candidates = Vec::new();
    if let Some(root) = ctx.config_dir().parent() {
        candidates.push(root.join("mods").join(&file));
        if let Some(ndml) = root
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "ndml"))
        {
            candidates.push(ndml.join("mods").join(&file));
        }
    }
    let id = candidates
        .iter()
        .find_map(|p| std::fs::read(p).ok())
        .map_or_default(|b| super::lobby::files_id(&b));
    if id.is_empty() {
        log::warn!("co-op steam: unable to read the mod's own DLL");
    } else {
        log::info!("co-op steam: co-op mod files {id} (both players need the same)");
    }
    id
}

fn idv(id: u64) -> Value {
    Value::Int(id as i64)
}

/// A datagram length (at most 1200) as a GML number.
fn len_value(n: usize) -> Value {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX)).into()
}

/// What one receive found.
enum Recv {
    Empty,
    /// Empty, oversize or unreadable: dropped.
    Skipped,
    Got(u64, Vec<u8>),
}

/// A Steam (or lobby) id getter.
fn read_id(call: &Call<'_>, b: Option<Builtin>) -> u64 {
    events::id_of(call_b(call, b, &[]).as_ref())
}

/// GML truth of a call result (`undefined` counts as "no complaint").
fn ok_result(v: Option<&Value>) -> bool {
    v.is_some_and(|v| v.as_bool() != Some(false))
}

fn index_of(v: &Value) -> Option<i64> {
    match v {
        Value::Ref(p) => Some((p & 0xffff_ffff) as i64),
        other => other.as_f64().filter(|f| *f >= 0.0).map(|f| f as i64),
    }
}

fn is_instance(v: &Value) -> bool {
    match v {
        Value::Ref(_) => true,
        other => other.as_f64().is_some_and(|n| n >= 100_000.0),
    }
}

/// The most the game's close waits for a goodbye to reach Steam's queue.
pub const GOODBYE_WAIT: std::time::Duration = std::time::Duration::from_millis(2);

impl SteamCoop {
    /// The game is closing: move the queued goodbyes to Steam, waiting at
    /// most `GOODBYE_WAIT`. Returns the datagrams moved.
    pub fn flush_on_exit(&self, call: &Call<'_>) -> usize {
        if self.availability() != Availability::Yes || !self.active.load(Ordering::Relaxed) {
            return 0;
        }
        let until = Instant::now() + GOODBYE_WAIT;
        let mut moved = 0;
        loop {
            moved += self.move_packets(call, false).0;
            if moved > 0 || Instant::now() >= until {
                return moved;
            }
            std::thread::yield_now();
        }
    }

    /// Resolve everything (no game calls); `bridge` is the game side of the
    /// lobby agent's Steam transport.
    #[must_use]
    pub fn new(
        ctx: &ModContext,
        settings: SteamSettings,
        net: NetHandle,
        bridge: SteamBridge,
        mod_version: u32,
        build: String,
    ) -> Self {
        let args: Vec<String> = std::env::args().collect();
        let launch_lobby = text::connect_lobby_arg(&args);
        if launch_lobby.is_some() {
            log::info!("co-op steam: started from an invite");
        }
        let files = own_files_id(ctx);
        let machine = Machine::new(MachineConfig {
            mod_version,
            build,
            files,
            max_members: super::settings::LOBBY_SIZE,
            visibility: settings.visibility,
            seed: ndml_net::rng::random_u64(),
        });
        Self {
            api: Api::resolve(ctx),
            listener_key: AtomicUsize::new(0),
            st: Mutex::new(State {
                bridge,
                machine,
                me: 0,
                tries: 0,
                names: BTreeMap::new(),
                bufs: None,
                listener: ListenerPolicy::new(),
                listener_id: None,
                assets: None,
                launch_lobby,
                actions: VecDeque::new(),
                sent_status: None,
                overlay: None,
                room: None,
                room_ticks: 0,
                stats: PumpStats::default(),
                stats_since: None,
                last_move: None,
            }),
            settings,
            net,
            tick: AtomicU64::new(0),
            avail: AtomicU8::new(AVAIL_UNKNOWN),
            active: AtomicBool::new(false),
            linger_until: AtomicU64::new(0),
            creating: AtomicBool::new(false),
            refused: AtomicU64::new(0),
            inbox: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::game::lock_or_report(&self.st, "steam")
    }

    /// The settings it runs with.
    #[must_use]
    pub fn settings(&self) -> &SteamSettings {
        &self.settings
    }

    fn availability(&self) -> Availability {
        match self.avail.load(Ordering::Relaxed) {
            AVAIL_YES => Availability::Yes,
            AVAIL_NO => Availability::No,
            _ => Availability::Unknown,
        }
    }

    /// The link line's Steam counts (`crate::link_log`): sends Steam refused
    /// this session and datagrams the bridge dropped. `None` while Steam is
    /// not up. Takes the connector's lock briefly: call it with no other lock
    /// held.
    #[must_use]
    pub fn link_counts(&self) -> Option<crate::link_log::SteamCounts> {
        if self.availability() != Availability::Yes {
            return None;
        }
        let b = self.lock().bridge.stats();
        Some(crate::link_log::SteamCounts {
            refused: self.refused.load(Ordering::Relaxed),
            bridge_drops: b.dropped_out + b.dropped_in,
        })
    }

    /// Whether Steam is up (known after the first pump ticks).
    #[must_use]
    pub fn available(&self) -> bool {
        self.availability() == Availability::Yes
    }

    /// Queue a UI action for the next pump tick (never calls the game).
    pub fn act(&self, action: SteamAction) {
        // A typed room code (a lobby's id) never goes to the log.
        if matches!(action, SteamAction::JoinCode(_)) {
            log::debug!("co-op steam: action JoinCode");
        } else {
            log::debug!("co-op steam: action {action:?}");
        }
        let mut st = self.lock();
        if st.actions.len() < 16 {
            st.actions.push_back(action);
        }
        self.active.store(true, Ordering::Relaxed);
    }

    /// The connector at a glance (see [`SteamView`]).
    #[must_use]
    pub fn view(&self) -> SteamView {
        let available = self.available();
        let st = self.lock();
        compose_view(&st, available, Instant::now())
    }

    /// `oGamepad_Step_1`, before: one tick (one atomic load when unused).
    pub fn pump(&self, call: &Call<'_>) {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed) + 1;
        match self.availability() {
            Availability::No => return,
            Availability::Unknown => {
                if tick % pump::AVAILABLE_EVERY != 1 || !self.check_available(call) {
                    return;
                }
            }
            Availability::Yes => {}
        }
        let active =
            self.active.load(Ordering::Relaxed) || tick < self.linger_until.load(Ordering::Relaxed);
        if !pump::runs(active, tick) {
            return;
        }
        let t0 = Instant::now();
        let room = room_index(call);
        self.ensure_assets(call);
        self.note_room(room);
        self.listener_tick(call, room, tick);
        let _ = call_b(call, self.api.update, &[]);
        if pump::recheck_overlay(self.lock().overlay, tick) {
            self.overlay_now(call);
        }
        let (sent, received, failed) = if active {
            let gap = self
                .lock()
                .last_move
                .replace(t0)
                .map(|t| t0.saturating_duration_since(t));
            if gap.is_some_and(pump::backlog_is_stale) {
                self.drop_backlog(call, gap.unwrap_or_default());
            }
            self.move_packets(call, true)
        } else {
            self.lock().last_move = None;
            (0, 0, 0)
        };
        let mut cmds = self.poll(call, tick);
        cmds.extend(self.process(room, tick, active, t0));
        self.execute(call, cmds, t0);
        self.account(t0, sent, received, failed);
    }

    /// `oGuiCursor_Draw_75`, after: send what the frame queued.
    pub fn flush(&self, call: &Call<'_>) {
        if self.availability() != Availability::Yes || !self.active.load(Ordering::Relaxed) {
            return;
        }
        let t0 = Instant::now();
        let (sent, _, failed) = self.move_packets(call, false);
        if sent + failed > 0 {
            self.account(t0, sent, 0, failed);
        }
    }

    fn check_available(&self, call: &Call<'_>) -> bool {
        let probe = self.probe(call);
        let tries = {
            let mut st = self.lock();
            st.tries += 1;
            st.tries
        };
        match pump::availability(probe, tries) {
            Availability::Yes => {
                let Probe::Up(me) = probe else { return false };
                let overlay = self.read_overlay(call);
                {
                    let mut st = self.lock();
                    st.me = me;
                    st.bridge.set_local_id(me);
                    st.machine.set_me(me);
                    st.overlay = Some(overlay);
                }
                self.avail.store(AVAIL_YES, Ordering::Relaxed);
                log::info!(
                    "co-op steam: Steam ready (overlay {})",
                    if overlay { "on" } else { "off" }
                );
                true
            }
            Availability::No => {
                self.avail.store(AVAIL_NO, Ordering::Relaxed);
                log::info!(
                    "co-op steam: Steam unavailable ({})",
                    match probe {
                        Probe::NotSteam => "not a Steam build, or no Steamworks extension",
                        _ => "not initialised or not logged in",
                    }
                );
                self.publish_status(false);
                false
            }
            Availability::Unknown => false,
        }
    }

    /// `steam_is_overlay_enabled` now (no answer counts as on).
    fn read_overlay(&self, call: &Call<'_>) -> bool {
        call_b(call, self.api.overlay_enabled, &[])
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    /// Ask the overlay again and keep the answer; logs when it turns on.
    fn overlay_now(&self, call: &Call<'_>) -> bool {
        let on = self.read_overlay(call);
        let was = self.lock().overlay.replace(on);
        if was == Some(false) && on {
            log::info!("co-op steam: overlay on");
        }
        on
    }

    fn probe(&self, call: &Call<'_>) -> Probe {
        if let (Some(g), Some(v)) = (call.global(), self.api.gog)
            && g.get_f64(v).is_some_and(|x| x > 0.5)
        {
            return Probe::NotSteam;
        }
        let Some(init) = self.api.initialised else {
            return Probe::NotSteam;
        };
        if call.builtin_available(init) == Ok(false) {
            return Probe::NotSteam;
        }
        let up = call
            .call_builtin(init, &[])
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !up {
            return Probe::NotYet;
        }
        Probe::Up(events::id_of(call_b(call, self.api.my_id, &[]).as_ref()))
    }

    fn ensure_assets(&self, call: &Call<'_>) {
        if self.lock().assets.is_some() {
            return;
        }
        let find = |name: &str| -> Option<i64> {
            let v = call_b(call, self.api.asset, &[name.into()])?;
            index_of(&v)
        };
        let obj = |name: &str| find(name).and_then(|i| u32::try_from(i).ok());
        let assets = Assets {
            listener: obj(names::LISTENER_OBJECT),
            leaderboard: names::LEADERBOARD_OBJECTS.map(obj),
            room_game: find(names::ROOM_GAME),
            room_leaderboard: find(names::ROOM_LEADERBOARD),
        };
        if assets.listener.is_none() {
            warn_once(
                "steam-listener-object",
                "co-op steam: oHighscoresDynamic not found; no async listener (polling only)",
            );
        }
        self.lock().assets = Some(assets);
    }

    fn note_room(&self, room: Option<i64>) {
        let mut st = self.lock();
        st.room_ticks += 1;
        if room != st.room {
            let msg = format!(
                "co-op steam: pump now in room {} ({} ticks in room {})",
                room.map_or_else(|| "?".into(), |r| r.to_string()),
                st.room_ticks,
                st.room.map_or_else(|| "?".into(), |r| r.to_string())
            );
            log::debug!("{msg}");
            st.room = room;
            st.room_ticks = 0;
        }
    }

    fn account(&self, t0: Instant, sent: usize, received: usize, failed: usize) {
        if failed > 0 {
            self.refused.fetch_add(failed as u64, Ordering::Relaxed);
        }
        let mut st = self.lock();
        st.stats.tick(t0.elapsed(), sent, received, failed);
        let since = *st.stats_since.get_or_insert(t0);
        let over = t0.saturating_duration_since(since);
        if over >= STATS_EVERY {
            let line = format!(
                "co-op steam: pump over {:.0} s: {}",
                over.as_secs_f64(),
                st.stats.line(over)
            );
            log::debug!("{line}");
            if st.stats.send_failed > 0 {
                // At most one line per window, only in a window with refusals.
                log::warn!(
                    "co-op steam: unable to send some packets (Steam refused {} in the last {:.0} s, {} this session)",
                    st.stats.send_failed,
                    over.as_secs_f64(),
                    self.refused.load(Ordering::Relaxed)
                );
            }
            st.stats = PumpStats::default();
            st.stats_since = Some(t0);
        }
    }

    fn exists(&self, call: &Call<'_>, v: &Value) -> bool {
        self.api.exists.is_some_and(|b| {
            call.call_builtin(b, std::slice::from_ref(v))
                .ok()
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
        })
    }

    fn listener_tick(&self, call: &Call<'_>, room: Option<i64>, tick: u64) {
        let (step, id) = {
            let st = self.lock();
            let lb_room = room.is_some()
                && st.assets.and_then(|a| a.room_leaderboard).is_some()
                && room == st.assets.and_then(|a| a.room_leaderboard);
            (st.listener.step(lb_room, tick), st.listener_id.clone())
        };
        match step {
            ListenerStep::Nothing => {}
            ListenerStep::Destroy => {
                self.destroy_listener(call);
                self.lock().listener.suppress();
                log::debug!("co-op steam: listener out of the way (leaderboard room)");
            }
            ListenerStep::CheckClear => {
                if self.leaderboard_clear(call) {
                    self.lock().listener.clear();
                    log::debug!("co-op steam: leaderboard gone; listener comes back");
                }
            }
            ListenerStep::Ensure => {
                let alive = self.ensure_listener(call, id.as_ref());
                if self.lock().listener.ensured(alive) {
                    warn_once(
                        "steam-listener-failed",
                        "co-op steam: the async listener could not be created; polling only \
                         (hosting and room codes work, invites accepted while the game runs \
                         don't arrive)",
                    );
                }
            }
        }
    }

    fn ensure_listener(&self, call: &Call<'_>, id: Option<&Value>) -> Option<bool> {
        if let Some(id) = id {
            if self.exists(call, id) {
                return Some(true);
            }
            // Deactivated by a freeze (or gone after game_restart).
            let _ = call_b(call, self.api.activate, std::slice::from_ref(id));
            if self.exists(call, id) {
                return Some(true);
            }
        }
        let obj = self.lock().assets.and_then(|a| a.listener)?;
        let script = self.api.instance_create?;
        self.creating.store(true, Ordering::Relaxed);
        let made = call.call_script(
            script,
            &[(-10_000.0).into(), (-10_000.0).into(), Value::object(obj)],
        );
        self.creating.store(false, Ordering::Relaxed);
        let Ok(v) = made else {
            return Some(false);
        };
        if !is_instance(&v) || !self.exists(call, &v) {
            return Some(false);
        }
        let mut key = 0;
        let _ = call.with(&v, |inst, _| {
            key = inst.key();
            let _ = inst.set_builtin_f64("visible", 0.0);
            let _ = inst.set_builtin_f64("persistent", 1.0);
        });
        if key == 0 {
            return Some(false);
        }
        self.listener_key.store(key, Ordering::Relaxed);
        self.lock().listener_id = Some(v);
        info_once(
            "steam-listener",
            "co-op steam: async listener created (hidden, persistent)",
        );
        Some(true)
    }

    fn destroy_listener(&self, call: &Call<'_>) {
        let id = self.lock().listener_id.take();
        self.listener_key.store(0, Ordering::Relaxed);
        if let Some(id) = id {
            let _ = call_b(call, self.api.activate, std::slice::from_ref(&id));
            if self.exists(call, &id) {
                let _ = call_b(call, self.api.destroy, &[id, Value::Bool(false)]);
            }
        }
    }

    fn leaderboard_clear(&self, call: &Call<'_>) -> bool {
        let Some(a) = self.lock().assets else {
            return false;
        };
        let present = |o: Option<u32>| o.is_some_and(|i| self.exists(call, &Value::object(i)));
        !(a.leaderboard.iter().any(|&o| present(o)) || present(a.listener))
    }

    /// Before leaderboard code that reaches `oHighscoresDynamic`: remove the listener.
    pub fn guard(&self, call: &Call<'_>) {
        if self.listener_key.load(Ordering::Relaxed) == 0 {
            return;
        }
        self.destroy_listener(call);
        self.lock().listener.suppress();
        log::debug!("co-op steam: listener out of the way (leaderboard object)");
    }

    fn is_listener(&self, call: &Call<'_>) -> bool {
        let k = self.listener_key.load(Ordering::Relaxed);
        k != 0 && call.instance().is_some_and(|i| i.key() == k)
    }

    /// `oHighscoresDynamic_Create_0`, before: skip while the listener is created.
    pub fn listener_create(&self, call: &mut Call<'_>) {
        if self.creating.load(Ordering::Relaxed) {
            call.skip_original();
        }
    }

    /// The listener's other events, before: skip them for the listener.
    pub fn listener_event(&self, call: &mut Call<'_>) {
        if self.is_listener(call) {
            call.skip_original();
        }
    }

    /// `oHighscoresDynamic_Other_69`, before: read `async_load`. The game's
    /// own instance is read only while the listener is away, so each event counts once.
    pub fn listener_async(&self, call: &mut Call<'_>) {
        if self.is_listener(call) {
            call.skip_original();
        } else if self.listener_key.load(Ordering::Relaxed) != 0 {
            return;
        }
        if self.availability() != Availability::Yes {
            return;
        }
        let Some(map) = call
            .instance()
            .and_then(|me| me.builtin_value(names::VAR_ASYNC_LOAD).ok())
        else {
            return;
        };
        let get = |key: &str| call_b(call, self.api.map_value, &[map.clone(), key.into()]);
        let Some(kind) = get(events::EVENT_TYPE).and_then(|v| v.as_str().map(str::to_owned)) else {
            return;
        };
        if !events::wanted(&kind) {
            return;
        }
        let fields: Vec<(&str, Value)> = events::keys_for(&kind)
            .iter()
            .filter_map(|&k| get(k).map(|v| (k, v)))
            .collect();
        let lookup = |k: &str| fields.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        let Some(ev) = events::parse(&kind, &lookup) else {
            return;
        };
        log::debug!("co-op steam: async {kind}");
        let mut inbox = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        if inbox.len() < INBOX_MAX {
            inbox.push(ev);
        }
        self.active.store(true, Ordering::Relaxed);
    }

    fn buffer_ok(&self, call: &Call<'_>, b: &Value) -> bool {
        call_b(call, self.api.buffer_exists, std::slice::from_ref(b))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn buffers(&self, call: &Call<'_>) -> Option<(Value, Value)> {
        let cur = self.lock().bufs.clone();
        if let Some((s, r)) = &cur
            && self.buffer_ok(call, s)
            && self.buffer_ok(call, r)
        {
            return cur;
        }
        let make = || {
            call_b(
                call,
                self.api.buffer_create,
                &[2048.0.into(), names::BUFFER_GROW.into(), 1.0.into()],
            )
            .filter(|v| v.as_f64().is_some_and(|n| n >= 0.0) || matches!(v, Value::Ref(_)))
        };
        let pair = (make()?, make()?);
        self.lock().bufs = Some(pair.clone());
        Some(pair)
    }

    fn send_one(&self, call: &Call<'_>, buf: &Value, to: u64, data: &[u8]) -> bool {
        let encoded = text::b64_encode(data);
        if call_b(
            call,
            self.api.b64_decode_ext,
            &[buf.clone(), encoded.into(), 0.0.into()],
        )
        .is_none()
        {
            return false;
        }
        let r = call_b(
            call,
            self.api.send,
            &[
                idv(to),
                buf.clone(),
                len_value(data.len()),
                names::SEND_UNRELIABLE.into(),
            ],
        );
        ok_result(r.as_ref())
    }

    /// After a stall past the link's timeout: drop both queues.
    fn drop_backlog(&self, call: &Call<'_>, gap: Duration) {
        let queued = {
            let st = self.lock();
            std::iter::from_fn(|| st.bridge.pop_outbound()).count()
        };
        let mut waiting = 0;
        if let Some((_, rbuf)) = self.buffers(call) {
            while waiting < pump::MAX_DISCARD {
                match self.recv_one(call, &rbuf) {
                    Recv::Empty => break,
                    Recv::Skipped | Recv::Got(..) => waiting += 1,
                }
            }
        }
        log::warn!(
            "co-op steam: the game thread did not pump for {} ms (past the link's timeout); dropped {queued} datagrams queued and {waiting} waiting from before, so the reconnect starts clean",
            gap.as_millis()
        );
    }

    /// One received datagram.
    fn recv_one(&self, call: &Call<'_>, buf: &Value) -> Recv {
        let waiting = call_b(call, self.api.receive, &[])
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !waiting {
            return Recv::Empty;
        }
        let size = call_b(call, self.api.size, &[])
            .and_then(|v| v.as_f64())
            .filter(|s| s.is_finite() && *s >= 1.0)
            .map_or(0, |s| s as usize);
        if size == 0 || size > MAX_DATAGRAM {
            return Recv::Skipped;
        }
        let _ = call_b(call, self.api.data, std::slice::from_ref(buf));
        let sender = events::id_of(call_b(call, self.api.sender, &[]).as_ref());
        let encoded = call_b(
            call,
            self.api.b64_encode,
            &[buf.clone(), 0.0.into(), len_value(size)],
        )
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
        match text::b64_decode(&encoded).filter(|b| b.len() == size) {
            Some(b) => Recv::Got(sender, b),
            None => Recv::Skipped,
        }
    }

    /// Move datagrams both ways. Returns `(sent, received, refused)`.
    fn move_packets(&self, call: &Call<'_>, receive: bool) -> (usize, usize, usize) {
        let out: Vec<_> = {
            let st = self.lock();
            std::iter::from_fn(|| st.bridge.pop_outbound())
                .take(pump::MAX_SEND_PER_TICK)
                .collect()
        };
        if out.is_empty() && !receive {
            return (0, 0, 0);
        }
        let Some((sbuf, rbuf)) = self.buffers(call) else {
            warn_once(
                "steam-buffers",
                "co-op steam: unable to create packet buffers",
            );
            return (0, 0, 0);
        };
        let (mut sent, mut failed) = (0, 0);
        let mut push_out = |to: u64, data: &[u8]| {
            if self.send_one(call, &sbuf, to, data) {
                sent += 1;
            } else {
                failed += 1;
            }
        };
        for d in &out {
            push_out(d.peer, &d.data);
        }
        let mut got = Vec::new();
        if receive {
            for _ in 0..pump::MAX_RECV_PER_TICK {
                match self.recv_one(call, &rbuf) {
                    Recv::Empty => break,
                    Recv::Skipped => {}
                    Recv::Got(from, bytes) => got.push((from, bytes)),
                }
            }
        }
        let received = got.len();
        if !got.is_empty() {
            let st = self.lock();
            for (sender, bytes) in got {
                let _ = st.bridge.deliver(sender, bytes);
            }
        }
        (sent, received, failed)
    }

    fn read_members(&self, call: &Call<'_>) -> Vec<u64> {
        let n = call_b(call, self.api.lobby_members, &[])
            .and_then(|v| v.as_f64())
            .map_or(0, |n| n.clamp(0.0, 8.0) as u32);
        (0..n)
            .map(|i| {
                events::id_of(call_b(call, self.api.lobby_member, &[f64::from(i).into()]).as_ref())
            })
            .filter(|&id| id != 0)
            .collect()
    }

    /// This player's own Steam name as plain text (never logged).
    fn own_name(&self, call: &Call<'_>) -> Option<String> {
        let me = events::id_of(call_b(call, self.api.my_id, &[]).as_ref());
        if me == 0 {
            return None;
        }
        let raw = call_b(call, self.api.persona, &[idv(me)])
            .and_then(|v| v.as_str().map(str::to_owned))?;
        Some(text::clean_persona(&raw)).filter(|n| n != "?")
    }

    /// Persona names for members not named yet (shown, never logged).
    fn name_members(&self, call: &Call<'_>, members: &[u64]) {
        let unnamed: Vec<u64> = {
            let st = self.lock();
            members
                .iter()
                .copied()
                .filter(|id| st.names.get(id).is_none_or(|n| n == "?"))
                .collect()
        };
        let named: Vec<(u64, String)> = unnamed
            .into_iter()
            .map(|id| {
                let raw = call_b(call, self.api.persona, &[idv(id)])
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                (id, text::clean_persona(&raw))
            })
            .collect();
        let mut st = self.lock();
        for (id, n) in named {
            st.names.insert(id, n);
        }
    }

    fn read_data(&self, call: &Call<'_>) -> LobbyData {
        let get = |k: &str| {
            call_b(call, self.api.lobby_get_data, &[k.into()])
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        LobbyData {
            ndml: get(KEY_MOD),
            ver: get(KEY_BUILD),
            state: get(KEY_STATE),
            files: get(KEY_FILES),
        }
    }

    fn read_list(&self, call: &Call<'_>) -> ListPoll {
        ListPoll {
            loading: call_b(call, self.api.list_loading, &[])
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            count: call_b(call, self.api.list_count, &[])
                .and_then(|v| v.as_f64())
                .map_or(0, |n| n.max(0.0) as u32),
        }
    }

    fn poll(&self, call: &Call<'_>, tick: u64) -> Vec<Cmd> {
        let now = Instant::now();
        let wants = self.lock().machine.wants();
        let mut cmds = Vec::new();
        let every = wants.poll_every;
        if every > 0 && tick.is_multiple_of(u64::from(every)) {
            let lobby = read_id(call, self.api.lobby_id);
            let owner = if lobby != 0 {
                read_id(call, self.api.lobby_owner)
            } else {
                0
            };
            let members = (lobby != 0).then(|| self.read_members(call));
            self.name_members(call, members.as_deref().unwrap_or_default());
            let mut st = self.lock();
            cmds.extend(st.machine.on_poll(
                &Poll {
                    lobby,
                    owner,
                    members,
                },
                now,
            ));
        }
        if wants.data {
            let d = self.read_data(call);
            cmds.extend(self.lock().machine.on_data(&d, now));
        }
        if wants.list {
            let l = self.read_list(call);
            cmds.extend(self.lock().machine.on_list(l, now));
        }
        cmds
    }

    fn process(&self, room: Option<i64>, tick: u64, active: bool, now: Instant) -> Vec<Cmd> {
        let events =
            std::mem::take(&mut *self.inbox.lock().unwrap_or_else(PoisonError::into_inner));
        let file_actions = self.net.take_steam_actions();
        let status = (!active || tick.is_multiple_of(STATUS_EVERY))
            .then(|| self.net.lobby_status())
            .flatten();
        let mut cmds = Vec::new();
        {
            let mut st = self.lock();
            if st.machine.page() == SteamPage::Idle
                && let Some(lobby) = st.launch_lobby.take()
            {
                log::info!("co-op steam: joining the lobby from the launch invite");
                cmds.extend(st.machine.join_request(lobby, now));
            }
            for ev in &events {
                cmds.extend(st.machine.on_event(ev, now));
            }
            for f in file_actions {
                if let Some(a) = file_action(&f, st.machine.visibility()) {
                    log::info!("co-op steam: peer.toml asks for {:?}", f.action);
                    st.actions.extend(a);
                }
            }
            while let Some(a) = st.actions.pop_front() {
                cmds.extend(st.machine.act(a, now));
            }
            if let Some(s) = &status {
                cmds.extend(st.machine.on_link(link_of(s), now));
            }
            let in_run = room.is_some() && room == st.assets.and_then(|a| a.room_game);
            cmds.extend(st.machine.set_in_run(in_run));
            cmds.extend(st.machine.tick(now));
            let busy = st.machine.active();
            let was = self.active.swap(busy, Ordering::Relaxed);
            if was && !busy {
                self.linger_until
                    .store(tick + pump::LINGER_TICKS, Ordering::Relaxed);
            }
        }
        if !active || tick.is_multiple_of(STATUS_EVERY) {
            self.publish_status(true);
        }
        cmds
    }

    fn execute(&self, call: &Call<'_>, mut cmds: Vec<Cmd>, now: Instant) {
        for _ in 0..3 {
            if cmds.is_empty() {
                return;
            }
            let mut more = Vec::new();
            for cmd in cmds {
                if !self.run_cmd(call, &cmd, now) {
                    more.extend(self.lock().machine.call_failed(&cmd));
                }
            }
            cmds = more;
        }
    }

    fn run_cmd(&self, call: &Call<'_>, cmd: &Cmd, _now: Instant) -> bool {
        let a = &self.api;
        let checked = |what: &str, r: Option<Value>| {
            let ok = ok_result(r.as_ref());
            if !ok {
                warn_once(
                    &format!("steam-call-{what}"),
                    &format!("co-op steam: {what} failed"),
                );
            }
            ok
        };
        match cmd {
            Cmd::Create { kind, max } => checked(
                sf::LOBBY_CREATE,
                call_b(
                    call,
                    a.lobby_create,
                    &[kind.gml().into(), f64::from(*max).into()],
                ),
            ),
            Cmd::SetType(kind) => checked(
                sf::LOBBY_SET_TYPE,
                call_b(call, a.lobby_set_type, &[kind.gml().into()]),
            ),
            Cmd::SetData(k, v) => checked(
                sf::LOBBY_SET_DATA,
                call_b(call, a.lobby_set_data, &[(*k).into(), v.as_str().into()]),
            ),
            Cmd::Join(id) => checked(sf::LOBBY_JOIN, call_b(call, a.lobby_join, &[idv(*id)])),
            Cmd::Leave => checked(sf::LOBBY_LEAVE, call_b(call, a.lobby_leave, &[])),
            Cmd::Invite => {
                // Asked again: a start-up "off" may be stale.
                if !self.overlay_now(call) {
                    self.lock().machine.tell(
                        "The Steam overlay is off: enable it in Steam, or use a room code.",
                        Instant::now(),
                    );
                    return true;
                }
                checked(sf::LOBBY_INVITE, call_b(call, a.lobby_invite, &[]))
            }
            Cmd::FindCode(code) => {
                let f = call_b(
                    call,
                    a.list_filter,
                    &[
                        super::lobby::KEY_CODE.into(),
                        code.as_str().into(),
                        names::LIST_EQUAL.into(),
                    ],
                );
                let _ = call_b(call, a.list_distance, &[names::LIST_WORLDWIDE.into()]);
                f.is_some() && call_b(call, a.list_request, &[]).is_some()
            }
            Cmd::JoinFound => checked(sf::LIST_JOIN, call_b(call, a.list_join, &[0.0.into()])),
            Cmd::SetOwner(id) => checked(
                sf::LOBBY_SET_OWNER,
                call_b(call, a.lobby_set_owner, &[idv(*id)]),
            ),
            Cmd::Net(n) => {
                // A Steam session goes by the player's own Steam name.
                if matches!(n, NetCmd::Host { .. } | NetCmd::Join { .. })
                    && let Some(name) = self.own_name(call)
                {
                    self.net.steam_request(SteamRequest::Name(name));
                }
                self.net.steam_request(match n {
                    NetCmd::Host { allow } => SteamRequest::Host {
                        allow: allow.clone(),
                    },
                    NetCmd::Join { host } => SteamRequest::Join { host: *host },
                    NetCmd::Leave => SteamRequest::Leave,
                });
                true
            }
        }
    }

    fn publish_status(&self, available: bool) {
        let status = {
            let st = self.lock();
            let view = compose_view(&st, available && self.available(), Instant::now());
            let s = status_of(&st, &view);
            if st.sent_status.as_ref() == Some(&s) {
                return;
            }
            s
        };
        self.net.set_steam_status(Some(status.clone()));
        self.lock().sent_status = Some(status);
    }
}

fn room_index(call: &Call<'_>) -> Option<i64> {
    let v = call.instance()?.builtin_value(names::VAR_ROOM).ok()?;
    index_of(&v)
}

/// A `peer.toml` Steam action as UI actions.
fn file_action(f: &SteamFileRequest, current: Visibility) -> Option<Vec<SteamAction>> {
    let host = |v: Visibility| {
        let mut a = Vec::new();
        if v != current {
            a.push(SteamAction::SetVisibility(v));
        }
        a.push(SteamAction::Host);
        a
    };
    Some(match f.action {
        SteamFileAction::Host => host(Visibility::Friends),
        SteamFileAction::HostInviteOnly => host(Visibility::InviteOnly),
        SteamFileAction::HostCode => host(Visibility::RoomCode),
        SteamFileAction::JoinCode => vec![SteamAction::JoinCode(f.code.clone()?)],
        SteamFileAction::Invite => vec![SteamAction::OpenInvite],
        SteamFileAction::Leave => vec![SteamAction::Leave],
        SteamFileAction::Retry => vec![SteamAction::Retry],
    })
}

/// The view, from the lobby flow and the name cache.
fn compose_view(st: &State, available: bool, now: Instant) -> SteamView {
    let m = &st.machine;
    let page = if available {
        m.page()
    } else {
        SteamPage::Unavailable
    };
    let link = m.link();
    let host = m.host();
    let name = |id: u64| st.names.get(&id).cloned().unwrap_or_else(|| "?".into());
    let mut members: Vec<MemberView> = m
        .members()
        .iter()
        .map(|&id| MemberView {
            name: name(id),
            me: id == st.me,
            host: Some(id) == host,
            linked: link.connected && id != st.me && (host == Some(st.me) || Some(id) == host),
        })
        .collect();
    members.sort_by_key(|v| !v.host);
    let partner = match m.role() {
        Some(SteamRole::Host) => members.iter().find(|v| !v.me).map(|v| v.name.clone()),
        Some(SteamRole::Join) => host.map(name),
        None => None,
    };
    let (refusal, error) = match m.outcome() {
        Some(Outcome::Refused(r)) => (Some(r.clone()), None),
        Some(Outcome::Error(e)) => (None, Some(e.clone())),
        _ => (None, None),
    };
    let idle = matches!(
        page,
        SteamPage::Idle | SteamPage::Error | SteamPage::Refused | SteamPage::Left
    );
    let in_lobby = matches!(
        page,
        SteamPage::Hosting
            | SteamPage::Connecting
            | SteamPage::Linked
            | SteamPage::InRun
            | SteamPage::PeerLost
            | SteamPage::InLobby
    );
    SteamView {
        available,
        page,
        role: m.role(),
        visibility: m.visibility(),
        members,
        max_members: st.machine_max(),
        partner,
        rtt_ms: link.rtt_ms.filter(|_| link.connected),
        lost_for_s: m
            .lost_since()
            .map(|t| now.saturating_duration_since(t).as_secs()),
        room_code: m.room_code().map(str::to_owned),
        refusal,
        error,
        notice: m.notice().map(str::to_owned),
        invites_arrive: st.listener.works(),
        offers: Offers {
            host: available && idle,
            // Offered even while the overlay reads off; the press rechecks.
            invite: available && in_lobby,
            join_code: available && (idle || page == SteamPage::Hosting),
            leave: available && !idle && page != SteamPage::Unavailable,
            retry: available
                && matches!(
                    page,
                    SteamPage::Error | SteamPage::Refused | SteamPage::Left
                ),
            visibility: available && (idle || page == SteamPage::Hosting),
        },
    }
}

impl State {
    fn machine_max(&self) -> u8 {
        self.machine.max_members()
    }
}

/// `[steam]` for `status.toml`: names only, no ids.
fn status_of(st: &State, v: &SteamView) -> SteamStatus {
    let (error_kind, error) = match (&v.refusal, &v.error) {
        (Some(r), _) => (Some(r.key().to_owned()), Some(r.message())),
        (None, Some(e)) => (Some(e.key().to_owned()), Some(e.message())),
        _ => match st.machine.outcome() {
            Some(Outcome::Left) => (Some("left".into()), Some(v.headline())),
            _ => (None, None),
        },
    };
    let owner = st.machine.owner();
    let host = st.machine.host();
    SteamStatus {
        available: v.available,
        lobby: st.machine.lobby_state(),
        page: v.page.key().into(),
        visibility: v.visibility.key().into(),
        max_members: v.max_members,
        room_code: v.room_code.clone(),
        listener: if st.listener.works() || st.listener.state() == ListenerState::Failed {
            st.listener.state()
        } else {
            ListenerState::Off
        },
        error_kind,
        error,
        members: st
            .machine
            .members()
            .iter()
            .map(|&id| SteamMember {
                name: st.names.get(&id).cloned().unwrap_or_else(|| "?".into()),
                me: id == st.me,
                owner: id == owner,
                host: Some(id) == host,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `false`, 0 and no answer fail; `true`, a positive number and
    /// `undefined` (calls that return nothing) succeed.
    #[test]
    fn a_steam_call_fails_on_false_or_no_answer() {
        assert!(!ok_result(None));
        assert!(!ok_result(Some(&Value::Bool(false))));
        assert!(!ok_result(Some(&Value::Real(0.0))));
        assert!(ok_result(Some(&Value::Bool(true))));
        assert!(ok_result(Some(&Value::Real(1.0))));
        assert!(ok_result(Some(&Value::Undefined)));
    }
}

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

//! The co-op UI's hooks: they read the game's values, write the swapped
//! strings and restore them in the same event, and run the display lists.
//!
//! A GML runtime error in a mod's call ends the process, so fonts and assets
//! are checked before use, scripts run as the hook's own instance, no lock is
//! held across a call into the game, and a missing name turns off only what
//! needs it, with one warning.

#![forbid(unsafe_code)]

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use ndml_sdk::{Arg, Builtin, Call, Instance, ModContext, Script, Value, Variable};

use super::layout::{self, MenuGeom, RunStyle};
use super::menu::{self, Route, Saved};
use super::page::{Effect, PageId, PageView, RowId};
use super::prefs;
use super::run::{LinkEvent, RunFacts, RunUi};
use super::style::{Font, Frame, Measure, Op, TextOp, color};
use super::text::{self, en};
use crate::Shared;
use crate::game::{self, call_b, num, truthy, warn_once};

/// `oMenu` instance variables.
#[derive(Debug, Clone)]
pub struct MenuVars {
    pub pos: Option<Variable>,
    pub min_pos: Option<Variable>,
    pub max_pos: Option<Variable>,
    pub alpha: Option<Variable>,
    pub new_game_check: Option<Variable>,
    pub new_game_check_position: Option<Variable>,
    pub pause_input: Option<Variable>,
    pub h_pos: Option<Variable>,
    pub v_pos: Option<Variable>,
    pub ll: Option<Variable>,
    pub box_h_pos: Option<Variable>,
    pub box_h_pos2: Option<Variable>,
    pub click_pos10: Option<Variable>,
    pub click_pos10b: Option<Variable>,
    pub mouse_active: Option<Variable>,
    pub warning_alpha: Option<Variable>,
    /// The black fade-in drawn over the menu while `global.menuFade` is set.
    pub black_alpha: Option<Variable>,
}

/// Input binding globals (`menuLeftKey`, `gamepadConfirm`, `player1`, ...).
#[derive(Debug, Clone)]
pub struct Keys {
    pub left: Option<Variable>,
    pub right: Option<Variable>,
    pub confirm: Option<Variable>,
    pub confirm2: Option<Variable>,
    pub back: Option<Variable>,
    pub player1: Option<Variable>,
    pub pad_left: Option<Variable>,
    pub pad_right: Option<Variable>,
    pub pad_confirm: Option<Variable>,
    pub pad_back: Option<Variable>,
    /// The pause key and pad button (polled on the held upgrade card).
    pub pause: Option<Variable>,
    pub pad_pause: Option<Variable>,
}

/// Builtins only the UI calls.
#[derive(Debug, Clone)]
pub struct UiBuiltins {
    pub set_font: Option<Builtin>,
    pub get_font: Option<Builtin>,
    pub font_exists: Option<Builtin>,
    pub kb_pressed: Option<Builtin>,
    pub kb_down: Option<Builtin>,
    pub mouse_x: Option<Builtin>,
    pub mouse_y: Option<Builtin>,
    pub point_in_rect: Option<Builtin>,
    pub alarm_get: Option<Builtin>,
    pub asset_get_index: Option<Builtin>,
    pub instance_exists: Option<Builtin>,
    pub sprite_get_height: Option<Builtin>,
    pub sprite_get_width: Option<Builtin>,
    pub deck_check: Option<Builtin>,
    pub deck_show: Option<Builtin>,
    pub deck_dismiss: Option<Builtin>,
    /// The pause menu's row places (`oController.hPos[i]`, `vPos[i]`).
    pub var_get: Option<Builtin>,
    pub array_get: Option<Builtin>,
    pub array_length: Option<Builtin>,
}

/// Everything the UI resolves at init.
#[derive(Debug)]
pub struct UiHandles {
    pub menu: MenuVars,
    pub rows: [Option<Variable>; menu::ROWS],
    pub resume_query: Option<Variable>,
    pub settings_back: Option<Variable>,
    pub pause_resume: Option<Variable>,
    pub pause_save_quit: Option<Variable>,
    pub pause_exit: Option<Variable>,
    pub pause_save_quit_query: Option<Variable>,
    pub pause_confirm_quit: Option<Variable>,
    pub fonts: [Option<Variable>; 8],
    pub gui_scale: Option<Variable>,
    pub language: Option<Variable>,
    pub time_alpha: Option<Variable>,
    /// `global.menuFade`: the menu fades in from black.
    pub menu_fade: Option<Variable>,
    pub keys: Keys,
    pub play: Option<Script>,
    pub text_box: Option<Script>,
    pub input_pressed: Option<Script>,
    pub pad_pressed: Option<Script>,
    pub b: UiBuiltins,
    assets: Mutex<BTreeMap<&'static str, Option<Value>>>,
    /// Draws in a row where each font global was not a font.
    font_misses: [AtomicU32; 8],
}

/// Draws without a font before the warning (a few seconds at 60 frames).
const FONT_WARN_DRAWS: u32 = 600;

/// Asset names looked up at runtime.
mod asset {
    pub const MOVE: &str = "cursorMove";
    pub const SELECT: &str = "cursorSelect";
    /// The hint bar `drawTextBox` draws.
    pub const TEXT_BOX: &str = "sTutorialBarWide";
    /// The backdrop Settings draws behind its items.
    pub const GRADIENT: &str = "sGUIGradient";
    pub const WAVE_ALERT: &str = "oWaveAlert";
    pub const MENU_CURSOR: &str = "oMenuCursor2";
}

impl UiHandles {
    /// Resolve every name (missing ones are logged and left `None`).
    #[must_use]
    pub fn resolve(ctx: &ModContext) -> Self {
        let v = |n| game::var(ctx, n);
        let b = |n| game::builtin(ctx, n);
        let sc = |n| game::script(ctx, n);
        Self {
            menu: MenuVars {
                pos: v("pos"),
                min_pos: v("minPos"),
                max_pos: v("maxPos"),
                alpha: v("alpha"),
                new_game_check: v("newGameCheck"),
                new_game_check_position: v("newGameCheckPosition"),
                pause_input: v("pauseInput"),
                h_pos: v("hPos"),
                v_pos: v("vPos"),
                ll: v("ll"),
                box_h_pos: v("boxHPos"),
                box_h_pos2: v("boxHPos2"),
                click_pos10: v("clickPos10"),
                click_pos10b: v("clickPos10b"),
                mouse_active: v("mouseActive"),
                warning_alpha: v("warningAlpha"),
                black_alpha: v("blackAlpha"),
            },
            rows: menu::ROW_GLOBALS.map(v),
            resume_query: v("strMenuResumeRunQuery"),
            settings_back: v("strSettingsBack"),
            pause_resume: v("strPauseMenuResumeGame"),
            pause_save_quit: v("strPauseMenuSaveAndQuit"),
            pause_exit: v("strPauseMenuExitGame"),
            pause_save_quit_query: v("strPauseMenuSaveAndQuitQuery"),
            pause_confirm_quit: v("strPauseMenuConfirmQuit"),
            fonts: Font::ALL.map(|f| v(f.global())),
            gui_scale: v("guiScale"),
            language: v("language"),
            time_alpha: v("timeAlpha"),
            menu_fade: v("menuFade"),
            keys: Keys {
                left: v("menuLeftKey"),
                right: v("menuRightKey"),
                confirm: v("confirmKey"),
                confirm2: v("confirmKey2"),
                back: v("backKey"),
                player1: v("player1"),
                pad_left: v("gamepadMenuLeft"),
                pad_right: v("gamepadMenuRight"),
                pad_confirm: v("gamepadConfirm"),
                pad_back: v("gamepadMenuBack"),
                pause: v("menuKey"),
                pad_pause: v("gamepadTogglePause"),
            },
            play: sc("play"),
            text_box: sc("drawTextBox"),
            input_pressed: sc("inputButtonPressed"),
            pad_pressed: sc("gamepadPressed"),
            b: UiBuiltins {
                set_font: b("draw_set_font"),
                get_font: b("draw_get_font"),
                font_exists: b("font_exists"),
                kb_pressed: b("keyboard_check_pressed"),
                kb_down: b("keyboard_check"),
                mouse_x: b("device_mouse_x_to_gui"),
                mouse_y: b("device_mouse_y_to_gui"),
                point_in_rect: b("point_in_rectangle"),
                alarm_get: b("alarm_get"),
                asset_get_index: b("asset_get_index"),
                instance_exists: b("instance_exists"),
                sprite_get_height: b("sprite_get_height"),
                sprite_get_width: b("sprite_get_width"),
                deck_check: b("steam_utils_is_steam_running_on_steam_deck"),
                deck_show: b("steam_show_floating_gamepad_text_input"),
                deck_dismiss: b("steam_dismiss_floating_gamepad_text_input"),
                var_get: b("variable_instance_get"),
                array_get: b("array_get"),
                array_length: b("array_length"),
            },
            assets: Mutex::new(BTreeMap::new()),
            font_misses: Default::default(),
        }
    }

    /// The names the Co-op row, the page and its questions need that are missing.
    #[must_use]
    pub fn missing_for_row(&self, h: &game::Handles) -> Vec<&'static str> {
        let m = &self.menu;
        let vars = [
            ("pos", m.pos),
            ("minPos", m.min_pos),
            ("maxPos", m.max_pos),
            ("hPos", m.h_pos),
            ("vPos", m.v_pos),
            ("ll", m.ll),
            ("newGameCheck", m.new_game_check),
            ("newGameCheckPosition", m.new_game_check_position),
            ("strMenuResumeRunQuery", self.resume_query),
        ];
        let rows = menu::ROW_GLOBALS.into_iter().zip(self.rows);
        let builtins = [
            ("draw_text", h.b.draw_text),
            ("keyboard_check_pressed", self.b.kb_pressed),
        ];
        let mut out = absent(vars.into_iter().chain(rows).map(|(n, v)| (n, v.is_some())));
        out.extend(absent(builtins.map(|(n, b)| (n, b.is_some()))));
        out
    }

    /// The five pause strings [`super::run::PauseText`] writes (all or none).
    fn pause_labels(&self) -> [(&'static str, Option<Variable>); 5] {
        [
            ("strPauseMenuResumeGame", self.pause_resume),
            ("strPauseMenuSaveAndQuit", self.pause_save_quit),
            ("strPauseMenuExitGame", self.pause_exit),
            ("strPauseMenuSaveAndQuitQuery", self.pause_save_quit_query),
            ("strPauseMenuConfirmQuit", self.pause_confirm_quit),
        ]
    }

    /// Count draws where `font`'s global is not a font; warn once past
    /// [`FONT_WARN_DRAWS`].
    fn font_seen(&self, font: Font, ok: bool) {
        let n = &self.font_misses[font.index()];
        if ok {
            n.store(0, Ordering::Relaxed);
        } else if streak_warns(n.fetch_add(1, Ordering::Relaxed), FONT_WARN_DRAWS) {
            let name = font.global();
            warn_once(
                &format!("ui-font-{name}"),
                &format!(
                    "co-op UI: {name} is not a font; co-op text is drawn in whatever font is set"
                ),
            );
        }
    }

    /// An asset by name (`asset_get_index`, cached; `None` if missing).
    fn asset(&self, call: &Call<'_>, name: &'static str) -> Option<Value> {
        let cached = self
            .assets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned();
        if let Some(v) = cached {
            return v;
        }
        let found = call_b(call, self.b.asset_get_index, &[name.into()]).filter(|v| match v {
            Value::Ref(_) => true,
            other => other.as_f64().is_some_and(|x| x >= 0.0),
        });
        if found.is_none() {
            warn_once(
                name,
                &format!("co-op UI: asset {name} not found; what needs it is skipped"),
            );
        }
        self.assets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name, found.clone());
        found
    }
}

fn play(call: &Call<'_>, s: &Shared, sound: &'static str) {
    let (Some(script), Some(snd)) = (s.ui.play, s.ui.asset(call, sound)) else {
        return;
    };
    if let Err(e) = call.call_script(script, &[snd.clone(), snd]) {
        warn_once("ui-play", &format!("co-op UI: play: {e}"));
    }
}

/// The game's menu move sound (a locked row: nothing changes).
pub(crate) fn play_move(call: &Call<'_>, s: &Shared) {
    play(call, s, asset::MOVE);
}

/// The game's menu select sound.
pub(crate) fn play_select(call: &Call<'_>, s: &Shared) {
    play(call, s, asset::SELECT);
}

fn global_num(call: &Call<'_>, v: Option<Variable>) -> Option<f64> {
    num(&call.global()?, v)
}

fn global_value(call: &Call<'_>, v: Option<Variable>) -> Option<Value> {
    call.global()?.get_value(v?)
}

fn key_pressed(call: &Call<'_>, s: &Shared, key: Option<Variable>) -> bool {
    let (Some(script), Some(code)) = (s.ui.input_pressed, global_value(call, key)) else {
        return false;
    };
    call.call_script(script, &[code])
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn pad_pressed(call: &Call<'_>, s: &Shared, button: Option<Variable>) -> bool {
    let k = &s.ui.keys;
    let (Some(script), Some(dev), Some(btn)) = (
        s.ui.pad_pressed,
        global_value(call, k.player1),
        global_value(call, button),
    ) else {
        return false;
    };
    call.call_script(script, &[dev, btn])
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn kb(call: &Call<'_>, b: Option<Builtin>, vk: u32) -> bool {
    call_b(call, b, &[f64::from(vk).into()])
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

mod vk {
    pub const BACK: u32 = 0x08;
    pub const ENTER: u32 = 0x0D;
    pub const CTRL: u32 = 0x11;
    pub const ESC: u32 = 0x1B;
    pub const DELETE: u32 = 0x2E;
    pub const V: u32 = 0x56;
}

fn back_pressed(call: &Call<'_>, s: &Shared) -> bool {
    key_pressed(call, s, s.ui.keys.back)
        || pad_pressed(call, s, s.ui.keys.pad_back)
        || kb(call, s.ui.b.kb_pressed, vk::ESC)
}

fn left_right(call: &Call<'_>, s: &Shared) -> i32 {
    let k = &s.ui.keys;
    if key_pressed(call, s, k.left) || pad_pressed(call, s, k.pad_left) {
        -1
    } else {
        i32::from(key_pressed(call, s, k.right) || pad_pressed(call, s, k.pad_right))
    }
}

fn set(me: &Instance<'_>, v: Option<Variable>, x: f64) {
    if let Some(v) = v
        && !me.set_f64(v, x)
    {
        warn_once(
            "ui-set",
            "co-op UI: an oMenu variable is not a number; left alone",
        );
    }
}

fn mouse(call: &Call<'_>, s: &Shared) -> Option<(f64, f64)> {
    let x = call_b(call, s.ui.b.mouse_x, &[0.0.into()])?.as_f64()?;
    let y = call_b(call, s.ui.b.mouse_y, &[0.0.into()])?.as_f64()?;
    Some((x, y))
}

/// Hide the menu's beam (during text entry).
fn hide_cursor(call: &Call<'_>, s: &Shared) {
    if let Some(obj) = s.ui.asset(call, asset::MENU_CURSOR) {
        let _ = call.with(&obj, |inst, _| {
            let _ = inst.set_builtin_f64("image_alpha", 0.0);
        });
    }
}

fn language(call: &Call<'_>, s: &Shared) -> String {
    global_value(call, s.ui.language)
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "english".into())
}

fn frame_of(call: &Call<'_>, s: &Shared) -> Option<Frame> {
    let gui = game::gui_size(call, &s.h.b)?;
    Some(Frame::new(gui.w, gui.h, global_num(call, s.ui.gui_scale)))
}

fn deck_keyboard(call: &Call<'_>, s: &Shared, show: bool, frame: Option<Frame>) {
    if !s.lock().settings.ui.deck_keyboard {
        return;
    }
    let b = &s.ui.b;
    let available = |x: Option<Builtin>| x.is_some_and(|x| call.builtin_available(x) == Ok(true));
    if !available(b.deck_check)
        || !call_b(call, b.deck_check, &[])
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        return;
    }
    if show && available(b.deck_show) {
        let f = frame.unwrap_or(Frame::new(1280.0, 800.0, None));
        let _ = call_b(
            call,
            b.deck_show,
            &[
                0.0.into(),
                0.0.into(),
                0.0.into(),
                f.w.into(),
                (f.h / 2.0).into(),
            ],
        );
    } else if !show && available(b.deck_dismiss) {
        let _ = call_b(call, b.deck_dismiss, &[]);
    }
}

/// Draws [`Op`]s with the game's fonts, measures text, restores the draw state.
struct Painter<'c, 'a> {
    call: &'c Call<'a>,
    s: &'c Shared,
    fonts: [Option<Value>; 8],
    saved: [Option<Value>; 5],
    widths: RefCell<BTreeMap<(Font, String), f64>>,
    lines: RefCell<[Option<f64>; 8]>,
    text_box_h: OnceCell<Option<f64>>,
    current: RefCell<Option<Font>>,
}

impl<'c, 'a> Painter<'c, 'a> {
    fn new(call: &'c Call<'a>, s: &'c Shared) -> Self {
        let b = &s.ui.b;
        // A font global missing or not a font: drawn in whatever font is set.
        let fonts = Font::ALL.map(|font| {
            let f = global_value(call, s.ui.fonts[font.index()])
                .filter(|f| !matches!(f, Value::Undefined | Value::Str(_) | Value::Other { .. }))
                .filter(|f| {
                    call_b(call, b.font_exists, std::slice::from_ref(f))
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false)
                });
            s.ui.font_seen(font, f.is_some());
            f
        });
        let hb = &s.h.b;
        let saved = [
            call_b(call, b.get_font, &[]),
            call_b(call, hb.draw_get_color, &[]),
            call_b(call, hb.draw_get_alpha, &[]),
            call_b(call, hb.draw_get_halign, &[]),
            call_b(call, hb.draw_get_valign, &[]),
        ];
        Self {
            call,
            s,
            fonts,
            saved,
            widths: RefCell::new(BTreeMap::new()),
            lines: RefCell::new([None; 8]),
            text_box_h: OnceCell::new(),
            current: RefCell::new(None),
        }
    }

    fn use_font(&self, f: Font) {
        if *self.current.borrow() == Some(f) {
            return;
        }
        if let Some(h) = &self.fonts[f.index()] {
            let _ = call_b(self.call, self.s.ui.b.set_font, std::slice::from_ref(h));
        }
        *self.current.borrow_mut() = Some(f);
    }

    fn finish(self) {
        let b = &self.s.ui.b;
        let hb = &self.s.h.b;
        let [font, col, alpha, ha, va] = self.saved;
        if self.current.borrow().is_some()
            && let Some(f) = font.filter(|f| {
                call_b(self.call, b.font_exists, std::slice::from_ref(f))
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false)
            })
        {
            let _ = call_b(self.call, b.set_font, &[f]);
        }
        for (setter, v) in [
            (hb.draw_set_color, col),
            (hb.draw_set_alpha, alpha),
            (hb.draw_set_halign, ha),
            (hb.draw_set_valign, va),
        ] {
            if let Some(v) = v.filter(|v| v.as_f64().is_some()) {
                let _ = call_b(self.call, setter, &[v]);
            }
        }
    }

    fn state(&self, c: u32, a: f64, h: f64, v: f64) {
        let hb = &self.s.h.b;
        let _ = call_b(self.call, hb.draw_set_color, &[f64::from(c).into()]);
        let _ = call_b(self.call, hb.draw_set_alpha, &[a.clamp(0.0, 1.0).into()]);
        let _ = call_b(self.call, hb.draw_set_halign, &[h.into()]);
        let _ = call_b(self.call, hb.draw_set_valign, &[v.into()]);
    }

    fn text(&self, t: &TextOp) {
        self.use_font(t.font);
        self.state(t.color, t.alpha, t.h.gml(), t.v.gml());
        let hb = &self.s.h.b;
        if (t.scale - 1.0).abs() < 1e-6 {
            let _ = call_b(
                self.call,
                hb.draw_text,
                &[t.x.into(), t.y.into(), t.text.as_str().into()],
            );
        } else {
            let _ = call_b(
                self.call,
                hb.draw_text_transformed,
                &[
                    t.x.into(),
                    t.y.into(),
                    t.text.as_str().into(),
                    t.scale.into(),
                    t.scale.into(),
                    0.0.into(),
                ],
            );
        }
    }

    fn rect(&self, (x1, y1, x2, y2): (f64, f64, f64, f64), c: u32, a: f64, outline: bool) {
        self.state(c, a, 0.0, 0.0);
        let _ = call_b(
            self.call,
            self.s.h.b.draw_rectangle,
            &[x1.into(), y1.into(), x2.into(), y2.into(), outline.into()],
        );
    }

    fn draw(&self, ops: &[Op], f: Frame) {
        for op in ops {
            match op {
                Op::Text(t) => self.text(t),
                Op::TextBox { x, y, text, alpha } => {
                    // drawTextBox sizes the bar with the current font.
                    self.use_font(Font::Small);
                    self.state(color::WHITE, 1.0, 0.0, 0.0);
                    if let Some(script) = self.s.ui.text_box
                        && let Err(e) = self.call.call_script(
                            script,
                            &[
                                (*x).into(),
                                (*y).into(),
                                text.as_str().into(),
                                (*alpha).into(),
                            ],
                        )
                    {
                        warn_once("ui-textbox", &format!("co-op UI: drawTextBox: {e}"));
                    }
                }
                Op::Rect {
                    x1,
                    y1,
                    x2,
                    y2,
                    color,
                    alpha,
                    outline,
                } => self.rect((*x1, *y1, *x2, *y2), *color, *alpha, *outline),
                Op::Dim { alpha } => self.rect((0.0, 0.0, f.w, f.h), color::BLACK, *alpha, false),
                Op::Triangle { pts, color, alpha } => {
                    self.state(*color, *alpha, 0.0, 0.0);
                    let [(ax, ay), (bx, by), (cx, cy)] = *pts;
                    let _ = call_b(
                        self.call,
                        self.s.h.b.draw_triangle,
                        &[
                            ax.into(),
                            ay.into(),
                            bx.into(),
                            by.into(),
                            cx.into(),
                            cy.into(),
                            false.into(),
                        ],
                    );
                }
                Op::Sprite(ship) => {
                    self.state(color::WHITE, 1.0, 0.0, 0.0);
                    let _ = game::draw_sprite(self.call, &self.s.h.b, ship);
                }
                Op::Backdrop { y, w, h, alpha } => self.backdrop(*y, *w, *h, *alpha),
            }
        }
    }

    fn backdrop(&self, y: f64, w: f64, h: f64, a: f64) {
        let Some(spr) = self.s.ui.asset(self.call, asset::GRADIENT) else {
            return;
        };
        let exists = call_b(
            self.call,
            self.s.h.b.sprite_exists,
            std::slice::from_ref(&spr),
        )
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
        if !exists {
            return;
        }
        let size = |b| {
            call_b(self.call, b, std::slice::from_ref(&spr))
                .and_then(|v| v.as_f64())
                .filter(|v| v.is_finite() && *v > 0.0)
        };
        let (Some(sw), Some(sh)) = (
            size(self.s.ui.b.sprite_get_width),
            size(self.s.ui.b.sprite_get_height),
        ) else {
            return;
        };
        self.state(color::WHITE, 1.0, 0.0, 0.0);
        let _ = call_b(
            self.call,
            self.s.h.b.draw_sprite_ext,
            &[
                spr,
                0.0.into(),
                0.0.into(),
                y.into(),
                (w / sw).into(),
                (h / sh).into(),
                0.0.into(),
                f64::from(color::WHITE).into(),
                a.clamp(0.0, 1.0).into(),
            ],
        );
    }
}

impl Measure for Painter<'_, '_> {
    fn width(&self, font: Font, text: &str) -> f64 {
        if let Some(w) = self.widths.borrow().get(&(font, text.to_owned())) {
            return *w;
        }
        self.use_font(font);
        let w = call_b(self.call, self.s.h.b.string_width, &[text.into()])
            .and_then(|v| v.as_f64())
            .filter(|w| w.is_finite() && *w >= 0.0)
            .unwrap_or_else(|| {
                f64::from(u32::try_from(text.chars().count()).unwrap_or(u32::MAX))
                    * self.line(font)
                    * 0.5
            });
        self.widths.borrow_mut().insert((font, text.to_owned()), w);
        w
    }

    fn line(&self, font: Font) -> f64 {
        if let Some(h) = self.lines.borrow()[font.index()] {
            return h;
        }
        self.use_font(font);
        let h = call_b(self.call, self.s.h.b.string_height, &["M".into()])
            .and_then(|v| v.as_f64())
            .filter(|h| h.is_finite() && *h > 2.0 && *h < 600.0)
            .unwrap_or(20.0);
        self.lines.borrow_mut()[font.index()] = Some(h);
        h
    }

    fn text_box_sprite_height(&self) -> Option<f64> {
        *self.text_box_h.get_or_init(|| {
            let spr = self.s.ui.asset(self.call, asset::TEXT_BOX)?;
            let exists = call_b(
                self.call,
                self.s.h.b.sprite_exists,
                std::slice::from_ref(&spr),
            )
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
            if !exists {
                return None;
            }
            let h = call_b(self.call, self.s.ui.b.sprite_get_height, &[spr])
                .and_then(|v| v.as_f64())
                .filter(|h| h.is_finite() && *h > 0.0 && *h < 4096.0)?;
            game::info_once(
                "ui-textbox-h",
                &format!("co-op UI: {} is {h} px tall", asset::TEXT_BOX),
            );
            Some(h)
        })
    }
}

/// What the glue remembers about the main menu.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent facts about the menu
pub struct MenuState {
    /// Page mode is on: the values to put back.
    pub saved: Option<Saved>,
    /// Rows on the page shown.
    pub rows: usize,
    /// Which rows (to keep the selection on its row when they change).
    pub ids: Vec<RowId>,
    /// The last valid selection on the page.
    pub last_pos: f64,
    /// The Co-op row was drawn this frame.
    pub coop_row: bool,
    /// What the mod draws at row 11 (the old Exit).
    pub row11: String,
    /// `maxPos` is raised on this `oMenu` (it follows the row).
    pub raised: bool,
    /// The game's own `maxPos` on this `oMenu` (`None`: no Co-op row here).
    pub game_max: Option<f64>,
    /// Frames in a row whose label swap failed.
    pub failed_frames: u32,
    /// Every piece the Co-op row needs is in this game (set at start-up).
    pub row_ok: bool,
    /// The page's question is in the game's `newGameCheck` dialog.
    pub native_confirm: bool,
    /// The typed text feed.
    pub feed: text::TextFeed,
    pub frames: u64,
    pub started: Option<Instant>,
}

/// The `Back` row in the game's own words.
fn localize(view: &mut PageView, back: Option<&str>) {
    if let Some(b) = back.filter(|b| !b.is_empty()) {
        for r in &mut view.rows {
            if r.id == RowId::Back {
                b.clone_into(&mut r.label);
            }
        }
    }
}

fn back_label(call: &Call<'_>, s: &Shared) -> Option<String> {
    global_value(call, s.ui.settings_back).and_then(|v| v.as_str().map(str::to_owned))
}

/// Enter page mode on this `oMenu`.
fn open_page(me: &Instance<'_>, s: &Shared, page: PageId) {
    let m = &s.ui.menu;
    let saved = Saved {
        pos: num(me, m.pos).unwrap_or(0.0),
        min_pos: num(me, m.min_pos).unwrap_or(0.0),
        max_pos: num(me, m.max_pos).unwrap_or(10.0),
    };
    let ids = page_rows(s, page);
    let (lo, hi) = menu::page_limits(ids.len());
    set(me, m.min_pos, lo);
    set(me, m.max_pos, hi);
    set(me, m.pos, 1.0);
    set(me, m.alpha, 0.0);
    let mut c = s.lock();
    if c.menu.saved.is_none() {
        c.menu.saved = Some(saved);
    }
    c.menu.rows = ids.len();
    c.menu.ids = ids;
    c.menu.last_pos = 1.0;
    c.page.show(page);
    log::info!("co-op UI: page {page:?} open");
}

/// The rows `page` has for the Steam state now (co-op lock, then Steam's).
fn page_rows(s: &Shared, page: PageId) -> Vec<RowId> {
    let c = s.lock();
    let steam = c.steam.view();
    page.rows(&steam)
}

/// While the page is open: follow a change of its rows (a Steam row came or went).
fn sync_rows(me: &Instance<'_>, s: &Shared) {
    let (old, new) = {
        let c = s.lock();
        let Some(page) = c.page.open else { return };
        let steam = c.steam.view();
        let new = page.rows(&steam);
        if new == c.menu.ids {
            return;
        }
        (c.menu.ids.clone(), new)
    };
    let m = &s.ui.menu;
    let pos = num(me, m.pos).unwrap_or(1.0);
    let next = menu::follow_row(&old, &new, pos);
    let (lo, hi) = menu::page_limits(new.len());
    set(me, m.min_pos, lo);
    set(me, m.max_pos, hi);
    set(me, m.pos, next);
    let mut c = s.lock();
    c.menu.rows = new.len();
    c.menu.ids = new;
    c.menu.last_pos = next;
}

/// Leave page mode: the menu's own values back, the selection on Co-op.
fn close_page(me: &Instance<'_>, s: &Shared) {
    let m = &s.ui.menu;
    let (saved, native) = {
        let mut c = s.lock();
        c.page.close();
        let native = std::mem::take(&mut c.menu.native_confirm);
        (c.menu.saved.take(), native)
    };
    if native {
        set(me, m.new_game_check, 0.0);
    }
    if let Some(sv) = saved {
        set(me, m.min_pos, sv.min_pos);
        set(me, m.max_pos, sv.max_pos);
        set(me, m.pos, sv.pos);
    }
    log::info!("co-op UI: page closed");
}

fn switch_page(me: &Instance<'_>, s: &Shared, page: PageId) {
    let ids = page_rows(s, page);
    let (lo, hi) = menu::page_limits(ids.len());
    let m = &s.ui.menu;
    set(me, m.min_pos, lo);
    set(me, m.max_pos, hi);
    set(me, m.pos, 1.0);
    let mut c = s.lock();
    c.menu.rows = ids.len();
    c.menu.ids = ids;
    c.menu.last_pos = 1.0;
}

fn persist(s: &Shared, changes: &[(&str, String)]) {
    let path: PathBuf = s.lock().config_path.clone();
    if let Err(e) = prefs::save(&path, crate::NDML_DEFAULT_CONFIG, changes) {
        warn_once(
            "ui-prefs",
            &format!("co-op UI: unable to save the page's choices ({e})"),
        );
    }
}

/// Carry out the page's effects (no lock held across game calls).
fn apply(call: &Call<'_>, s: &Shared, me: &Instance<'_>, effects: Vec<Effect>) {
    for e in effects {
        match e {
            Effect::Closed => close_page(me, s),
            Effect::Switched(p) => {
                if p == PageId::Lan {
                    // The code needs the public address, looked up from now on.
                    log::info!("co-op page: Codes and LAN opened");
                    s.lock().net.lobby_discover();
                }
                switch_page(me, s, p);
            }
            Effect::Lobby(r) => {
                log::info!(
                    "co-op page: {:?}{}",
                    r.role,
                    if r.disconnect { " (disconnect)" } else { "" }
                );
                s.lock().net.lobby_request(r);
            }
            Effect::Copy(t) => {
                let _ = call_b(call, s.h.b.clipboard_set, &[t.into()]);
            }
            Effect::Paste => {
                let clip = clipboard(call, s);
                let mut c = s.lock();
                let status = c.net.lobby_status();
                c.page.paste(clip.as_deref(), status.as_ref());
            }
            Effect::Steam(a) => {
                let mut c = s.lock();
                let r = c.steam.act(a);
                c.page.steam_result(r);
            }
            Effect::SetColor(ch) => {
                s.lock().colors.set_mine(ch.value);
                persist(s, &[("color", prefs::toml_str(&ch.key))]);
            }
            Effect::SetLabels(mode) => {
                s.lock().labels = mode;
                persist(
                    s,
                    &[
                        ("show_names", mode.names().to_string()),
                        ("show_marker", mode.marker().to_string()),
                    ],
                );
            }
        }
    }
}

fn clipboard(call: &Call<'_>, s: &Shared) -> Option<String> {
    let has = call_b(call, s.h.b.clipboard_has, &[])
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !has {
        return None;
    }
    call_b(call, s.h.b.clipboard_get, &[]).and_then(|v| v.as_str().map(str::to_owned))
}

/// After the page answered: open the YES/NO dialog or the text entry if new.
fn after_page_change(call: &Call<'_>, s: &Shared, me: &Instance<'_>, entry_before: bool) {
    let (question, entry_now, native) = {
        let c = s.lock();
        (
            c.page.confirm().is_some(),
            c.page.entry().is_some(),
            c.menu.native_confirm,
        )
    };
    if question && !native {
        let m = &s.ui.menu;
        set(me, m.new_game_check, 1.0);
        set(me, m.new_game_check_position, 1.0);
        s.lock().menu.native_confirm = true;
    }
    if entry_now && !entry_before {
        let typed = typed_text(me);
        s.lock().menu.feed.sync(&typed);
        deck_keyboard(call, s, true, frame_of(call, s));
    }
}

fn typed_text(me: &Instance<'_>) -> String {
    me.builtin_value(crate::names::builtin_var::KEYBOARD_STRING)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// `oMenu_Create_0` after: page mode off; the game's own `maxPos` noted.
pub(crate) fn menu_create_after(call: &mut Call<'_>, s: &Shared) {
    let Some(me) = call.instance() else { return };
    let row_ok = {
        let mut c = s.lock();
        c.page.close();
        c.menu.saved = None;
        c.menu.native_confirm = false;
        c.menu.coop_row = false;
        c.menu.raised = false;
        c.menu.game_max = None;
        c.menu.failed_frames = 0;
        c.menu.row_ok
    };
    if !row_ok {
        return; // said once at start-up
    }
    match row_max(num(&me, s.ui.menu.max_pos)) {
        Ok(max) => s.lock().menu.game_max = Some(max),
        Err(what) => {
            warn_once(
                "ui-maxpos",
                &format!(
                    "co-op UI: no Co-op menu row: the main menu's maxPos {what}; co-op needs \
                     a mod update for this game version"
                ),
            );
        }
    }
}

/// The names of the pieces that are not there (`false`), in order.
fn absent(pieces: impl IntoIterator<Item = (&'static str, bool)>) -> Vec<&'static str> {
    pieces.into_iter().filter(|p| !p.1).map(|p| p.0).collect()
}

/// The game's own `maxPos` for the Co-op row (10 or more), or what is wrong.
fn row_max(max: Option<f64>) -> Result<f64, String> {
    match max {
        Some(m) if m >= 10.0 => Ok(m),
        Some(m) => Err(format!("is {m}, not 10")),
        None => Err("cannot be read".into()),
    }
}

/// True exactly when a failure streak of `before` reaches `limit` with this one.
fn streak_warns(before: u32, limit: u32) -> bool {
    before.saturating_add(1) == limit
}

/// At start-up: no Co-op row if a menu hook (`missing`) or a piece of the game
/// the row needs is absent.
pub(crate) fn set_row_pieces(s: &Shared, mut missing: Vec<&str>) {
    missing.extend(s.ui.missing_for_row(&s.h));
    if !missing.is_empty() {
        log::warn!(
            "co-op UI: no Co-op menu row: this game has no {}; co-op needs a mod update \
             for this game version",
            missing.join(", ")
        );
    }
    s.lock().menu.row_ok = missing.is_empty();
}

/// At start-up: each in-run piece this game lacks is turned off with one warning.
pub(crate) fn set_run_pieces(s: &Shared) {
    let u = &s.ui;
    let lacking = |what: &str, pieces: &[(&'static str, bool)]| {
        let names = absent(pieces.iter().copied());
        if !names.is_empty() {
            log::warn!(
                "co-op UI: {what}: this game has no {}; co-op needs a mod update for this \
                 game version",
                names.join(", ")
            );
        }
    };
    lacking(
        "no hint bars in co-op runs or on the new game screen",
        &[("drawTextBox", u.text_box.is_some())],
    );
    lacking(
        "the pause menu keeps its own labels in co-op runs",
        &u.pause_labels().map(|(n, v)| (n, v.is_some())),
    );
}

/// From the menu tick (`oMenu_Step_0`, as `oMenu`): the page's refresh.
pub(crate) fn menu_tick(call: &Call<'_>, s: &Shared) {
    let Some(me) = call.instance() else { return };
    let lang = language(call, s);
    {
        let mut c = s.lock();
        let status = c.net.lobby_status();
        c.page.refresh(status.as_ref(), &lang);
        c.lang.clone_from(&lang);
        c.menu.frames += 1;
        c.menu.started.get_or_insert_with(Instant::now);
    }
    sync_rows(&me, s);
}

#[allow(clippy::struct_excessive_bools)] // a copy of independent flags
struct MenuSnap {
    open: bool,
    entry: bool,
    native_confirm: bool,
    coop_row: bool,
    raised: bool,
    rows: usize,
}

fn snap(s: &Shared) -> MenuSnap {
    let c = s.lock();
    MenuSnap {
        open: c.page.open.is_some(),
        entry: c.page.entry().is_some(),
        native_confirm: c.menu.native_confirm,
        coop_row: c.menu.coop_row,
        raised: c.menu.raised,
        rows: c.menu.rows,
    }
}

/// `oMenu_Step_2` before: where the menu's input lives.
pub(crate) fn menu_step2_before(call: &mut Call<'_>, s: &Shared) {
    let Some(me) = call.instance() else { return };
    let sn = snap(s);
    if !sn.open {
        if sn.coop_row && sn.raised {
            hover_row11(call, s, &me);
        }
        return;
    }
    if sn.entry {
        call.skip_original();
        hide_cursor(call, s);
        entry_input(call, s, &me);
        return;
    }
    if sn.native_confirm {
        // The game runs the dialog (left/right, mouse, Esc); Esc clears it.
        if num(&me, s.ui.menu.new_game_check) == Some(0.0) {
            let mut c = s.lock();
            c.menu.native_confirm = false;
            let _ = c.page.back();
        }
        return;
    }
    // The page's own fade-in.
    if let Some(a) = num(&me, s.ui.menu.alpha).filter(|a| *a < 1.0) {
        set(&me, s.ui.menu.alpha, (a + 0.1).min(1.0));
    }
    if back_pressed(call, s) {
        let effects = s.lock().page.back();
        play(call, s, asset::SELECT);
        apply(call, s, &me, effects);
        return;
    }
    let dir = left_right(call, s);
    if dir != 0 {
        let pos = num(&me, s.ui.menu.pos).unwrap_or(1.0);
        shift(call, s, &me, pos, dir, sn.rows);
    }
}

fn shift(call: &Call<'_>, s: &Shared, me: &Instance<'_>, pos: f64, dir: i32, rows: usize) {
    let Some(i) = menu::row_index(pos, rows) else {
        return;
    };
    let effects = {
        let mut c = s.lock();
        let steam = c.steam.view();
        if !c.page.row_shifts(i, &steam) {
            return;
        }
        let status = c.net.lobby_status();
        c.page.shift_row(i, dir, &steam, status.as_ref())
    };
    play(call, s, asset::MOVE);
    apply(call, s, me, effects);
}

fn hover_row11(call: &Call<'_>, s: &Shared, me: &Instance<'_>) {
    let m = &s.ui.menu;
    if !truthy(me, m.mouse_active).unwrap_or(false)
        || truthy(me, m.new_game_check).unwrap_or(false)
        || num(me, m.pos) == Some(11.0)
    {
        return;
    }
    let (Some(x1), Some(x2), Some(y1), Some(y2), Some(ll)) = (
        num(me, m.box_h_pos),
        num(me, m.box_h_pos2),
        num(me, m.click_pos10),
        num(me, m.click_pos10b),
        num(me, m.ll),
    ) else {
        warn_once(
            "ui-hover",
            "co-op UI: the menu's hit boxes are unreadable; row 11 has no mouse hover",
        );
        return;
    };
    let Some((mx, my)) = mouse(call, s) else {
        return;
    };
    let inside = call_b(
        call,
        s.ui.b.point_in_rect,
        &[
            mx.into(),
            my.into(),
            x1.into(),
            (y1 + ll).into(),
            x2.into(),
            (y2 + ll).into(),
        ],
    )
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
    if inside {
        set(me, m.pos, 11.0);
        play(call, s, asset::MOVE);
    }
}

fn entry_input(call: &Call<'_>, s: &Shared, me: &Instance<'_>) {
    let b = &s.ui.b;
    let typed = typed_text(me);
    let enter = kb(call, b.kb_pressed, vk::ENTER) || pad_pressed(call, s, s.ui.keys.pad_confirm);
    let esc = kb(call, b.kb_pressed, vk::ESC) || pad_pressed(call, s, s.ui.keys.pad_back);
    let backspace = kb(call, b.kb_pressed, vk::BACK);
    let delete = kb(call, b.kb_pressed, vk::DELETE);
    let paste = kb(call, b.kb_down, vk::CTRL) && kb(call, b.kb_pressed, vk::V);
    let clip = if paste { clipboard(call, s) } else { None };
    let (effects, closed) = {
        let mut c = s.lock();
        let mut added = c.menu.feed.feed(&typed);
        if paste && added.ends_with(['v', 'V']) {
            added.pop();
        }
        c.page.entry_text(&added);
        if backspace {
            c.page.entry_backspace();
        }
        if delete {
            c.page.entry_clear();
        }
        if paste {
            let status = c.net.lobby_status();
            c.page.paste(clip.as_deref(), status.as_ref());
        }
        let mut effects = Vec::new();
        if esc {
            c.page.entry_cancel();
        } else if enter {
            effects = c.page.entry_done();
        }
        (effects, c.page.entry().is_none())
    };
    if closed {
        // Hand the input back without the key that ended the entry.
        let _ = call_b(call, s.h.b.io_clear, &[]);
        deck_keyboard(call, s, false, None);
        play(call, s, asset::SELECT);
        apply(call, s, me, effects);
    }
}

fn confirm_row(call: &Call<'_>, s: &Shared, me: &Instance<'_>, pos: f64) {
    let (effects, entry_before) = {
        let mut c = s.lock();
        let rows = c.menu.rows;
        let entry_before = c.page.entry().is_some();
        let Some(i) = menu::row_index(pos, rows) else {
            return;
        };
        let status = c.net.lobby_status();
        let steam = c.steam.view();
        (c.page.confirm_row(i, &steam, status.as_ref()), entry_before)
    };
    play(call, s, asset::SELECT);
    apply(call, s, me, effects);
    after_page_change(call, s, me, entry_before);
}

/// `oMenu_Step_2` after: keep the selection on the page's rows.
pub(crate) fn menu_step2_after(call: &mut Call<'_>, s: &Shared) {
    let sn = snap(s);
    if !sn.open || sn.entry {
        return;
    }
    let Some(me) = call.instance() else { return };
    let Some(pos) = num(&me, s.ui.menu.pos) else {
        return;
    };
    let last = s.lock().menu.last_pos;
    let p = menu::clamp_pos(pos, sn.rows, last);
    if (p - pos).abs() > f64::EPSILON {
        set(&me, s.ui.menu.pos, p);
    }
    s.lock().menu.last_pos = p;
}

/// The game's guards at the top of `confirmMenu` (as `oMenu`).
fn guards_ok(call: &Call<'_>, s: &Shared, me: &Instance<'_>) -> bool {
    let alarm_off = |n: f64| {
        call_b(call, s.ui.b.alarm_get, &[n.into()])
            .and_then(|v| v.as_f64())
            .is_none_or(|a| a < 0.0)
    };
    alarm_off(1.0) && alarm_off(2.0) && num(me, s.ui.menu.pause_input).is_none_or(|p| p <= 0.0)
}

/// `confirmMenu` before. Returns the `pos` to restore after a shifted row.
pub(crate) fn confirm_menu_before(call: &mut Call<'_>, s: &Shared) -> Option<f64> {
    let me = call.instance()?;
    let pos = num(&me, s.ui.menu.pos)?;
    let sn = snap(s);
    if sn.open {
        call.skip_original();
        if !guards_ok(call, s, &me) {
            return None;
        }
        set(&me, s.ui.menu.pause_input, 7.0);
        if sn.native_confirm {
            let yes = num(&me, s.ui.menu.new_game_check_position) == Some(0.0);
            set(&me, s.ui.menu.new_game_check, 0.0);
            let effects = {
                let mut c = s.lock();
                c.menu.native_confirm = false;
                c.page.answer(yes)
            };
            play(call, s, asset::SELECT);
            apply(call, s, &me, effects);
            return None;
        }
        confirm_row(call, s, &me, pos);
        return None;
    }
    match menu::route(sn.coop_row, sn.raised, pos.round() as i64) {
        Route::Original => None,
        Route::OpenCoop => {
            call.skip_original();
            if guards_ok(call, s, &me) {
                set(&me, s.ui.menu.pause_input, 7.0);
                play(call, s, asset::SELECT);
                open_page(&me, s, PageId::Main);
            }
            None
        }
        Route::Shifted(p) => {
            #[allow(clippy::cast_precision_loss)]
            set(&me, s.ui.menu.pos, p as f64);
            Some(pos)
        }
        Route::Swallow => {
            call.skip_original();
            None
        }
    }
}

/// `confirmMenu` after: put a shifted `pos` back.
pub(crate) fn confirm_menu_after(call: &mut Call<'_>, s: &Shared, saved: Option<f64>) {
    if let (Some(pos), Some(me)) = (saved, call.instance()) {
        set(&me, s.ui.menu.pos, pos);
    }
}

/// What a Draw `before` changed, for its `after`.
#[derive(Debug, Default)]
pub struct Swapped {
    globals: Vec<(Variable, Value)>,
    locals: Vec<(Variable, f64)>,
}

fn swap_global(call: &Call<'_>, sw: &mut Swapped, var: Option<Variable>, text: &str) -> bool {
    let (Some(g), Some(v)) = (call.global(), var) else {
        return false;
    };
    let Some(old) = g.get_value(v) else {
        return false;
    };
    if old.as_str() == Some(text) {
        return true;
    }
    match g.set(v, text) {
        Ok(()) => {
            sw.globals.push((v, old));
            true
        }
        Err(e) => {
            warn_once(
                "ui-swap",
                &format!("co-op UI: cannot write a menu string: {e}"),
            );
            false
        }
    }
}

fn restore(call: &Call<'_>, sw: Swapped) {
    if let Some(g) = call.global() {
        for (v, old) in sw.globals.into_iter().rev() {
            if let Err(e) = g.set(v, old) {
                warn_once(
                    "ui-restore",
                    &format!("co-op UI: cannot restore a menu string: {e}"),
                );
            }
        }
    }
    if let Some(me) = call.instance() {
        for (v, old) in sw.locals.into_iter().rev() {
            let _ = me.set_f64(v, old);
        }
    }
}

/// `oMenu_Draw_64` before: write the row labels the game will draw.
pub(crate) fn menu_draw_before(call: &mut Call<'_>, s: &Shared) -> Swapped {
    let mut sw = Swapped::default();
    let sn = snap(s);
    if sn.open {
        let back = back_label(call, s);
        let (labels, question) = {
            let c = s.lock();
            let status = c.net.lobby_status();
            let steam = c.steam.view();
            let mut rows: Vec<(String, bool)> = c
                .page
                .rows(&steam, status.as_ref())
                .into_iter()
                .map(|r| {
                    let label = match (&back, r.id) {
                        (Some(b), RowId::Back) if !b.is_empty() => b.clone(),
                        _ => r.label,
                    };
                    (label, r.enabled)
                })
                .collect();
            if c.page.entry().is_some() {
                rows.clear();
            }
            (
                menu::page_labels(&rows),
                c.menu
                    .native_confirm
                    .then(|| c.page.confirm().map(|q| q.question.clone()))
                    .flatten(),
            )
        };
        for (var, label) in s.ui.rows.iter().zip(labels.iter()) {
            swap_global(call, &mut sw, *var, label);
        }
        if let (Some(me), Some(v)) = (call.instance(), s.ui.menu.warning_alpha)
            && let Some(old) = me.get_f64(v)
            && me.set_f64(v, 0.0)
        {
            sw.locals.push((v, old));
        }
        if let Some(q) = question {
            swap_global(call, &mut sw, s.ui.resume_query, &q);
        }
        return sw;
    }
    let Some(game_max) = s.lock().menu.game_max else {
        return sw;
    };
    let Some(me) = call.instance() else { return sw };
    // The row only where row 11 can be drawn too.
    let row11 = (frame_of(call, s).is_some() && menu_geom(call, &me, s).is_some())
        .then(|| insert_labels(call, s, &mut sw))
        .flatten();
    let mut drawn = row11.is_some();
    if !follow_row_limit(&me, s, game_max, drawn) && drawn {
        // Row 11 out of reach: no row this frame either.
        restore(call, std::mem::take(&mut sw));
        drawn = false;
    }
    let row11 = row11.filter(|_| drawn);
    let mut c = s.lock();
    c.menu.coop_row = drawn;
    if let Some(r) = row11 {
        c.menu.row11 = r;
        c.menu.failed_frames = 0;
    } else {
        let before = c.menu.failed_frames;
        c.menu.failed_frames = before.saturating_add(1);
        if streak_warns(before, SWAP_WARN_FRAMES) {
            warn_once(
                "ui-labels",
                "co-op UI: the main menu's labels cannot be swapped; no Co-op row until they can",
            );
        }
    }
    sw
}

/// Frames in a row without the Co-op row before it is worth a warning.
const SWAP_WARN_FRAMES: u32 = 60;

/// Write the Co-op row's labels; on a failure put back what was written.
fn insert_labels(call: &Call<'_>, s: &Shared, sw: &mut Swapped) -> Option<String> {
    let g = call.global()?;
    let old: Vec<String> =
        s.ui.rows
            .iter()
            .map(|v| g.get_value((*v)?)?.as_str().map(str::to_owned))
            .collect::<Option<_>>()?;
    let (writes, extra) = menu::inserted_labels(&old.try_into().ok()?, en::COOP_ROW);
    let ok =
        s.ui.rows
            .iter()
            .zip(writes)
            .all(|(var, w)| w.is_none_or(|w| swap_global(call, sw, *var, &w)));
    if ok {
        Some(extra)
    } else {
        restore(call, std::mem::take(sw));
        None
    }
}

/// `maxPos` follows the Co-op row, the selection staying on its label. False
/// when `maxPos` could not be written.
fn follow_row_limit(me: &Instance<'_>, s: &Shared, game_max: f64, drawn: bool) -> bool {
    let m = &s.ui.menu;
    if s.lock().menu.raised == drawn {
        return true;
    }
    let max = if drawn { game_max + 1.0 } else { game_max };
    if !m.max_pos.is_some_and(|v| me.set_f64(v, max)) {
        return false;
    }
    if let Some(pos) = num(me, m.pos) {
        set(me, m.pos, menu::pos_across(pos, drawn));
    }
    s.lock().menu.raised = drawn;
    if drawn {
        log::info!("co-op UI: Co-op menu row on (maxPos {game_max} -> {max})");
    }
    true
}

fn menu_geom(call: &Call<'_>, me: &Instance<'_>, s: &Shared) -> Option<MenuGeom> {
    let m = &s.ui.menu;
    // The menu's fade from black covers what the mod draws too.
    let black = global_value(call, s.ui.menu_fade)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        .then(|| num(me, m.black_alpha))
        .flatten();
    let geom = MenuGeom {
        h_pos: num(me, m.h_pos)?,
        v_pos: num(me, m.v_pos)?,
        ll: num(me, m.ll)?,
        alpha: layout::faded(num(me, m.alpha).unwrap_or(1.0), black),
        dialog: truthy(me, m.new_game_check).unwrap_or(false),
    };
    if geom.ll.abs() < 1.0 {
        return None;
    }
    Some(geom)
}

/// What the menu's Draw `after` draws.
enum What {
    Page(PageView),
    Menu(String, Option<String>),
    Nothing,
}

/// `oMenu_Draw_64` after: put the strings back, then draw the extras.
pub(crate) fn menu_draw_after(call: &mut Call<'_>, s: &Shared, sw: Swapped) {
    restore(call, sw);
    let Some(me) = call.instance() else { return };
    let Some(f) = frame_of(call, s) else { return };
    let Some(geom) = menu_geom(call, &me, s) else {
        warn_once(
            "ui-geom",
            "co-op UI: oMenu.hPos/vPos/ll unreadable; the co-op extras are not drawn",
        );
        return;
    };
    let back = back_label(call, s);
    let what = {
        let c = s.lock();
        let status = c.net.lobby_status();
        let steam = c.steam.view();
        if let Some(mut v) = c.page.view(&steam, status.as_ref()) {
            localize(&mut v, back.as_deref());
            What::Page(v)
        } else if c.menu.coop_row {
            let st = if c.settings.overlay {
                menu::row_status(status.as_ref(), &steam, &c.lang)
            } else {
                None
            };
            What::Menu(c.menu.row11.clone(), st)
        } else {
            What::Nothing
        }
    };
    let painter = Painter::new(call, s);
    let ops: Vec<Op> = match &what {
        What::Page(v) => layout::page_ops(v, &geom, f, &painter),
        What::Menu(row11, st) => {
            layout::menu_extras(&geom, row11, en::COOP_ROW, st.as_deref(), f, &painter)
        }
        What::Nothing => Vec::new(),
    };
    painter.draw(&ops, f);
    painter.finish();
}

/// `oController_Draw_64` before: this frame's run UI and the pause relabel.
pub(crate) fn run_draw_before(call: &mut Call<'_>, s: &Shared) -> Swapped {
    let mut sw = Swapped::default();
    let now = Instant::now();
    let Some(me) = call.instance() else { return sw };
    let paused = num(&me, s.h.pause).is_some_and(|p| p != 0.0);
    let ui = {
        let mut guard = s.lock();
        let c = &mut *guard;
        let sw_cfg = c.settings.ui.switches(c.settings.overlay);
        if !(c.core.linked() || c.core.coop_run()) {
            // A solo run: nothing to say, nothing to look up.
            c.run_msgs.reset();
            c.run_ui = RunUi::default();
            return sw;
        }
        for ev in std::mem::take(&mut c.run_events) {
            c.run_msgs.event(ev, now);
        }
        let status = c.net.lobby_status();
        let peer = status
            .as_ref()
            .and_then(|st| st.connected_peers().next().cloned());
        if let Some(p) = c.core.partners().values().next() {
            c.last_partner = text::display_name(&p.name, &c.lang, text::FALLBACK_PARTNER);
        }
        let partner = if c.last_partner.is_empty() {
            text::FALLBACK_PARTNER.to_owned()
        } else {
            c.last_partner.clone()
        };
        let linked = c.core.linked();
        let facts = RunFacts {
            coop_run: c.core.coop_run(),
            linked,
            gate: c.core.gate_phase(),
            partner: &partner,
            rtt_ms: peer.as_ref().map(|p| p.rtt_ms),
            loss_pct: peer.as_ref().map(|p| p.loss_pct),
            shared_pause: None,
            upgrade: None,
            silent_for: c.silent_for,
            separate: c.rules.separated(),
            wait: c.core.wait_view(now),
            paused,
            host: c.core.hosting(),
            over: c.core.run_over().is_some(),
        };
        let ui = c.run_msgs.frame(&facts, sw_cfg, now);
        c.run_ui = ui.clone();
        ui
    };
    let relabel = absent(s.ui.pause_labels().map(|(n, v)| (n, v.is_some()))).is_empty();
    if paused
        && relabel
        && let Some(p) = &ui.pause
    {
        if let Some(r) = &p.row0 {
            swap_global(call, &mut sw, s.ui.pause_resume, r);
        }
        if let Some((label, query)) = &p.quit {
            swap_global(call, &mut sw, s.ui.pause_save_quit, label);
            swap_global(call, &mut sw, s.ui.pause_exit, label);
            swap_global(call, &mut sw, s.ui.pause_save_quit_query, query);
            swap_global(call, &mut sw, s.ui.pause_confirm_quit, query);
        }
    }
    sw
}

/// `oController_Draw_64` after: restore the pause strings, draw the run UI.
pub(crate) fn run_paint(
    call: &Call<'_>,
    s: &Shared,
    sw: Swapped,
    partner: Vec<crate::render::DrawItem>,
) {
    restore(call, sw);
    let mut ui: RunUi = s.lock().run_ui.clone();
    if s.ui.text_box.is_none() {
        ui.bars.clear(); // no bar without drawTextBox: said at start-up
    }
    if partner.is_empty() && ui == RunUi::default() {
        return;
    }
    let Some(f) = frame_of(call, s) else { return };
    let time_alpha = call
        .instance()
        .and_then(|me| num(&me, s.ui.time_alpha))
        .unwrap_or(1.0);
    let paused = call
        .instance()
        .and_then(|me| num(&me, s.h.pause))
        .is_some_and(|p| p != 0.0);
    let wave_alert = ui.alert.is_some()
        && s.ui.asset(call, asset::WAVE_ALERT).is_some_and(|o| {
            call_b(call, s.ui.b.instance_exists, &[o])
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    let (hud_corner, t) = {
        let mut c = s.lock();
        let t0 = *c.menu.started.get_or_insert_with(Instant::now);
        (c.settings.ui.hud_corner, t0.elapsed().as_secs_f64())
    };
    let pause_rows = if paused { pause_rows(call, s) } else { None };
    let painter = Painter::new(call, s);
    let mut ops = layout::partner_ops(partner, paused);
    ops.extend(layout::run_ops(
        &ui,
        RunStyle {
            hud_corner,
            time_alpha,
            wave_alert,
            t,
            paused,
            pause_rows,
        },
        f,
        &painter,
    ));
    painter.draw(&ops, f);
    painter.finish();
}

/// A note outside the run shows only with `drawTextBox`.
fn shown_note(note: Option<&str>, text_box: bool) -> Option<&str> {
    note.filter(|_| text_box)
}

/// A note bar and the link line on a screen outside the run (the loadout
/// screen), as the run's bars; either may be absent.
pub(crate) fn paint_note(
    call: &Call<'_>,
    s: &Shared,
    note: Option<&str>,
    hud: Option<super::run::Hud>,
) {
    let note = shown_note(note, s.ui.text_box.is_some());
    if note.is_none() && hud.is_none() {
        return;
    }
    let Some(f) = frame_of(call, s) else { return };
    let ui = RunUi {
        bars: note.map(super::run::Bar::new).into_iter().collect(),
        hud,
        ..RunUi::default()
    };
    // The screen's bottom left holds its own score lines.
    let hud_corner = layout::HudCorner::Right;
    let painter = Painter::new(call, s);
    let ops = layout::run_ops(
        &ui,
        RunStyle {
            hud_corner,
            time_alpha: 1.0,
            wave_alert: false,
            t: 0.0,
            paused: false,
            pause_rows: None,
        },
        f,
        &painter,
    );
    painter.draw(&ops, f);
    painter.finish();
}

/// The `fontSmall` line height for placing partner labels.
pub(crate) fn label_line(call: &Call<'_>, s: &Shared) -> f64 {
    let p = Painter::new(call, s);
    let h = p.line(Font::Small);
    p.finish();
    h
}

/// The pause menu's rows are `pos` 1-4; Resume is row 1.
const RESUME_ROW: f64 = 1.0;
/// The pause menu's row 1 (`hPos[1]`, `vPos[1]`), read through array builtins.
fn pause_rows(call: &Call<'_>, s: &Shared) -> Option<layout::PauseRows> {
    let me = call.instance()?;
    let id = me.builtin_value("id").ok()?;
    let b = &s.ui.b;
    let (get, at, len) = (b.var_get?, b.array_get?, b.array_length?);
    let elem = |name: &str, i: usize| -> Option<f64> {
        let arr = call
            .call_builtin_held(
                get,
                &[Arg::Value(&id), Arg::Value(&Value::Str(name.into()))],
            )
            .ok()?;
        let n = call
            .call_builtin_held(len, &[Arg::Held(&arr)])
            .ok()?
            .value()
            .as_f64()?;
        #[allow(clippy::cast_precision_loss)]
        if n <= i as f64 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let idx = Value::Real(i as f64);
        call.call_builtin_held(at, &[Arg::Held(&arr), Arg::Value(&idx)])
            .ok()?
            .value()
            .as_f64()
    };
    let alpha = num(&me, s.ui.menu.alpha).unwrap_or(1.0).clamp(0.0, 1.0);
    Some(layout::PauseRows {
        x: elem("hPos", 1)?,
        first_y: elem("vPos", 1)?,
        alpha,
    })
}

/// `confirmPause` before: Resume is refused while the relabelled row says why.
pub(crate) fn confirm_pause_before(call: &mut Call<'_>, s: &Shared) {
    let Some(me) = call.instance() else { return };
    let pos = num(&me, s.ui.menu.pos);
    let blocked = {
        let c = s.lock();
        c.run_ui.pause.as_ref().is_some_and(|p| p.row0_blocked)
    };
    if blocked && pos == Some(RESUME_ROW) {
        call.skip_original();
        play(call, s, asset::MOVE);
    }
}

/// The partner left this run for good: what the run UI should say.
#[must_use]
pub fn link_left(name: &str, lang: &str) -> LinkEvent {
    LinkEvent::Left {
        name: text::display_name(name, lang, text::FALLBACK_PARTNER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without `drawTextBox` the loadout screen's note goes.
    #[test]
    fn the_loadout_note_goes_with_the_hint_bars() {
        assert_eq!(
            shown_note(Some("Sam picks the run"), true),
            Some("Sam picks the run")
        );
        assert_eq!(shown_note(Some("Sam picks the run"), false), None);
        assert_eq!(shown_note(None, true), None);
    }

    /// Missing pieces are named in order; one missing pause string is enough.
    #[test]
    fn missing_pieces_are_named_and_one_is_enough() {
        assert_eq!(absent([("pos", true), ("maxPos", true)]), [""; 0]);
        assert_eq!(
            absent([("pos", false), ("ll", true), ("drawTextBox", false)]),
            ["pos", "drawTextBox"]
        );
        let mut labels = [
            ("strPauseMenuResumeGame", true),
            ("strPauseMenuSaveAndQuit", true),
            ("strPauseMenuExitGame", true),
            ("strPauseMenuSaveAndQuitQuery", true),
            ("strPauseMenuConfirmQuit", true),
        ];
        assert_eq!(absent(labels), [""; 0], "all there: relabel");
        labels[3].1 = false;
        assert_eq!(
            absent(labels),
            ["strPauseMenuSaveAndQuitQuery"],
            "one gone: none"
        );
    }

    #[test]
    fn the_co_op_row_needs_the_games_ten_rows() {
        assert_eq!(row_max(Some(10.0)), Ok(10.0));
        assert_eq!(row_max(Some(12.0)), Ok(12.0));
        assert_eq!(row_max(Some(9.0)), Err("is 9, not 10".into()));
        assert_eq!(row_max(None), Err("cannot be read".into()));
    }

    /// One warning per failure streak, at its `limit`-th miss; no wrap.
    #[test]
    fn a_failure_streak_warns_once_at_its_limit() {
        for limit in [SWAP_WARN_FRAMES, FONT_WARN_DRAWS] {
            let mut n = 0_u32;
            let mut warned = 0;
            for _ in 0..limit * 3 {
                warned += u32::from(streak_warns(n, limit));
                n = n.saturating_add(1);
            }
            assert_eq!(warned, 1, "{limit}");
            n = 0; // a success
            let at = (0..limit * 2)
                .position(|_| {
                    let w = streak_warns(n, limit);
                    n += 1;
                    w
                })
                .unwrap();
            assert_eq!(at + 1, limit as usize);
            assert!(!streak_warns(u32::MAX, limit));
        }
    }

    #[test]
    fn link_events_carry_a_drawable_name() {
        assert_eq!(
            link_left("Сэм", "english"),
            LinkEvent::Left {
                name: "Player 2".into()
            }
        );
        assert_eq!(
            link_left("Wing", "english"),
            LinkEvent::Left {
                name: "Wing".into()
            }
        );
    }
}

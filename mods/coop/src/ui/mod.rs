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

//! The co-op UI, drawn in the game's own style.
//!
//! * [`menu`], [`page`], [`lan`]: the "Co-op" main-menu row and its pages.
//! * [`run`]: in-run messages (hint bar, alert, HUD line, pause, waiting card).
//! * [`steam_link`], [`steam_coop`]: the seam to the Steam connector.
//! * [`style`], [`text`], [`layout`]: fonts, colours, strings, positions.
//! * [`prefs`], [`settings`]: config switches and saving the page's choices.
//! * [`glue`]: the hooks. All but `glue` and `steam_coop::CoopSteam` are pure.

#![forbid(unsafe_code)]

pub mod glue;
pub mod lan;
pub mod layout;
pub mod menu;
pub mod page;
pub mod prefs;
pub mod run;
pub mod settings;
pub mod steam_coop;
pub mod steam_link;
pub mod style;
pub mod text;

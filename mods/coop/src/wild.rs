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

//! `wild_mods`, the Co-op page's Wild Mods row: a player's own choice of
//! wild mods on their upgrade screens in a co-op run, apart from the host's
//! Wild Metamorphosis. Nothing is sent to the partner.
//!
//! * `oUpgrade_Create_0` reads `global.gameModWildMetamorphosis` for the
//!   screen's wild slots. `initializeLoadout` and `refreshWild` read it for
//!   `oPowers.wildModCount`, and `setRerolls` halves the rerolls when it is on.
//! * For the length of those four calls the flag reads as this player chose,
//!   then the run's value is put back. Drops, music, the end screen and
//!   records keep the run's value.
//! * The choice counts from the next run.
//! * A Daily Pandemonium screen and a full-wild screen after
//!   Transmogrification do not read the flag.

#![forbid(unsafe_code)]

use ndml_sdk::Value;
use serde::Deserialize;

/// The config value (`wild_mods`) and the page row's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WildMods {
    /// As the run's Wild Metamorphosis setting (the host's). `"game"` reads as
    /// `Sync`.
    #[default]
    #[serde(alias = "game")]
    Sync,
    /// Offered to this player in a co-op run, the rerolls halved.
    On,
    /// None offered to this player in a co-op run (the normal rerolls too).
    Off,
}

impl WildMods {
    const ALL: [Self; 3] = [Self::Sync, Self::On, Self::Off];

    /// The row's value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Sync => "Sync",
            Self::On => "Force On",
            Self::Off => "Force Off",
        }
    }

    /// The config file's word.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// What the value means, for the page's column.
    #[must_use]
    pub fn explain(self) -> &'static str {
        match self {
            Self::Sync => {
                "Wild mods as the host's run has them (Wild Metamorphosis), for both of you."
            }
            Self::On => {
                "Wild mods on your own upgrade screens in co-op, even when the host has Wild \
                 Metamorphosis off. Your rerolls are halved, as with Wild Metamorphosis."
            }
            Self::Off => {
                "No wild mods on your own upgrade screens in co-op, even when the host has Wild \
                 Metamorphosis on. You keep the normal rerolls."
            }
        }
    }

    /// The next value left (`dir < 0`) or right.
    #[must_use]
    pub fn step(self, dir: i32) -> Self {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        let n = Self::ALL.len();
        Self::ALL[if dir < 0 {
            (i + n - 1) % n
        } else {
            (i + 1) % n
        }]
    }
}

/// This player's choice for `gameModWildMetamorphosis`, in the flag's own
/// kind. `None`: leave it (Sync, not a co-op run, already as chosen, or not a
/// flag).
#[must_use]
pub fn flag_value(setting: WildMods, coop: bool, current: Option<&Value>) -> Option<Value> {
    if !coop {
        return None;
    }
    let want = match setting {
        WildMods::Sync => return None,
        WildMods::On => true,
        WildMods::Off => false,
    };
    let (on, value) = match current? {
        Value::Bool(b) => (*b, Value::Bool(want)),
        Value::Real(v) => (*v > 0.5, Value::Real(if want { 1.0 } else { 0.0 })),
        Value::Int(v) => (*v != 0, Value::Int(i64::from(want))),
        _ => return None,
    };
    (on != want).then_some(value)
}

/// The value put aside while the flag reads as chosen. The run and menu ticks
/// call [`Pending::take`] to put it back when an after-hook never ran.
#[derive(Debug, Default)]
pub struct Pending(std::sync::Mutex<Option<Value>>);

impl Pending {
    /// The flag now reads as chosen; `old` is what to put back.
    pub fn set(&self, old: Value) {
        if let Ok(mut p) = self.0.lock() {
            // An older pending value is the run's value too; keep the first.
            p.get_or_insert(old);
        }
    }

    /// The value to put back, once.
    pub fn take(&self) -> Option<Value> {
        self.0.lock().ok().and_then(|mut p| p.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_only_for_off_in_a_coop_run_with_the_flag_on() {
        let on = Value::Bool(true);
        assert_eq!(flag_value(WildMods::Sync, true, Some(&on)), None);
        assert_eq!(flag_value(WildMods::Off, false, Some(&on)), None, "solo");
        assert_eq!(
            flag_value(WildMods::Off, true, Some(&on)),
            Some(Value::Bool(false))
        );
        assert_eq!(
            flag_value(WildMods::Off, true, Some(&Value::Real(1.0))),
            Some(Value::Real(0.0))
        );
        assert_eq!(
            flag_value(WildMods::Off, true, Some(&Value::Int(1))),
            Some(Value::Int(0))
        );
        for already in [
            Value::Bool(false),
            Value::Real(0.0),
            Value::Int(0),
            Value::Undefined,
        ] {
            assert_eq!(flag_value(WildMods::Off, true, Some(&already)), None);
        }
        assert_eq!(flag_value(WildMods::Off, true, None), None, "missing");
    }

    #[test]
    fn on_only_for_on_in_a_coop_run_with_the_flag_off() {
        let off = Value::Bool(false);
        assert_eq!(flag_value(WildMods::Sync, true, Some(&off)), None);
        assert_eq!(flag_value(WildMods::On, false, Some(&off)), None, "solo");
        assert_eq!(
            flag_value(WildMods::On, true, Some(&off)),
            Some(Value::Bool(true))
        );
        assert_eq!(
            flag_value(WildMods::On, true, Some(&Value::Real(0.0))),
            Some(Value::Real(1.0)),
            "the game compares the flag with 1"
        );
        assert_eq!(
            flag_value(WildMods::On, true, Some(&Value::Int(0))),
            Some(Value::Int(1))
        );
        for already in [Value::Bool(true), Value::Real(1.0), Value::Int(1)] {
            assert_eq!(flag_value(WildMods::On, true, Some(&already)), None);
        }
        assert_eq!(
            flag_value(WildMods::On, true, Some(&Value::Undefined)),
            None,
            "not a flag"
        );
    }

    #[test]
    fn pending_gives_back_the_first_value_once() {
        let p = Pending::default();
        assert_eq!(p.take(), None);
        p.set(Value::Bool(true));
        p.set(Value::Bool(false));
        assert_eq!(p.take(), Some(Value::Bool(true)));
        assert_eq!(p.take(), None);
    }

    #[test]
    fn the_config_value_reads_every_word_and_game_as_sync() {
        #[derive(Deserialize)]
        struct F {
            wild_mods: WildMods,
        }
        let read =
            |s: &str| toml::from_str::<F>(&format!("wild_mods = \"{s}\"")).map(|f| f.wild_mods);
        assert_eq!(read("sync").ok(), Some(WildMods::Sync));
        assert_eq!(read("game").ok(), Some(WildMods::Sync), "\"game\" is Sync");
        assert_eq!(read("on").ok(), Some(WildMods::On));
        assert_eq!(read("off").ok(), Some(WildMods::Off));
        assert!(read("force").is_err());
        for w in WildMods::ALL {
            assert_eq!(read(w.key()).ok(), Some(w), "the row writes what is read");
        }
    }

    #[test]
    fn the_row_cycles_three_values_both_ways() {
        let mut w = WildMods::Sync;
        let mut seen = Vec::new();
        for _ in 0..3 {
            w = w.step(1);
            seen.push(w);
        }
        assert_eq!(seen, vec![WildMods::On, WildMods::Off, WildMods::Sync]);
        assert_eq!(WildMods::Sync.step(-1), WildMods::Off);
        assert_eq!(
            WildMods::ALL.map(WildMods::label),
            ["Sync", "Force On", "Force Off"]
        );
        for w in WildMods::ALL {
            assert!(w.explain().is_ascii() && !w.explain().contains("loadout"));
        }
    }
}

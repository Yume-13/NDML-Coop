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

//! `wild_mods`: a player's choice to have no wild mods offered on their own
//! upgrade screens in a co-op run, even when the host's run has Wild
//! Metamorphosis on.
//!
//! `oUpgrade_Create_0` (wild slots) and `setRerolls` (halved rerolls) are the
//! only reads of `global.gameModWildMetamorphosis` that matter here; with
//! `"off"` the flag reads 0 for the length of those two calls and is then put
//! back. Daily Pandemonium screens and the full-wild screen after
//! Transmogrification do not read the flag.

#![forbid(unsafe_code)]

use ndml_sdk::Value;
use serde::Deserialize;

/// The config value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WildMods {
    /// As the run's Wild Metamorphosis setting (the game's own behaviour).
    #[default]
    Game,
    /// None offered to this player in a co-op run (the normal rerolls too).
    Off,
}

/// The "off" value of the flag's kind to write for one event, or `None` to
/// leave it as it is.
#[must_use]
pub fn off_value(setting: WildMods, coop: bool, current: Option<&Value>) -> Option<Value> {
    if setting != WildMods::Off || !coop {
        return None;
    }
    match current? {
        Value::Bool(true) => Some(Value::Bool(false)),
        Value::Real(v) if *v > 0.5 => Some(Value::Real(0.0)),
        Value::Int(v) if *v != 0 => Some(Value::Int(0)),
        _ => None,
    }
}

/// The value put aside while the flag reads off, so the run and menu ticks
/// can put it back when an event's after-hook never ran.
#[derive(Debug, Default)]
pub struct Pending(std::sync::Mutex<Option<Value>>);

impl Pending {
    /// The flag now reads off; `old` is what to put back.
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
        assert_eq!(off_value(WildMods::Game, true, Some(&on)), None);
        assert_eq!(off_value(WildMods::Off, false, Some(&on)), None, "solo");
        assert_eq!(
            off_value(WildMods::Off, true, Some(&on)),
            Some(Value::Bool(false))
        );
        assert_eq!(
            off_value(WildMods::Off, true, Some(&Value::Real(1.0))),
            Some(Value::Real(0.0))
        );
        assert_eq!(
            off_value(WildMods::Off, true, Some(&Value::Int(1))),
            Some(Value::Int(0))
        );
        for already in [
            Value::Bool(false),
            Value::Real(0.0),
            Value::Int(0),
            Value::Undefined,
        ] {
            assert_eq!(off_value(WildMods::Off, true, Some(&already)), None);
        }
        assert_eq!(off_value(WildMods::Off, true, None), None, "missing");
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
    fn the_config_value_reads_both_words() {
        #[derive(Deserialize)]
        struct F {
            wild_mods: WildMods,
        }
        let f: F = toml::from_str("wild_mods = \"off\"").expect("off");
        assert_eq!(f.wild_mods, WildMods::Off);
        let f: F = toml::from_str("wild_mods = \"game\"").expect("game");
        assert_eq!(f.wild_mods, WildMods::Game);
        assert!(
            toml::from_str::<F>("wild_mods = \"on\"").is_err(),
            "no forcing on"
        );
    }
}

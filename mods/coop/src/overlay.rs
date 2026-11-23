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

//! The session's status in words, as `state::Core` reports it (the in-run UI
//! is `ui::run`).

#![forbid(unsafe_code)]

use crate::gate::GatePhase;
use crate::render::color;

/// One overlay line and its colour.
pub type Line = (String, u32);

/// What the run overlay shows.
#[derive(Debug, Clone, Default)]
pub struct RunView<'a> {
    /// A partner is connected.
    pub linked: bool,
    /// This run started linked.
    pub coop_run: bool,
    pub phase: Option<GatePhase>,
    pub partner: Option<&'a str>,
    /// Round trip to the partner.
    pub rtt_ms: Option<f64>,
    /// Why the partner is gone (this run).
    pub left: Option<&'a str>,
    /// Recent one-off messages.
    pub notices: Vec<&'a str>,
}

/// The run overlay. Empty for a solo run with no link (the unmodded game).
#[must_use]
pub fn run_lines(v: &RunView<'_>) -> Vec<Line> {
    let mut out = Vec::new();
    let name = v.partner.unwrap_or("your partner");
    match (v.linked, v.coop_run) {
        (false, false) => {}
        (true, false) => out.push((
            format!("CO-OP  linked with {name}: co-op starts with your next run"),
            color::DIM,
        )),
        (false, true) => out.push((
            format!(
                "CO-OP  {}. Playing on alone; saves stay off for this run.",
                v.left.unwrap_or("Your partner left")
            ),
            color::AMBER,
        )),
        (true, true) => match v.phase {
            Some(GatePhase::Holding) => out.push((
                format!("CO-OP  waiting for {name} to start the run..."),
                color::AMBER,
            )),
            Some(GatePhase::Refused { .. }) => {
                out.push((
                    "CO-OP  run refused: your game modes differ.".into(),
                    color::RED,
                ));
                out.push((
                    "Both players must pick the same mode and modifiers.".into(),
                    color::TEXT,
                ));
            }
            _ => out.push((
                match v.rtt_ms {
                    Some(r) => format!("CO-OP  {name}  {r:.0} ms"),
                    None => format!("CO-OP  {name}"),
                },
                color::CYAN,
            )),
        },
    }
    if v.linked || v.coop_run {
        out.extend(v.notices.iter().map(|n| ((*n).to_owned(), color::TEXT)));
    }
    out
}

/// Names as "A", "A and B", "A, B and C".
#[must_use]
pub fn join_names(names: &[&str]) -> String {
    match names {
        [] => "your partner".into(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The start-gate status (empty unless the game is held at the gate).
#[must_use]
pub fn gate_banner(phase: GatePhase, partner: Option<&str>) -> Vec<Line> {
    let name = partner.unwrap_or("your partner");
    match phase {
        GatePhase::Holding => vec![
            (
                format!("CO-OP: waiting for {name} to start the run..."),
                color::AMBER,
            ),
            (
                "The run begins for both of you as soon as they arrive.".into(),
                color::TEXT,
            ),
        ],
        GatePhase::Refused { .. } => vec![
            (
                "CO-OP: run refused, your game modes differ".into(),
                color::RED,
            ),
            (
                "Both players must pick the same mode and modifiers.".into(),
                color::TEXT,
            ),
        ],
        GatePhase::Idle | GatePhase::Running => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solo_runs_show_nothing() {
        assert!(run_lines(&RunView::default()).is_empty());
        let v = RunView {
            notices: vec!["ignored"],
            ..RunView::default()
        };
        assert!(run_lines(&v).is_empty());
    }

    #[test]
    fn run_lines_follow_the_gate_and_link() {
        let mut v = RunView {
            linked: true,
            coop_run: true,
            phase: Some(GatePhase::Holding),
            partner: Some("Wing"),
            rtt_ms: Some(42.4),
            ..RunView::default()
        };
        assert!(run_lines(&v)[0].0.contains("waiting for Wing"));
        v.phase = Some(GatePhase::Running);
        assert_eq!(run_lines(&v)[0].0, "CO-OP  Wing  42 ms");
        v.phase = Some(GatePhase::Refused {
            peer: ndml_net::PeerId(1),
        });
        let l = run_lines(&v);
        assert!(l[0].0.contains("modes differ") && l.len() == 2);
        v.linked = false;
        v.left = Some("Wing left (timed out)");
        assert!(
            run_lines(&v)[0]
                .0
                .contains("Wing left (timed out). Playing on alone")
        );
        v.coop_run = false;
        v.linked = true;
        assert!(run_lines(&v)[0].0.contains("next run"));
    }

    #[test]
    fn gate_banner_only_while_held() {
        assert!(gate_banner(GatePhase::Running, Some("Wing")).is_empty());
        let l = gate_banner(GatePhase::Holding, Some("Wing"));
        assert!(l[0].0.contains("waiting for Wing"));
        let r = gate_banner(
            GatePhase::Refused {
                peer: ndml_net::PeerId(1),
            },
            None,
        );
        assert!(r[0].0.contains("modes differ"));
    }

    #[test]
    fn names_are_joined() {
        assert_eq!(join_names(&[]), "your partner");
        assert_eq!(join_names(&["A", "B", "C"]), "A, B and C");
    }
}

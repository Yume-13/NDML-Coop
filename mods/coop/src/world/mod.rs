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

//! The shared world: the host's enemies and what they depend on replicated
//! into the joining game, per-enemy targeting of both players, the partner's
//! ship as an in-world avatar, enemy shots, area effects, world view, waves
//! and the co-op colours.
//!
//! The host owns enemies (existence, position, hull, death). The joining game
//! makes each copy with a bare create and the host's whole state ([`state`]),
//! then keeps it on the host's interpolated pose; the copy's own code runs its
//! animation, attacks and collisions. The joining player's hits on copies
//! cross as damage reports ([`crate::damage`]). Game calls and hooks are in
//! [`glue`] and its `*_glue` parts; the other modules are pure.

#![forbid(unsafe_code)]

pub mod aim;
pub mod area;
pub mod classify;
pub mod client;
pub mod color;
pub mod config;
pub mod entities;
pub mod glue;
pub mod host;
pub mod lifecycle;
pub mod names;
pub mod present;
pub mod put_back;
pub mod repl;
pub mod shots;
pub mod state;
pub mod target;

use std::time::{Duration, Instant};

use ndml_net::endpoint::Event;
use ndml_net::msg::{Message, PeerId};

pub use config::WorldSettings;
pub use lifecycle::Loss;
pub use present::PartnerView;

use crate::state::Core;
use color::ColorSync;

/// A network event sorted for its consumer.
#[derive(Debug)]
pub enum Routed {
    /// For the shared world.
    World(Message),
    /// A partner's colour.
    Color(PeerId, Option<u32>),
    /// For the session (`Core`). A `Disconnected` also reports its loss kind.
    Session(Event, Option<Loss>),
}

/// Sort one event.
#[must_use]
pub fn route(ev: Event) -> Routed {
    match ev {
        Event::Message {
            msg: Message::PlayerColor { peer, color },
            ..
        } => Routed::Color(peer, color),
        Event::Message { msg, .. }
            if matches!(
                msg,
                Message::EnemySpawn(_)
                    | Message::EnemySnapshot(_)
                    | Message::Despawn { .. }
                    | Message::WaveState { .. }
                    | Message::WorldResync { .. }
                    | Message::EnemyLook { .. }
                    | Message::SpawnMarker(_)
                    | Message::DamageReport(_)
                    | Message::DamageAck(_)
                    | Message::KillRefused { .. }
                    | Message::StateNames(_)
                    | Message::InstanceStateIx(_)
                    | Message::EnemySnapshotFine(_)
                    | Message::WorldView(_)
                    | Message::WorldCall(_)
                    | Message::Asleep { .. }
                    | Message::WorldInputs(_)
                    | Message::Status(_)
                    | Message::AreaState(_)
                    | Message::OrbSpawn { .. }
                    | Message::OrbCollect { .. }
                    | Message::PickupShared { .. }
                    | Message::ShotFire(_)
                    | Message::ShotGone(_)
                    | Message::ShotState(_)
                    | Message::ShotSteer(_)
                    | Message::AimThings(_)
                    | Message::CoverState(_)
                    | Message::LitStates(_)
                    | Message::LureState(_)
            ) =>
        {
            Routed::World(msg)
        }
        Event::Disconnected { reason, .. } => {
            let loss = Loss::of(reason);
            Routed::Session(ev, Some(loss))
        }
        other => Routed::Session(other, None),
    }
}

/// Every partner still in the run as the world needs it this frame: the ship
/// interpolated at `render(peer)`, else its newest state.
pub fn partner_views(
    core: &mut Core,
    now: Instant,
    render: &dyn Fn(PeerId) -> Option<Duration>,
    colors: &ColorSync,
) -> Vec<PartnerView> {
    let mut out = Vec::new();
    for (&peer, p) in core.run_partners_mut() {
        let fresh = p.ship.is_fresh(now);
        let Some(latest) = p.ship.latest().copied() else {
            continue;
        };
        let t = render(peer).unwrap_or_else(|| ndml_net::time::from_wire_ms(latest.time_ms));
        let at = p.ship.pose(t, now).map(|ps| (ps.pos, ps.vel, ps.angle));
        let at = at.or(Some((
            ndml_net::interp::Vec2::new(latest.x, latest.y),
            ndml_net::interp::Vec2::new(latest.vx, latest.vy),
            latest.angle,
        )));
        let sprite = (latest.sprite != u16::MAX).then_some(u32::from(latest.sprite));
        let mut look = p.ship.look().map_or_default(present::look_params);
        // The continuous look (`PlayerLookFrame`) at the pose's render time.
        if let Some(l) = p.ship.look_at(t) {
            look.xscale = f64::from(l.xscale);
            look.yscale = f64::from(l.yscale);
            look.alpha = f64::from(l.alpha);
            look.blend = l.blend;
        }
        if latest.downed {
            // Down: hidden at once, before the next `PlayerLook`.
            look.visible = false;
        }
        out.push(PartnerView {
            peer,
            name: p.name.clone(),
            fresh,
            downed: latest.downed,
            pose: at,
            sprite,
            frame: latest.frame,
            look,
            color: colors.partner(peer),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::endpoint::DisconnectReason;
    use ndml_net::msg::DespawnCause;

    #[test]
    fn routing() {
        let from = PeerId::HOST;
        let m = |msg| Event::Message { from, msg };
        assert!(matches!(
            route(m(Message::Despawn {
                net_id: 1,
                cause: DespawnCause::Removed
            })),
            Routed::World(_)
        ));
        assert!(matches!(
            route(m(Message::WorldResync { enemies: 0 })),
            Routed::World(_)
        ));
        // The status channel goes where the damage reports go.
        assert!(matches!(
            route(m(Message::Status(ndml_net::status::StatusMsg::Report {
                by: PeerId(1),
                entries: Vec::new()
            }))),
            Routed::World(_)
        ));
        // Every shared-shot message goes to the world.
        for msg in [
            Message::ShotFire(ndml_net::shots::ShotFire { shots: Vec::new() }),
            Message::ShotGone(ndml_net::shots::ShotGone { ends: Vec::new() }),
            Message::ShotSteer(ndml_net::shots::ShotSteer {
                changes: Vec::new(),
            }),
        ] {
            assert!(matches!(route(m(msg)), Routed::World(_)));
        }
        assert!(matches!(
            route(m(Message::PlayerColor {
                peer: from,
                color: Some(3)
            })),
            Routed::Color(p, Some(3)) if p == from
        ));
        assert!(matches!(
            route(m(Message::Go { start_ms: 1 })),
            Routed::Session(_, None)
        ));
        assert!(matches!(
            route(Event::Disconnected {
                peer: from,
                reason: DisconnectReason::Timeout
            }),
            Routed::Session(_, Some(Loss::Interrupted))
        ));
    }
}

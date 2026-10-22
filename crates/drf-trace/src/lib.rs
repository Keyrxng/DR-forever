// SPDX-License-Identifier: GPL-3.0-or-later

//! drf-trace: canonical trace vocabulary (std-only).
//!
//! Mirrors the project's canonical trace schema v1.1.
//! Dotted semantic names are canonical; build-specific callsites live only
//! in `Record.callsite`. Parsers must reject unknown names (subtractive
//! parsing), so `parse` returns `None` for anything outside this list.

/// All 23 canonical events from trace.schema.yaml v1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    FramePresent,
    UpdatePresent,
    TickBegin,
    TickEnd,
    RngDraw,
    ActionEnqueue,
    ActionDequeue,
    ActionExecute,
    EntitySpawn,
    EntityDespawn,
    EntityIterationOrder,
    PropertyWrite,
    MovementRequest,
    MovementResolve,
    HealthChange,
    PositionUpdate,
    EffectApply,
    AiTransition,
    SerializeIn,
    SerializeOut,
    NetRx,
    NetTx,
    EpochChange,
}

impl Event {
    /// Canonical dotted name — identical to the schema `event` enum.
    pub fn canonical_name(&self) -> &'static str {
        match self {
            Event::FramePresent => "frame.present",
            Event::UpdatePresent => "update.present",
            Event::TickBegin => "tick.begin",
            Event::TickEnd => "tick.end",
            Event::RngDraw => "rng.draw",
            Event::ActionEnqueue => "action.enqueue",
            Event::ActionDequeue => "action.dequeue",
            Event::ActionExecute => "action.execute",
            Event::EntitySpawn => "entity.spawn",
            Event::EntityDespawn => "entity.despawn",
            Event::EntityIterationOrder => "entity.iteration_order",
            Event::PropertyWrite => "property.write",
            Event::MovementRequest => "movement.request",
            Event::MovementResolve => "movement.resolve",
            Event::HealthChange => "health.change",
            Event::PositionUpdate => "position.update",
            Event::EffectApply => "effect.apply",
            Event::AiTransition => "ai.transition",
            Event::SerializeIn => "serialize.in",
            Event::SerializeOut => "serialize.out",
            Event::NetRx => "net.rx",
            Event::NetTx => "net.tx",
            Event::EpochChange => "epoch.change",
        }
    }

    /// All variants in schema order.
    pub fn all() -> [Event; 23] {
        [
            Event::FramePresent,
            Event::UpdatePresent,
            Event::TickBegin,
            Event::TickEnd,
            Event::RngDraw,
            Event::ActionEnqueue,
            Event::ActionDequeue,
            Event::ActionExecute,
            Event::EntitySpawn,
            Event::EntityDespawn,
            Event::EntityIterationOrder,
            Event::PropertyWrite,
            Event::MovementRequest,
            Event::MovementResolve,
            Event::HealthChange,
            Event::PositionUpdate,
            Event::EffectApply,
            Event::AiTransition,
            Event::SerializeIn,
            Event::SerializeOut,
            Event::NetRx,
            Event::NetTx,
            Event::EpochChange,
        ]
    }

    /// Subtractive parse: `None` for any name outside the canonical vocab.
    pub fn parse(name: &str) -> Option<Event> {
        Self::all().into_iter().find(|e| e.canonical_name() == name)
    }
}

/// Minimal envelope subset. `entity` is `None` where N/A (schema rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub tick: i64,
    pub frame: u64,
    pub event: Event,
    pub entity: Option<u64>,
    pub callsite: String,
}

impl Record {
    pub fn new(tick: i64, frame: u64, event: Event, entity: Option<u64>, callsite: &str) -> Self {
        Record {
            tick,
            frame,
            event,
            entity,
            callsite: callsite.to_string(),
        }
    }

    pub fn event_name(&self) -> &'static str {
        self.event.canonical_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_matches_schema_v1_1() {
        let names: Vec<&str> = Event::all().iter().map(|e| e.canonical_name()).collect();
        assert_eq!(names.len(), 23);
        for must in [
            "frame.present",
            "tick.end",
            "rng.draw",
            "action.execute",
            "entity.iteration_order",
            "property.write",
            "net.rx",
            "net.tx",
            "epoch.change",
        ] {
            assert!(names.contains(&must), "missing {must}");
        }
    }

    #[test]
    fn parse_roundtrips_and_rejects_unknown() {
        for e in Event::all() {
            assert_eq!(Event::parse(e.canonical_name()), Some(e));
        }
        assert_eq!(Event::parse("client.exe+0x1a2b3c"), None);
        assert_eq!(Event::parse("tick_end"), None);
    }

    #[test]
    fn record_construction_uses_canonical_name() {
        let r = Record::new(-1, 0, Event::TickEnd, None, "drf-sim!tick");
        assert_eq!(r.event_name(), "tick.end");
        assert_eq!(r.entity, None);
    }
}

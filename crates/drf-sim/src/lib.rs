// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-sim`: deterministic zone simulation primitives (std-only).
//!
//! Rules: no async, no threads, no wall-clock, no persistence, and no hidden
//! RNG. Ordering is fixed: FIFO input/action queues plus `Vec` entity order.
//! Stages whose protocol behavior is not yet pinned remain explicit no-ops.
//! An async executor must never define simulation ordering.

use std::collections::VecDeque;

pub mod movement;

/// Minimal entity handle. Position/health/combat are NOT modelled here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entity {
    pub id: u64,
}

/// Opaque client request queued into the next tick. Payload is uninterpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    pub entity: u64,
    pub payload: u32,
}

/// Authoritative action formed by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    pub entity: u64,
    pub payload: u32,
}

/// One simulated zone. Single owner per zone; `tick` only moves forward.
#[derive(Debug, Default)]
pub struct Zone {
    pub tick: u64,
    pub entities: Vec<Entity>,
    pub input_queue: VecDeque<Input>,
    pub action_queue: VecDeque<Action>,
    /// Canonical trace lines (dotted names mirror drf-trace vocabulary).
    pub trace: Vec<String>,
}

impl Zone {
    pub fn new() -> Self {
        Zone {
            tick: 0,
            entities: Vec::new(),
            input_queue: VecDeque::new(),
            action_queue: VecDeque::new(),
            trace: Vec::new(),
        }
    }

    pub fn add_entity(&mut self, id: u64) {
        self.entities.push(Entity { id });
    }

    pub fn submit(&mut self, input: Input) {
        self.input_queue.push_back(input);
    }

    /// Advance exactly one 30 Hz tick through the four fixed stages.
    pub fn tick(&mut self) {
        self.tick += 1;
        self.process_inputs();
        self.execute_actions();
        self.update_entities();
        self.emit_trace();
    }

    /// SIM-002: validation + action formation are UNKNOWN; current
    /// behaviour is a 1:1 FIFO move so ordering stays deterministic.
    fn process_inputs(&mut self) {
        while let Some(inp) = self.input_queue.pop_front() {
            self.action_queue.push_back(Action {
                entity: inp.entity,
                payload: inp.payload,
            });
        }
    }

    /// Action application is a documented no-op (combat/movement live elsewhere).
    fn execute_actions(&mut self) {
        while self.action_queue.pop_front().is_some() {}
    }

    /// SIM-001: entity update order is pinned `Vec` order; per-entity
    /// behaviour is a no-op until the replay rig defines it.
    fn update_entities(&mut self) {
        // Intentional no-op: iterate in place to pin the order.
        for _ in self.entities.iter() {}
    }

    /// Record one `tick.end` line per tick using the canonical dotted name.
    fn emit_trace(&mut self) {
        self.trace.push(format!("tick.end tick={}", self.tick));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_empty_at_tick_zero() {
        let z = Zone::new();
        assert_eq!(z.tick, 0);
        assert!(z.entities.is_empty());
        assert!(z.trace.is_empty());
    }

    #[test]
    fn tick_advances_and_emits_trace() {
        let mut z = Zone::new();
        z.add_entity(7);
        z.submit(Input {
            entity: 7,
            payload: 1,
        });
        z.tick();
        assert_eq!(z.tick, 1);
        assert_eq!(z.trace, vec!["tick.end tick=1".to_string()]);
        assert!(z.input_queue.is_empty());
        assert!(z.action_queue.is_empty());
    }

    #[test]
    fn same_inputs_give_identical_trace_twice() {
        let run = || {
            let mut z = Zone::new();
            z.add_entity(1);
            z.add_entity(2);
            for i in 0..4u64 {
                z.submit(Input {
                    entity: (i % 2) + 1,
                    payload: i as u32,
                });
            }
            for _ in 0..8 {
                z.tick();
            }
            (z.tick, z.trace)
        };
        let (t1, tr1) = run();
        let (t2, tr2) = run();
        assert_eq!(t1, t2);
        assert_eq!(tr1, tr2);
    }
}

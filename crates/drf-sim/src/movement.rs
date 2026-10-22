// SPDX-License-Identifier: GPL-3.0-or-later

//! Deterministic movement slice.
//!
//! Scope: one town patch, single avatar, movement only. No networking, no
//! sockets, no wall-clock, no threads, no async, no floats, no persistence.
//! Everything outside the patch is rejected, never simulated.
//!
//! Content contract for the movement fixture:
//! ```json
//! {"heightfield": [[x,y,w], ...], "bbox": [xmin,ymin,xmax,ymax],
//!  "spawn": [x,y] | [x,y,w], "blocked": [[ax,ay,bx,by] | [ax,ay,bx,by,nx,ny], ...]}
//! ```
//! All coordinates are raw x256 fixed-point integers (LE on the wire). Use
//! [`MovementModel::from_slice_json`] (takes `&str`; `drf-sim` touches no
//! disk, so the edge loads the file) or [`MovementModel::new`]
//! with a fixture table. `blocked` entries are directed gates (§Gates).
//!
//! ## Rule map (V1–V8)
//! | Rule | Check | Disposition |
//! | V1 record grammar | `[07 34][ent u32][n u8][p,X,Y,W 13B]xN`, `n>=1`, `len>=7+13n` | drop + `grammar_drops` |
//! | V2 plausibility+collision | entity relatches if all else passes (`entity_switch`), else reject; inside bbox+margin; no directed-gate crossing | relatch (`entity_switch`) / reject (`EntityMismatch` / `oob` / `wall_clip`) |
//! | V3 speed cap | per-leg `dist/dt` vs cap (MOVE-001) | reject (`speed_hack`), flag `speed_clamp` |
//! | V4 heightfield | `|W_in−W_table|<=band`; in-place `|dW|<=W_SETTLE_MAX_RAW` settles with `w_settle` (+`w_settles`), larger `dW!=0` at `dX=dY=0` rejected | accept+`w_mismatch` / accept+`w_settle` / reject (`vertical_event`) |
//! | V5 p-flags | p01 must be last + duplicate prev; p02/p03 accepted anywhere | reject (`p01_violation`) |
//! | V6 input foot/HP | c2s carries none; HP never taken from input | informational pass |
//! | V7 sequencing | FIFO in; backlog only metered, never fast-forwarded | meter `queue_depth` |
//! | V8 teleport/epoch | `Δ`-beyond-threshold jump reanchored as a new epoch; W-plane-shift plus a medium jump above `SNAP_MAX_RAW` is rejected; snap-jog at or below it is accepted with `snap` (+`snap_dx`/`snap_dy`) | reanchor (`teleport_reanchor`) / accept+`snap` / reject (`Teleport`) |
//!
//! Non-movers on the channel (short `07,52` len-6/10) are NOT
//! movers: [`parse_report`] returns `ParseFail::Passthrough` and they pass
//! through undecoded.
//!
//! ## Meters-first rule
//! All scheduler dials stay frozen. [`Meters`] records queue depth, drain
//! count, authority-lag histogram, felt-stutter markers, W-residuals and
//! HP-divergence. Lag is a METERED parameter only: [`MovementModel::record_lag`]
//! / [`StepObs::lag_frames`] record histogram values; nothing tunes from them.
//! No dial moves without a meter reading first.
//!
//! ## Gates
//! Doorways are directed gates (heading + plane crossing), NEVER radial
//! distance: zigzag bursts straddling a circle misfire under a radial model
//! (filed issue), so radial checks are rejected by construction — there is no
//! radius anywhere in this module. A gate fires iff the movement segment
//! intersects the gate segment AND the heading has a positive component along
//! the gate normal (`dot(curr−prev, n) > 0`). Pure strafe parallel to the gate
//! (`dot == 0`), same-side passes, and reverse crossings do not fire.

use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Wire + tuning constants (frozen dials; meters authorize future moves)
// ---------------------------------------------------------------------------

/// c2s mover opcode.
pub const OP_C2S_MOVER: [u8; 2] = [0x07, 0x34];
/// s2c authority opcode.
pub const OP_S2C_AUTH: [u8; 2] = [0x07, 0x35];
/// Mover header: `[07 34][ent u32][n u8]`.
pub const MOVER_HEADER_LEN: usize = 7;
/// One waypoint record: `[p u8][X i32][Y i32][W i32-bits]`.
pub const WAYPOINT_LEN: usize = 13;
/// Authority tail: foot (5B) + `06`.
pub const AUTH_TAIL_LEN: usize = 6;

/// Authority foot prefix. E1-ASSUMPTION (avatar entity only, high byte 0x40):
/// `02 ++ HP_LE(4B)`. Mid bytes `00 ec 09 00` = `0x0009EC00` = 650240.
pub const AUTH_FOOT: [u8; 5] = [0x02, 0x00, 0xEC, 0x09, 0x00];
/// Authority frame terminator (bare-`0706` shape never changes).
pub const AUTH_END: u8 = 0x06;

/// HP store default: 2540.0 HP × 256 = 650240 raw (SIM-004 open; damage-drop
/// behaviour is acceptance-only future work, never modelled here).
pub const HP_DEFAULT_RAW: u32 = 2540 * 256;

/// Heightfield acceptance band, raw units (`transfer-003-fit.md` tolerance).
/// UNKNOWN-precision fallback shared by W-match, bbox margin and lookup radius.
pub const W_BAND_RAW: i64 = 2500;
/// Walkable margin around the bbox (= session portal band). Outside
/// bbox+margin = reject, never clamp (MOVE-002 open: bbox only, no polygons).
pub const BBOX_MARGIN_RAW: i64 = 2500;
/// Heightfield lookup radius: nearest sample within this X/Y distance, else
/// carry input W + flag `w_unmapped`.
pub const LOOKUP_RADIUS_RAW: i64 = 2500;
/// Speed cap per waypoint step: straight-walk quantum 502 raw/step @~30 wp/s.
/// Exact cap UNKNOWN (MOVE-001; COMBAT-SPEED-001 axis). Multi-tick first legs
/// scale as `502*dt_ticks`. The current slice fail-closes (rejects); clamp-and-step is a dial
/// move requiring meter authorization, not implemented.
pub const SPEED_CAP_RAW_PER_STEP: i64 = 502;
/// W-settle band for in-place legs (ANCHOR1 live: client standing perfectly
/// still at (46264,149667) reports W gliding ~500/leg, burst
/// W=[69216,68713,68210,67707]). In-place (`dX==0,dY==0`) legs with
/// `|dW| <= W_SETTLE_MAX_RAW` accept with the `w_settle` flag (+`w_settles`
/// meter) instead of rejecting as `VerticalEvent`. 1024 = 2x the observed
/// ~500/leg glide: covers it with margin while staying an order of magnitude
/// below real height steps, so true vertical anomalies still reject.
pub const W_SETTLE_MAX_RAW: i64 = 1024;
/// Snap-jog ceiling (ANCHOR1 live: post-halt client continued with a
/// ~6156-dist leg carrying a large W residual — the recurring ±6144
/// grid-snap family). V8 legs with dist in `(SPEED_CAP*8, SNAP_MAX]` that
/// would otherwise hit the plane-shift `Reject(Teleport)` accept instead
/// with the `snap` flag (+`snap_dx`/`snap_dy` leg-vector meters). 12288 = 2x
/// the 6144 grid quantum: covers the observed 6156 with margin while staying
/// well below [`TELEPORT_THRESHOLD_RAW`], so real teleports still reanchor
/// and larger plane-shifts still reject.
pub const SNAP_MAX_RAW: i64 = 12_288;
/// Teleport shape: single-leg jump `Δ ≫ cap` is reanchored as a new epoch
/// anchor (`Disposition::Reanchor`, reason `teleport_reanchor`), never
/// executed in place. Wire layout UNKNOWN (SIM-005, M005 capture needed).
pub const TELEPORT_THRESHOLD_RAW: i64 = 20_000;

// ---------------------------------------------------------------------------
// Content types (contract: townston-slice.json)
// ---------------------------------------------------------------------------

/// One heightfield sample: raw (X, Y) → W (x256 fixed point, bits preserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeightSample {
    pub x: i32,
    pub y: i32,
    pub w: i32,
}

/// Walkable bounds `[xmin, ymin, xmax, ymax]` (contract order assumption;
/// constructor normalizes inverted pairs defensively but operators must ship
/// the documented order — order ambiguity is OPEN under MOVE-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub xmin: i32,
    pub ymin: i32,
    pub xmax: i32,
    pub ymax: i32,
}

impl BBox {
    pub fn new(xmin: i32, ymin: i32, xmax: i32, ymax: i32) -> Self {
        BBox {
            xmin: xmin.min(xmax),
            ymin: ymin.min(ymax),
            xmax: xmin.max(xmax),
            ymax: ymin.max(ymax),
        }
    }

    /// Inside bbox expanded by [`BBOX_MARGIN_RAW`] (V2).
    pub fn contains_with_margin(&self, x: i32, y: i32) -> bool {
        let (x, y) = (x as i64, y as i64);
        x >= self.xmin as i64 - BBOX_MARGIN_RAW
            && x <= self.xmax as i64 + BBOX_MARGIN_RAW
            && y >= self.ymin as i64 - BBOX_MARGIN_RAW
            && y <= self.ymax as i64 + BBOX_MARGIN_RAW
    }
}

/// Spawn triple: X, Y (+ W resolved from the heightfield at spawn, else
/// carried W from the movement fixture — resolved via [`MovementModel::spawn_w`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawn {
    pub x: i32,
    pub y: i32,
    pub w: i32,
}

/// Directed gate: segment `a→b` plus the blocked travel direction `n`.
///
/// Fires iff the movement segment intersects the gate segment AND
/// `dot(curr−prev, n) > 0`. There is deliberately NO radius: radial
/// arm/re-enter checks are rejected per the filed strafe/straddle issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    pub ax: i32,
    pub ay: i32,
    pub bx: i32,
    pub by: i32,
    /// Blocked direction (sign matters, magnitude does not).
    pub nx: i64,
    pub ny: i64,
}

impl Gate {
    pub fn new(ax: i32, ay: i32, bx: i32, by: i32, nx: i64, ny: i64) -> Self {
        Gate {
            ax,
            ay,
            bx,
            by,
            nx,
            ny,
        }
    }

    /// 4-element contract entries `[ax,ay,bx,by]` default to the right-hand
    /// normal `(dy,−dx)` of the segment direction. 6-element entries carry an
    /// explicit normal. OPEN (MOVE-002/M003): orientation vs live faces.
    pub fn from_segment(ax: i32, ay: i32, bx: i32, by: i32) -> Self {
        let (dx, dy) = (bx as i64 - ax as i64, by as i64 - ay as i64);
        Gate::new(ax, ay, bx, by, dy, -dx)
    }

    /// Directed crossing test (integer math only, no floats).
    pub fn crossed_by(&self, px: i32, py: i32, cx: i32, cy: i32) -> bool {
        if !segments_intersect(
            px as i64,
            py as i64,
            cx as i64,
            cy as i64,
            self.ax as i64,
            self.ay as i64,
            self.bx as i64,
            self.by as i64,
        ) {
            return false;
        }
        let (dx, dy) = (cx as i64 - px as i64, cy as i64 - py as i64);
        dx * self.nx + dy * self.ny > 0
    }
}

fn orient(ax: i64, ay: i64, bx: i64, by: i64, cx: i64, cy: i64) -> i64 {
    (bx - ax) * (cy - ay) - (by - ay) * (cx - ax)
}

fn on_segment(ax: i64, ay: i64, bx: i64, by: i64, cx: i64, cy: i64) -> bool {
    cx >= ax.min(bx) && cx <= ax.max(bx) && cy >= ay.min(by) && cy <= ay.max(by)
}

/// Inclusive segment intersection (touching counts as crossing: halt at the
/// patch edge, fail-closed to reject — never to silence).
#[allow(clippy::too_many_arguments)]
pub fn segments_intersect(
    p1x: i64,
    p1y: i64,
    p2x: i64,
    p2y: i64,
    p3x: i64,
    p3y: i64,
    p4x: i64,
    p4y: i64,
) -> bool {
    let o1 = orient(p1x, p1y, p2x, p2y, p3x, p3y);
    let o2 = orient(p1x, p1y, p2x, p2y, p4x, p4y);
    let o3 = orient(p3x, p3y, p4x, p4y, p1x, p1y);
    let o4 = orient(p3x, p3y, p4x, p4y, p2x, p2y);
    ((o1 > 0 && o2 < 0) || (o1 < 0 && o2 > 0)) && ((o3 > 0 && o4 < 0) || (o3 < 0 && o4 > 0))
        || (o1 == 0 && on_segment(p1x, p1y, p2x, p2y, p3x, p3y))
        || (o2 == 0 && on_segment(p1x, p1y, p2x, p2y, p4x, p4y))
        || (o3 == 0 && on_segment(p3x, p3y, p4x, p4y, p1x, p1y))
        || (o4 == 0 && on_segment(p3x, p3y, p4x, p4y, p2x, p2y))
}

// ---------------------------------------------------------------------------
// Report (parsed 07,34) + validation verdicts V1–V8
// ---------------------------------------------------------------------------

/// One input waypoint: p-flag + raw (X, Y, W). W bits match the wire u32
/// reinterpreted as i32 (terrain W can set the high bit; residual math is i64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waypoint {
    pub p: u8,
    pub x: i32,
    pub y: i32,
    pub w: i32,
}

/// Parsed c2s `07,34` report. Grammar already proven by [`parse_report`] (V1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub entity: u32,
    pub waypoints: Vec<Waypoint>,
    /// Full raw length for the trailing-bytes meter. Trailing bytes are
    /// tolerated by the reference prefix rule, but never silently stripped.
    pub raw_len: usize,
}

/// Raw-frame classification failure (V1 / PROTO-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseFail {
    /// Not a mover at all (e.g. short `07,52` len-6/10): pass through
    /// undecoded, never counted as a drop.
    Passthrough,
    /// Mover-shaped but malformed (`n==0`, short body): drop + `grammar_drops`.
    GrammarDrop,
}

/// Parse a raw c2s frame into a [`Report`]. Layout:
/// `[07 34][ent u32 LE][n u8][p u8,X i32 LE,Y i32 LE,W bits LE]xN`.
/// Never panics.
pub fn parse_report(raw: &[u8]) -> Result<Report, ParseFail> {
    if raw.len() < MOVER_HEADER_LEN || raw[0] != 0x07 || raw[1] != 0x34 {
        return Err(ParseFail::Passthrough);
    }
    let n = raw[6] as usize;
    if n < 1 || raw.len() < MOVER_HEADER_LEN + WAYPOINT_LEN * n {
        return Err(ParseFail::GrammarDrop);
    }
    let entity = u32::from_le_bytes([raw[2], raw[3], raw[4], raw[5]]);
    let mut waypoints = Vec::with_capacity(n);
    for i in 0..n {
        let off = MOVER_HEADER_LEN + WAYPOINT_LEN * i;
        waypoints.push(Waypoint {
            p: raw[off],
            x: i32::from_le_bytes([raw[off + 1], raw[off + 2], raw[off + 3], raw[off + 4]]),
            y: i32::from_le_bytes([raw[off + 5], raw[off + 6], raw[off + 7], raw[off + 8]]),
            w: i32::from_le_bytes([raw[off + 9], raw[off + 10], raw[off + 11], raw[off + 12]]),
        });
    }
    Ok(Report {
        entity,
        waypoints,
        raw_len: raw.len(),
    })
}

/// Trusted caller state for validation: zone avatar id + last authoritative
/// position + ticks elapsed since it (V3 `dist/dt`; dt>=1, no clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrevState {
    pub avatar_entity: u32,
    pub x: i32,
    pub y: i32,
    pub dt_ticks: u64,
}

/// Terminal disposition of a validated report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Accept,
    /// Reanchor: teleport-shaped jump accepted as a new epoch anchor (V8).
    /// The caller advances `prev` to the post-jump position and steps the
    /// report through [`MovementModel::step_then_emit`] exactly like an
    /// accept; only the disposition label and the portal-vector meters
    /// differ.
    Reanchor(ReanchorReason),
    Reject(RejectReason),
    /// Relatch: entity differs from the latched avatar BUT every other check
    /// passed (grammar, p01, bbox+margin, gates, vertical/settle, speed incl.
    /// the teleport-shape mapping, snap). Steps exactly like an accept (the
    /// caller advances `prev` to the reported waypoints AND switches its
    /// latched entity to the new id). The interleaving guard — alternating
    /// entities burst-to-burst is unsupported and must stay reject — is the
    /// CALLER's job: `validate` judges one report at a time and has no burst
    /// history.
    Relatch(RelatchReason),
    /// V1 only: malformed mover, dropped before any rule runs.
    Drop(DropReason),
}

/// Reanchor reasons: teleport-shaped jumps accepted as new epoch anchors.
/// The caller advances `prev` past the jump; the portal vector is metered
/// (`teleport_reanchors`, `last_teleport_dx/dy`) for portal mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReanchorReason {
    /// V8: single-leg jump beyond [`TELEPORT_THRESHOLD_RAW`] with entity,
    /// bbox+margin, p01 and speed-cap on non-teleport legs all valid.
    TeleportReanchor,
}

impl ReanchorReason {
    /// Wire-stable reason string (mirrors the reference parity check's
    /// `teleport_reanchor`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ReanchorReason::TeleportReanchor => "teleport_reanchor",
        }
    }
}

/// Reject reasons (fail-closed: reverts to an echo for that leg per the
/// fail-closed rule
/// Shape C — the sim itself emits nothing past a halt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// V2: entity != zone avatar COMBINED with any other defect (grammar
    /// aside: malformed movers drop before any rule runs). A clean burst
    /// from an unexpected entity relatches instead (`Disposition::Relatch`).
    EntityMismatch,
    /// V2: waypoint outside bbox+margin (MOVE-002 open: reject, never clamp).
    Oob,
    /// V2/collision: directed gate crossed (strafe-aware; never radial).
    WallClip,
    /// V3: per-leg `dist/dt` over cap (MOVE-001); flag `speed_clamp` set.
    SpeedHack,
    /// V4: `dW!=0` at `dX=dY=0` beyond [`W_SETTLE_MAX_RAW`] — vertical-only
    /// anomaly, never modelled. In-place `0 < |dW| <= W_SETTLE_MAX_RAW`
    /// settles instead (flag `w_settle`, meter `w_settles`).
    VerticalEvent,
    /// V8: W-plane shift paired with a medium jump above [`SNAP_MAX_RAW`]
    /// (SIM-005 open). Jumps beyond [`TELEPORT_THRESHOLD_RAW`] reanchor
    /// instead (`Disposition::Reanchor`); plane-shifts at/below `SNAP_MAX_RAW`
    /// accept with the `snap` flag; a threshold jump plus a speed defect
    /// elsewhere falls through to `SpeedHack`.
    Teleport,
    /// V5: p01 not last-in-msg or not duplicating prev (X,Y,W).
    P01Violation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Grammar,
}

/// Relatch reasons: entity switches accepted as a re-latch onto the new id.
/// The caller advances `prev` past the burst AND switches its latched entity;
/// alternating entities burst-to-burst stays reject by the caller (see
/// [`Disposition::Relatch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelatchReason {
    /// V2: entity != expected avatar BUT grammar, p01, bbox+margin, gates,
    /// vertical/settle, speed (incl. the teleport-shape mapping) and snap
    /// all pass. Carries the latched (old) and reported (new) entity ids;
    /// metered as `entity_switches` + `last_entity_old`/`last_entity_new`.
    EntitySwitch {
        /// Expected avatar entity (the model's zone avatar id).
        old_entity: u32,
        /// Reported entity the caller must latch onto.
        new_entity: u32,
    },
}

impl RelatchReason {
    /// Wire-stable reason string (mirrors the reference parity check's
    /// `entity_switch`).
    pub fn as_str(&self) -> &'static str {
        match self {
            RelatchReason::EntitySwitch { .. } => "entity_switch",
        }
    }
}

/// Non-terminal flags carried on the verdict (metered even on rejects).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub speed_clamp: bool,
    pub w_mismatch: bool,
    pub w_unmapped: bool,
    /// V4 settle: an in-place leg carried `0 < |dW| <= W_SETTLE_MAX_RAW`
    /// (mirrors the reference parity check's `w_settle`).
    pub w_settle: bool,
    /// V8 snap-jog: a `(SPEED_CAP*8, SNAP_MAX]` leg with a W-plane shift
    /// accepted instead of rejected (mirrors the mirror's `snap`).
    pub snap: bool,
}

/// Per-waypoint verdict detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegVerdict {
    /// Waypoint index in the report.
    pub index: usize,
    pub in_bounds: bool,
    /// Nearest-table distance (None = unmapped, input W carried).
    pub table_dist: Option<i64>,
    /// `|W_in − W_table|` (None when unmapped).
    pub w_residual: Option<i64>,
    pub gate_hit: bool,
}

/// Per-rule verdicts V1–V8 for one report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameVerdict {
    pub disposition: Disposition,
    pub v1_grammar_ok: bool,
    pub v2_plausible: bool,
    pub v3_speed_ok: bool,
    pub v4_height_ok: bool,
    pub v5_pflags_ok: bool,
    /// V6 informational pass (input carries no foot/HP; HP never from input).
    pub v6_input_foot_ignored: bool,
    /// V7 informational pass (FIFO held by caller; depth metered, §SIM-003).
    pub v7_sequenced: bool,
    pub v8_epoch_ok: bool,
    pub flags: Flags,
    pub legs: Vec<LegVerdict>,
    /// Trailing bytes beyond `7+13n` (tolerated, metered).
    pub trailing_bytes: usize,
}

impl FrameVerdict {
    pub fn accepted(&self) -> bool {
        self.disposition == Disposition::Accept
    }

    /// Steppable through [`MovementModel::step_then_emit`]: accepts,
    /// reanchors AND relatches (a reanchor steps to the new-location
    /// waypoints exactly like an accept; a relatch steps to the reported
    /// waypoints while the caller additionally switches its latched entity
    /// to the new id — only the disposition label and meters differ).
    /// Rejects and drops must not step (fail-closed: avatar frozen).
    pub fn steppable(&self) -> bool {
        matches!(
            self.disposition,
            Disposition::Accept | Disposition::Reanchor(_) | Disposition::Relatch(_)
        )
    }
}

// ---------------------------------------------------------------------------
// Meters (read first, tune never)
// ---------------------------------------------------------------------------

/// Scheduler telemetry. Scheduler dials are frozen; this struct
/// only records. Targets live in the plan (depth flat, lag inside the live
/// ~10-frame window, zero drops, `w_mismatch` < 1%); nothing here acts on them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meters {
    /// Per-validate burst depth samples (SIM-003 open: metered, no catch-up).
    pub queue_depth_samples: Vec<usize>,
    /// Per-tick drain counts (target: flat across run speeds).
    pub drain_counts: Vec<usize>,
    /// Authority-lag histogram in frames (target: inside live ~10-frame
    /// window, zero drops). Recorded only — lag policy is a metered parameter.
    pub lag_histogram: Vec<u32>,
    /// Felt-stutter markers (opaque labels from the edge harness).
    pub stutter_markers: Vec<String>,
    /// Per-waypoint `|W_in − W_table|` residuals (unmapped legs skipped).
    pub w_residuals: Vec<i64>,
    /// HP-divergence counter (stays 0 on the current movement slice).
    pub hp_divergences: u64,
    pub grammar_drops: u64,
    pub oob_rejects: u64,
    pub wall_clips: u64,
    pub speed_clamps: u64,
    pub w_mismatches: u64,
    pub w_unmapped: u64,
    pub p01_violations: u64,
    pub epoch_rejects: u64,
    pub vertical_events: u64,
    /// Teleport re-anchors accepted as new epoch anchors (V8): count plus
    /// the last portal vector (incoming prev avatar pos → first post-jump
    /// waypoint) for portal mapping. `epoch_rejects` above still fires on
    /// the W-plane-shift + medium-jump branch.
    pub teleport_reanchors: u64,
    pub last_teleport_dx: i64,
    pub last_teleport_dy: i64,
    /// W-settle carve-out (V4): in-place `0 < |dW| <= W_SETTLE_MAX_RAW` legs
    /// accepted instead of rejected as `VerticalEvent` (per-report count;
    /// the per-verdict signal is `Flags::w_settle`).
    pub w_settles: u64,
    /// Snap-jog accept (V8): last accepted snap leg vector (reported
    /// waypoint − previous position). `(0,0)` initial = none yet (snap legs
    /// always carry `dist > SPEED_CAP*8`, so a zero vector is unambiguous).
    /// The per-verdict signal is `Flags::snap`.
    pub snap_dx: i64,
    pub snap_dy: i64,
    /// Entity relatches (V2): count plus the last latched (old) and
    /// reported (new) entity ids. Bumped only when every non-entity check
    /// passes; entity mismatches combined with any other defect stay
    /// `Reject(EntityMismatch)` (counted on `oob_rejects`, as before).
    pub entity_switches: u64,
    pub last_entity_old: u32,
    pub last_entity_new: u32,
}

impl Meters {
    /// Record queue depth before a tick drains (V7: metered, never
    /// fast-forwarded).
    pub fn record_queue_depth(&mut self, depth: usize) {
        self.queue_depth_samples.push(depth);
    }

    /// Record authorities drained this tick.
    pub fn record_drain(&mut self, n: usize) {
        self.drain_counts.push(n);
    }

    /// Record authority lag in frames. METERED ONLY: no tuning reads this.
    pub fn record_lag(&mut self, lag_frames: u32) {
        self.lag_histogram.push(lag_frames);
    }

    /// Record a felt-stutter marker from the edge harness (calibrates meters).
    pub fn record_stutter(&mut self, marker: &str) {
        self.stutter_markers.push(marker.to_string());
    }

    /// Record a per-waypoint W residual.
    pub fn record_w_residual(&mut self, residual: i64) {
        self.w_residuals.push(residual);
    }
}

// ---------------------------------------------------------------------------
// MovementModel
// ---------------------------------------------------------------------------

/// Authoritative movement state over one Townston patch: heightfield table +
/// bbox + spawn (+ directed gates + single-entry HP store for the one avatar).
#[derive(Debug, Clone)]
pub struct MovementModel {
    /// Heightfield `f(X,Y)=W` lookup table (replaceable; wire shape never changes).
    pub table: Vec<HeightSample>,
    pub bbox: BBox,
    pub spawn: Spawn,
    /// Directed doorway gates (never radial). Empty until pathmap work
    /// (MOVE-001/002, M003) ships geometry.
    pub gates: Vec<Gate>,
    /// Zone avatar entity id (V2 plausibility).
    pub avatar_entity: u32,
    /// Per-entity HP store, single-entry for the one-avatar movement scope.
    /// Default 2540.0; HP never taken from input (V6).
    pub hp_store: u32,
    /// Meters (record first; dials frozen).
    pub meters: Meters,
    /// FIFO report backlog (V7: queue; depth metered, never fast-forwarded).
    pub backlog: VecDeque<Report>,
}

impl MovementModel {
    pub fn new(table: Vec<HeightSample>, bbox: BBox, spawn: Spawn, avatar_entity: u32) -> Self {
        MovementModel {
            table,
            bbox,
            spawn,
            gates: Vec::new(),
            avatar_entity,
            hp_store: HP_DEFAULT_RAW,
            meters: Meters::default(),
            backlog: VecDeque::new(),
        }
    }

    pub fn with_gates(mut self, gates: Vec<Gate>) -> Self {
        self.gates = gates;
        self
    }

    /// Load the `townston-slice.json` contract from its text (no disk I/O
    /// inside `drf-sim`; the edge reads the file). std-only: hand-rolled
    /// parser, integers only (fraction digits skipped — no floats in sim).
    pub fn from_slice_json(text: &str, avatar_entity: u32) -> Result<Self, String> {
        let v = parse_json(text)?;
        let obj = match v {
            JVal::Obj(kv) => kv,
            _ => return Err("slice root must be an object".to_string()),
        };
        let get = |k: &str| {
            obj.iter()
                .find(|(key, _)| key == k)
                .map(|(_, val)| val)
                .ok_or_else(|| format!("slice missing key {:?}", k))
        };
        // heightfield: [[x,y,w], ...]
        let hf = match get("heightfield")? {
            JVal::Arr(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for (i, r) in rows.iter().enumerate() {
                    match r {
                        JVal::Arr(t) if t.len() == 3 => {
                            out.push(HeightSample {
                                x: jnum(&t[0])
                                    .ok_or_else(|| format!("heightfield[{i}][0] not an integer"))?
                                    as i32,
                                y: jnum(&t[1])
                                    .ok_or_else(|| format!("heightfield[{i}][1] not an integer"))?
                                    as i32,
                                w: jnum(&t[2])
                                    .ok_or_else(|| format!("heightfield[{i}][2] not an integer"))?
                                    as i32,
                            });
                        }
                        _ => return Err(format!("heightfield[{i}] must be [x,y,w]")),
                    }
                }
                out
            }
            _ => return Err("heightfield must be an array".to_string()),
        };
        if hf.is_empty() {
            return Err("heightfield must be non-empty".to_string());
        }
        // bbox: [xmin,ymin,xmax,ymax]
        let bb = match get("bbox")? {
            JVal::Arr(t) if t.len() == 4 => {
                let n = |i: usize| {
                    jnum(&t[i])
                        .ok_or_else(|| format!("bbox[{i}] not an integer"))
                        .map(|v| v as i32)
                };
                BBox::new(n(0)?, n(1)?, n(2)?, n(3)?)
            }
            _ => return Err("bbox must be [xmin,ymin,xmax,ymax]".to_string()),
        };
        // spawn: [x,y] | [x,y,w] (w carried as initial avatar height).
        let sp = match get("spawn")? {
            JVal::Arr(t) if t.len() == 2 || t.len() == 3 => Spawn {
                x: jnum(&t[0]).ok_or_else(|| "spawn[0] not an integer".to_string())? as i32,
                y: jnum(&t[1]).ok_or_else(|| "spawn[1] not an integer".to_string())? as i32,
                w: if t.len() == 3 {
                    jnum(&t[2]).ok_or_else(|| "spawn[2] not an integer".to_string())? as i32
                } else {
                    0
                },
            },
            _ => return Err("spawn must be [x,y] or [x,y,w]".to_string()),
        };
        // blocked: [[ax,ay,bx,by] | [ax,ay,bx,by,nx,ny], ...] (may be empty)
        let mut gates = Vec::new();
        if let Ok(b) = get("blocked") {
            match b {
                JVal::Arr(rows) => {
                    for (i, r) in rows.iter().enumerate() {
                        match r {
                            JVal::Arr(t) if t.len() == 4 => {
                                let n = |j: usize| jnum(&t[j]).unwrap_or(0) as i32;
                                gates.push(Gate::from_segment(n(0), n(1), n(2), n(3)));
                            }
                            JVal::Arr(t) if t.len() == 6 => {
                                let n = |j: usize| jnum(&t[j]).unwrap_or(0);
                                gates.push(Gate::new(
                                    n(0) as i32,
                                    n(1) as i32,
                                    n(2) as i32,
                                    n(3) as i32,
                                    n(4),
                                    n(5),
                                ));
                            }
                            _ => {
                                return Err(format!(
                                    "blocked[{i}] must be [ax,ay,bx,by] or [ax,ay,bx,by,nx,ny]"
                                ))
                            }
                        }
                    }
                }
                _ => return Err("blocked must be an array".to_string()),
            }
        }
        Ok(MovementModel::new(hf, bb, sp, avatar_entity).with_gates(gates))
    }

    /// Nearest heightfield sample by X/Y euclidean distance² (integer math).
    fn nearest(&self, x: i32, y: i32) -> Option<(HeightSample, i64)> {
        let mut best: Option<(HeightSample, i64)> = None;
        for s in &self.table {
            let dx = s.x as i64 - x as i64;
            let dy = s.y as i64 - y as i64;
            let d2 = dx * dx + dy * dy;
            match best {
                Some((_, bd)) if bd <= d2 => {}
                _ => best = Some((*s, d2)),
            }
        }
        best
    }

    /// Heightfield lookup `W=table(X,Y)`. Returns `(w, mapped)`: nearest
    /// sample within [`LOOKUP_RADIUS_RAW`]; if none, carry `w_in` and flag
    /// `w_unmapped` (residual check per waypoint skips unmapped legs).
    pub fn lookup_w(&self, x: i32, y: i32, w_in: i32) -> (i32, bool) {
        match self.nearest(x, y) {
            Some((s, d2)) if d2 <= LOOKUP_RADIUS_RAW * LOOKUP_RADIUS_RAW => (s.w, true),
            _ => (w_in, false),
        }
    }

    /// Authoritative W at spawn (table value, else carried 0 + unmapped).
    pub fn spawn_w(&self) -> (i32, bool) {
        self.lookup_w(self.spawn.x, self.spawn.y, 0)
    }

    /// V7: enqueue a parsed report into the FIFO backlog (resume bursts are
    /// expected `n=1`; backlog only metered, never fast-forwarded — SIM-003).
    pub fn enqueue(&mut self, report: Report) {
        self.meters.record_queue_depth(self.backlog.len() + 1);
        self.backlog.push_back(report);
    }

    /// Lag policy, metered parameter: record authority-lag histogram values.
    /// This function tunes nothing by construction.
    pub fn record_lag(&mut self, lag_frames: u32) {
        self.meters.record_lag(lag_frames);
    }

    /// Validate one parsed report → per-rule verdicts V1–V8.
    ///
    /// Check order: V1 (proven by parse) → V5 (p-shape) → V2 bounds +
    /// directed gates (entity is DEFERRED, see below) → vertical/settle →
    /// V8 (teleport/snap) → V3 (speed) → V4 (W band) → entity decision.
    /// First failure wins; flags stay metered on every path.
    ///
    /// Entity handling: `report.entity` is compared against the latched
    /// avatar up front, but a mismatch only relatches when every other check
    /// passes; a mismatch combined with any other defect returns
    /// `Reject(EntityMismatch)` (masking the underlying defect, counted on
    /// `oob_rejects` as before — the specific defect's meter does NOT fire).
    /// A clean mismatched burst returns `Disposition::Relatch` (metered as
    /// `entity_switches` + `last_entity_old`/`last_entity_new`).
    pub fn validate(&mut self, report: &Report, prev: &PrevState) -> FrameVerdict {
        self.meters.record_queue_depth(report.waypoints.len());
        let trailing = report
            .raw_len
            .saturating_sub(MOVER_HEADER_LEN + WAYPOINT_LEN * report.waypoints.len());
        let mut legs = Vec::with_capacity(report.waypoints.len());
        let mut flags = Flags::default();

        // Per-leg shared geometry: bounds, gate hits, table residuals.
        let mut px = prev.x;
        let mut py = prev.y;
        for (i, wp) in report.waypoints.iter().enumerate() {
            let in_bounds = self.bbox.contains_with_margin(wp.x, wp.y);
            let mut gate_hit = false;
            for g in &self.gates {
                if g.crossed_by(px, py, wp.x, wp.y) {
                    gate_hit = true;
                    break;
                }
            }
            let (table_w, mapped) = self.lookup_w(wp.x, wp.y, wp.w);
            let (dist, residual) = if mapped {
                let d2 = nearest_dist2(&self.table, wp.x, wp.y);
                let r = (wp.w as i64 - table_w as i64).abs();
                self.meters.record_w_residual(r);
                (Some(d2), Some(r))
            } else {
                flags.w_unmapped = true;
                (None, None)
            };
            let _ = dist;
            legs.push(LegVerdict {
                index: i,
                in_bounds,
                table_dist: dist,
                w_residual: residual,
                gate_hit,
            });
            px = wp.x;
            py = wp.y;
        }

        // V5: p01 must be last-in-msg and duplicate prev (X,Y,W); p02/p03 are
        // accepted anywhere and never rejected on position alone. Unknown p
        // values are accepted (p03 bitfield PROBABLE; only p01 shape rejects).
        let mut v5_ok = true;
        let n = report.waypoints.len();
        for (i, wp) in report.waypoints.iter().enumerate() {
            if wp.p == 0x01 {
                let last = i + 1 == n;
                let dup = if i > 0 {
                    let q = report.waypoints[i - 1];
                    wp.x == q.x && wp.y == q.y && wp.w == q.w
                } else {
                    // Single-waypoint arrival: prev is out-of-frame history
                    // (resume bursts are n=1, BUF1); metered, not rejected.
                    true
                };
                if !last || !dup {
                    v5_ok = false;
                    break;
                }
            }
        }
        if !v5_ok {
            // Entity mismatch masks the defect (relatch requires a clean
            // burst): count on the entity meter, not p01's.
            if report.entity != prev.avatar_entity || report.entity != self.avatar_entity {
                self.meters.oob_rejects += 1;
                return FrameVerdict {
                    disposition: Disposition::Reject(RejectReason::EntityMismatch),
                    v1_grammar_ok: true,
                    v2_plausible: false,
                    v3_speed_ok: true,
                    v4_height_ok: true,
                    v5_pflags_ok: false,
                    v6_input_foot_ignored: true,
                    v7_sequenced: true,
                    v8_epoch_ok: true,
                    flags,
                    legs,
                    trailing_bytes: trailing,
                };
            }
            self.meters.p01_violations += 1;
            return FrameVerdict {
                disposition: Disposition::Reject(RejectReason::P01Violation),
                v1_grammar_ok: true,
                v2_plausible: false,
                v3_speed_ok: true,
                v4_height_ok: true,
                v5_pflags_ok: false,
                v6_input_foot_ignored: true,
                v7_sequenced: true,
                v8_epoch_ok: true,
                flags,
                legs,
                trailing_bytes: trailing,
            };
        }

        // V2: entity latch check, DEFERRED. A mismatch relatches iff every
        // other check below passes; each reject site below maps back to
        // `EntityMismatch` when this is set (see the validate doc comment).
        let entity_mismatch =
            report.entity != prev.avatar_entity || report.entity != self.avatar_entity;
        // Canonical "old" id for the relatch payload/meters: the model's
        // zone avatar (normally equal to the caller's latched prev).
        let entity_old = self.avatar_entity;

        // Helper: the masked verdict for entity+mismatch+defect. The
        // specific defect's meter must NOT have fired on these paths.
        macro_rules! entity_mismatch_verdict {
            () => {
                FrameVerdict {
                    disposition: Disposition::Reject(RejectReason::EntityMismatch),
                    v1_grammar_ok: true,
                    v2_plausible: false,
                    v3_speed_ok: true,
                    v4_height_ok: true,
                    v5_pflags_ok: true,
                    v6_input_foot_ignored: true,
                    v7_sequenced: true,
                    v8_epoch_ok: true,
                    flags,
                    legs,
                    trailing_bytes: trailing,
                }
            };
        }

        // Helper: relatch verdict wrapping an otherwise-clean outcome.
        // `height_ok`/`epoch_ok` mirror the would-be verdict; geometry
        // meters (teleport/snap vectors, w_mismatch) were already recorded
        // on their paths so portal mapping survives the entity switch.
        macro_rules! relatch_verdict {
            ($height_ok:expr, $epoch_ok:expr) => {{
                self.meters.entity_switches += 1;
                self.meters.last_entity_old = entity_old;
                self.meters.last_entity_new = report.entity;
                FrameVerdict {
                    disposition: Disposition::Relatch(RelatchReason::EntitySwitch {
                        old_entity: entity_old,
                        new_entity: report.entity,
                    }),
                    v1_grammar_ok: true,
                    v2_plausible: false,
                    v3_speed_ok: true,
                    v4_height_ok: $height_ok,
                    v5_pflags_ok: true,
                    v6_input_foot_ignored: true,
                    v7_sequenced: true,
                    v8_epoch_ok: $epoch_ok,
                    flags,
                    legs,
                    trailing_bytes: trailing,
                }
            }};
        }

        // V2: bounds, then directed-gate collision.
        if legs.iter().any(|l| !l.in_bounds) {
            if entity_mismatch {
                self.meters.oob_rejects += 1;
                return entity_mismatch_verdict!();
            }
            self.meters.oob_rejects += 1;
            return FrameVerdict {
                disposition: Disposition::Reject(RejectReason::Oob),
                v1_grammar_ok: true,
                v2_plausible: false,
                v3_speed_ok: true,
                v4_height_ok: true,
                v5_pflags_ok: true,
                v6_input_foot_ignored: true,
                v7_sequenced: true,
                v8_epoch_ok: true,
                flags,
                legs,
                trailing_bytes: trailing,
            };
        }
        if legs.iter().any(|l| l.gate_hit) {
            if entity_mismatch {
                self.meters.oob_rejects += 1;
                return entity_mismatch_verdict!();
            }
            self.meters.wall_clips += 1;
            return FrameVerdict {
                disposition: Disposition::Reject(RejectReason::WallClip),
                v1_grammar_ok: true,
                v2_plausible: false,
                v3_speed_ok: true,
                v4_height_ok: true,
                v5_pflags_ok: true,
                v6_input_foot_ignored: true,
                v7_sequenced: true,
                v8_epoch_ok: true,
                flags,
                legs,
                trailing_bytes: trailing,
            };
        }

        // Vertical-only anomaly with a W-settle carve-out: `dW!=0` at
        // `dX=dY=0` is rejected, never modelled (marks1 f1511-1513, dun01a
        // f252-254 — out of patch by construction) — EXCEPT small in-place
        // glides `0 < |dW| <= W_SETTLE_MAX_RAW`, which accept with the
        // `w_settle` flag (+`w_settles` meter). Exact stillness (`dW==0`,
        // e.g. a p01 arrival duplicating prev) sets no flag. Larger
        // in-place `|dW|` stays `Reject(VerticalEvent)`.
        px = prev.x;
        py = prev.y;
        let mut pw = if report.waypoints.is_empty() {
            0
        } else {
            self.lookup_w(prev.x, prev.y, report.waypoints[0].w).0
        };
        let mut vertical = false;
        let mut settle_seen = false;
        for wp in &report.waypoints {
            if wp.x == px && wp.y == py && wp.w != pw {
                let dw = (wp.w as i64 - pw as i64).abs();
                if dw <= W_SETTLE_MAX_RAW {
                    // Settle glide: keep chaining W (the avatar is still,
                    // the height reading is drifting) and remember the
                    // flag for the accept path below.
                    settle_seen = true;
                    pw = wp.w;
                    continue;
                }
                vertical = true;
                break;
            }
            px = wp.x;
            py = wp.y;
            pw = wp.w;
        }
        // Metered once per report (like `vertical_events`), even if a later
        // rule rejects — the flag rides on every verdict below via `flags`.
        if settle_seen {
            flags.w_settle = true;
            self.meters.w_settles += 1;
        }
        if vertical {
            if entity_mismatch {
                self.meters.oob_rejects += 1;
                return entity_mismatch_verdict!();
            }
            self.meters.vertical_events += 1;
            return FrameVerdict {
                disposition: Disposition::Reject(RejectReason::VerticalEvent),
                v1_grammar_ok: true,
                v2_plausible: true,
                v3_speed_ok: true,
                v4_height_ok: false,
                v5_pflags_ok: true,
                v6_input_foot_ignored: true,
                v7_sequenced: true,
                v8_epoch_ok: true,
                flags,
                legs,
                trailing_bytes: trailing,
            };
        }

        // V8 teleport: sequential per-leg scan in report order (evaluation
        // order mirrors the reference parity check). ONLY single-leg jumps with
        // `Δ > TELEPORT_THRESHOLD` reanchor as a new epoch anchor (SIM-005
        // portal semantics): bounds, gates, vertical/settle and p01 already
        // passed above, and non-teleport legs must additionally pass the
        // speed cap. The second shape — a W-plane shift paired with a medium
        // jump — splits at `SNAP_MAX_RAW`: at/below it the leg is a grid-snap
        // jog and ACCEPTS with the `snap` flag (+`snap_dx`/`snap_dy`
        // leg-vector meters); above it stays `Reject(Teleport)`
        // (`epoch_rejects`), never reanchored. First trigger in scan order
        // wins, exactly like the mirror's per-leg return (so a snap accept
        // precedes the V3 speed check below, mirroring how the old reject
        // did). No epoch bytes exist in corpus; nothing is ACKed here.
        let mut teleport_leg = vec![false; report.waypoints.len()];
        let mut plane_shift = false;
        // Snap-jog accept candidate: (leg index, dx, dy) of the first
        // snap-shaped leg in scan order.
        let mut snap_leg: Option<(usize, i64, i64)> = None;
        let mut seen_jump = false;
        px = prev.x;
        py = prev.y;
        for (i, wp) in report.waypoints.iter().enumerate() {
            let dx = wp.x as i64 - px as i64;
            let dy = wp.y as i64 - py as i64;
            let dist2 = dx * dx + dy * dy;
            let w_jump = legs[i]
                .w_residual
                .map(|r| r > W_BAND_RAW * 4)
                .unwrap_or(false);
            if dist2 > TELEPORT_THRESHOLD_RAW * TELEPORT_THRESHOLD_RAW {
                teleport_leg[i] = true;
                seen_jump = true;
            } else if w_jump
                && dist2 > SPEED_CAP_RAW_PER_STEP * SPEED_CAP_RAW_PER_STEP * 64
                && !seen_jump
            {
                if dist2 <= SNAP_MAX_RAW * SNAP_MAX_RAW {
                    snap_leg = Some((i, dx, dy));
                    break;
                }
                plane_shift = true;
                break;
            }
            px = wp.x;
            py = wp.y;
        }
        // Snap-jog accept: the jog is over the walk cap by shape, so V3 is
        // bypassed exactly like a reanchor bypasses it for the jump leg
        // (V4 band flags are still computed, as on the accept path).
        if let Some((_, dx, dy)) = snap_leg {
            flags.snap = true;
            self.meters.snap_dx = dx;
            self.meters.snap_dy = dy;
            let mut height_ok = true;
            for l in &legs {
                if let Some(r) = l.w_residual {
                    if r > W_BAND_RAW {
                        height_ok = false;
                        flags.w_mismatch = true;
                    }
                }
            }
            if flags.w_mismatch {
                self.meters.w_mismatches += 1;
            }
            if flags.w_unmapped {
                self.meters.w_unmapped += 1;
            }
            if entity_mismatch {
                // Clean burst under an entity switch: relatch (geometry
                // meters above are kept so the snap vector survives; no
                // reject counter fires — this is a steppable verdict).
                return relatch_verdict!(height_ok, true);
            }
            return FrameVerdict {
                disposition: Disposition::Accept,
                v1_grammar_ok: true,
                v2_plausible: true,
                v3_speed_ok: true,
                v4_height_ok: height_ok,
                v5_pflags_ok: true,
                v6_input_foot_ignored: true,
                v7_sequenced: true,
                v8_epoch_ok: true,
                flags,
                legs,
                trailing_bytes: trailing,
            };
        }
        if plane_shift {
            if entity_mismatch {
                self.meters.oob_rejects += 1;
                return entity_mismatch_verdict!();
            }
            self.meters.epoch_rejects += 1;
            return FrameVerdict {
                disposition: Disposition::Reject(RejectReason::Teleport),
                v1_grammar_ok: true,
                v2_plausible: true,
                v3_speed_ok: false,
                v4_height_ok: false,
                v5_pflags_ok: true,
                v6_input_foot_ignored: true,
                v7_sequenced: true,
                v8_epoch_ok: false,
                flags,
                legs,
                trailing_bytes: trailing,
            };
        }
        if teleport_leg.iter().any(|t| *t) {
            // Speed cap on NON-teleport legs only (the jump itself is over
            // cap by shape; first legs scale with dt_ticks, inner legs are
            // single steps). Any other overcap stays `SpeedHack`.
            let mut nontele_speed_ok = true;
            px = prev.x;
            py = prev.y;
            for (i, wp) in report.waypoints.iter().enumerate() {
                let dx = wp.x as i64 - px as i64;
                let dy = wp.y as i64 - py as i64;
                px = wp.x;
                py = wp.y;
                if teleport_leg[i] {
                    continue;
                }
                let cap = if i == 0 {
                    SPEED_CAP_RAW_PER_STEP * prev.dt_ticks.max(1) as i64
                } else {
                    SPEED_CAP_RAW_PER_STEP
                };
                if dx * dx + dy * dy > cap * cap {
                    nontele_speed_ok = false;
                    break;
                }
            }
            if nontele_speed_ok {
                // Reanchor: meter the portal vector (incoming prev avatar
                // pos → first post-jump waypoint) for portal mapping, then
                // accept-as-new-anchor (V4 flags computed as on the accept
                // path; V4 never rejects).
                let j = teleport_leg.iter().position(|t| *t).unwrap_or(0);
                let fp = &report.waypoints[j];
                self.meters.teleport_reanchors += 1;
                self.meters.last_teleport_dx = fp.x as i64 - prev.x as i64;
                self.meters.last_teleport_dy = fp.y as i64 - prev.y as i64;
                let mut height_ok = true;
                for l in &legs {
                    if let Some(r) = l.w_residual {
                        if r > W_BAND_RAW {
                            height_ok = false;
                            flags.w_mismatch = true;
                        }
                    }
                }
                if flags.w_mismatch {
                    self.meters.w_mismatches += 1;
                }
                if flags.w_unmapped {
                    self.meters.w_unmapped += 1;
                }
                if entity_mismatch {
                    // Teleport-shaped but otherwise clean under an entity
                    // switch: relatch (portal-vector meters above are kept
                    // for portal mapping; no reject counter fires).
                    return relatch_verdict!(height_ok, true);
                }
                return FrameVerdict {
                    disposition: Disposition::Reanchor(ReanchorReason::TeleportReanchor),
                    v1_grammar_ok: true,
                    v2_plausible: true,
                    v3_speed_ok: true,
                    v4_height_ok: height_ok,
                    v5_pflags_ok: true,
                    v6_input_foot_ignored: true,
                    v7_sequenced: true,
                    v8_epoch_ok: true,
                    flags,
                    legs,
                    trailing_bytes: trailing,
                };
            }
            // Else fall through: V3 below rejects `SpeedHack` (the jump leg
            // is over cap too, so the speed verdict is unchanged).
        }

        // V3 speed cap: first leg scales with dt_ticks, inner legs are single
        // steps (MOVE-001 open: quantum value is an E1 side-observation).
        let mut speed_ok = true;
        px = prev.x;
        py = prev.y;
        for (i, wp) in report.waypoints.iter().enumerate() {
            let cap = if i == 0 {
                SPEED_CAP_RAW_PER_STEP * prev.dt_ticks.max(1) as i64
            } else {
                SPEED_CAP_RAW_PER_STEP
            };
            let dx = wp.x as i64 - px as i64;
            let dy = wp.y as i64 - py as i64;
            if dx * dx + dy * dy > cap * cap {
                speed_ok = false;
                break;
            }
            px = wp.x;
            py = wp.y;
        }
        if !speed_ok {
            if entity_mismatch {
                self.meters.oob_rejects += 1;
                return entity_mismatch_verdict!();
            }
            flags.speed_clamp = true;
            self.meters.speed_clamps += 1;
            return FrameVerdict {
                disposition: Disposition::Reject(RejectReason::SpeedHack),
                v1_grammar_ok: true,
                v2_plausible: true,
                v3_speed_ok: false,
                v4_height_ok: true,
                v5_pflags_ok: true,
                v6_input_foot_ignored: true,
                v7_sequenced: true,
                v8_epoch_ok: true,
                flags,
                legs,
                trailing_bytes: trailing,
            };
        }

        // V4 heightfield band (teleports already reanchored above: a lone W
        // mismatch is accepted with the flag (V4).
        let mut height_ok = true;
        for l in &legs {
            if let Some(r) = l.w_residual {
                if r > W_BAND_RAW {
                    height_ok = false;
                    flags.w_mismatch = true;
                }
            }
        }
        if flags.w_mismatch {
            self.meters.w_mismatches += 1;
        }
        if flags.w_unmapped {
            self.meters.w_unmapped += 1;
        }

        // Entity decision: a mismatch on an otherwise-clean burst relatches
        // (steps like an accept; the caller switches its latched entity).
        if entity_mismatch {
            return relatch_verdict!(height_ok, true);
        }

        FrameVerdict {
            disposition: Disposition::Accept,
            v1_grammar_ok: true,
            v2_plausible: true,
            v3_speed_ok: true,
            v4_height_ok: height_ok,
            v5_pflags_ok: true,
            v6_input_foot_ignored: true,
            v7_sequenced: true,
            v8_epoch_ok: true,
            flags,
            legs,
            trailing_bytes: trailing,
        }
    }

    /// Validate a raw c2s frame end-to-end (V1 parse, then V2–V8).
    /// Non-movers yield `None` (passthrough, PROTO-004); grammar failures
    /// yield a `Drop` verdict and bump `grammar_drops`.
    pub fn validate_frame(&mut self, raw: &[u8], prev: &PrevState) -> Option<FrameVerdict> {
        match parse_report(raw) {
            Err(ParseFail::Passthrough) => None,
            Err(ParseFail::GrammarDrop) => {
                self.meters.grammar_drops += 1;
                Some(FrameVerdict {
                    disposition: Disposition::Drop(DropReason::Grammar),
                    v1_grammar_ok: false,
                    v2_plausible: false,
                    v3_speed_ok: false,
                    v4_height_ok: false,
                    v5_pflags_ok: false,
                    v6_input_foot_ignored: true,
                    v7_sequenced: true,
                    v8_epoch_ok: false,
                    flags: Flags::default(),
                    legs: Vec::new(),
                    trailing_bytes: 0,
                })
            }
            Ok(report) => Some(self.validate(&report, prev)),
        }
    }

    /// Build a framed s2c `07,35` authority inner from post-step state:
    /// `[07 35][ent u32 LE][n u8][p,X,Y,W 13B]xN[foot 5B][06]`.
    /// W is CARRIED from stepped input (client truth); table lookup is the
    /// residual signal only, never emitted. HP from the store (default
    /// 2540.0), foot is the E1-assumption constant, p preserved.
    pub fn authority(&self, avatar: &AvatarState, steps: &[StepRecord]) -> Vec<u8> {
        let n = steps.len().max(1) as u8;
        let mut out = Vec::with_capacity(
            MOVER_HEADER_LEN + WAYPOINT_LEN * steps.len().max(1) + AUTH_TAIL_LEN,
        );
        out.extend_from_slice(&OP_S2C_AUTH);
        out.extend_from_slice(&avatar.entity.to_le_bytes());
        out.push(if steps.is_empty() { 1 } else { n });
        if steps.is_empty() {
            // Still-frame: authoritative rest at current pos, dW=0.
            out.push(0x01);
            out.extend_from_slice(&avatar.x.to_le_bytes());
            out.extend_from_slice(&avatar.y.to_le_bytes());
            out.extend_from_slice(&(avatar.w as u32).to_le_bytes());
        } else {
            for s in steps {
                out.push(s.p);
                out.extend_from_slice(&s.x.to_le_bytes());
                out.extend_from_slice(&s.y.to_le_bytes());
                out.extend_from_slice(&(s.w as u32).to_le_bytes());
            }
        }
        out.extend_from_slice(&AUTH_FOOT);
        out.push(AUTH_END);
        out
    }

    /// Step-then-emit ordering (`update_entities` before
    /// `emit_trace`): walk the avatar through every accepted waypoint at the
    /// §V3 cap — snapping exactly on p01 arrival — THEN frame the post-step
    /// positions via [`MovementModel::authority`]. Accepted input is already
    /// within cap (validate fail-closes), so stepping is exact here.
    ///
    /// `obs` carries metered observations only (queue depth before drain, lag
    /// in frames); both are recorded, neither tunes anything.
    pub fn step_then_emit(
        &mut self,
        avatar: &mut AvatarState,
        accepted: &[Waypoint],
        obs: &StepObs,
    ) -> Vec<u8> {
        self.meters.record_queue_depth(obs.queue_depth_before);
        self.meters.record_lag(obs.lag_frames);
        let mut steps = Vec::with_capacity(accepted.len().max(1));
        for wp in accepted {
            if wp.p == 0x01 {
                avatar.x = wp.x;
                avatar.y = wp.y;
                avatar.w = wp.w;
            } else {
                // Exact step (validated within cap); defensive clamp keeps the
                // model inside the cap even if a caller skips validation.
                // Teleport-shaped legs snap exactly instead: a validated
                // reanchor carries a new epoch anchor, and clamping it would
                // strand the avatar mid-void and cascade rejects.
                let dx = wp.x as i64 - avatar.x as i64;
                let dy = wp.y as i64 - avatar.y as i64;
                let cap = SPEED_CAP_RAW_PER_STEP;
                if dx * dx + dy * dy > TELEPORT_THRESHOLD_RAW * TELEPORT_THRESHOLD_RAW {
                    avatar.x = wp.x;
                    avatar.y = wp.y;
                } else if dx * dx + dy * dy > cap * cap {
                    // Integer-only clamp (fixed-point rule: no floats anywhere):
                    // scale the step to length `cap` via isqrt.
                    let dist = isqrt(dx * dx + dy * dy).max(1);
                    avatar.x += (dx * cap / dist) as i32;
                    avatar.y += (dy * cap / dist) as i32;
                    self.meters.speed_clamps += 1;
                } else {
                    avatar.x = wp.x;
                    avatar.y = wp.y;
                }
                // W is CARRIED from input (client truth), never table-fitted:
                // emitting table height where the session plane disagrees
                // hardwalls the client (auth1: 631 w-diverges, 10 relogs).
                // Table lookup survives only as the w_mismatch residual.
                avatar.w = wp.w;
            }
            avatar.tick += 1;
            steps.push(StepRecord {
                p: wp.p,
                x: avatar.x,
                y: avatar.y,
                w: avatar.w,
            });
        }
        self.meters.record_drain(steps.len());
        if avatar.hp_raw() != HP_DEFAULT_RAW {
            self.meters.hp_divergences += 1;
        }
        self.authority(avatar, &steps)
    }
}

/// Integer square root (floor) for the defensive step clamp. The validated
/// happy path never clamps, so this only runs when a caller skips validation.
fn isqrt(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    // Newton iteration from the top bit down; converges for all n < 2^62.
    let mut x = 1i64 << (64 - (n as u64).leading_zeros() + 1).div_ceil(2);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

fn nearest_dist2(table: &[HeightSample], x: i32, y: i32) -> i64 {
    let mut best = i64::MAX;
    for s in table {
        let dx = s.x as i64 - x as i64;
        let dy = s.y as i64 - y as i64;
        let d2 = dx * dx + dy * dy;
        if d2 < best {
            best = d2;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Avatar + step observations
// ---------------------------------------------------------------------------

/// Authoritative per-avatar state: position + tick. HP lives in the model's
/// single-entry store (V6: never from input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvatarState {
    pub entity: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub tick: u64,
    hp_raw: u32,
}

impl AvatarState {
    pub fn spawn(entity: u32, spawn: &Spawn) -> Self {
        AvatarState {
            entity,
            x: spawn.x,
            y: spawn.y,
            w: spawn.w,
            tick: 0,
            hp_raw: HP_DEFAULT_RAW,
        }
    }

    pub fn hp_raw(&self) -> u32 {
        self.hp_raw
    }
}

/// One authoritative stepped position (W resolved at emit; p preserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepRecord {
    pub p: u8,
    pub x: i32,
    pub y: i32,
    pub w: i32,
}

/// Metered step observations: queue depth before drain + authority lag in
/// frames. Recorded into [`Meters`]; no tuning reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepObs {
    pub queue_depth_before: usize,
    pub lag_frames: u32,
}

/// Grammar check for our single-block authority frame:
/// `[07 35][ent][n][13B records][foot 5B][06]` with exact `13+13n` length.
pub fn is_authority_frame(buf: &[u8]) -> bool {
    if buf.len() < MOVER_HEADER_LEN + WAYPOINT_LEN + AUTH_TAIL_LEN {
        return false;
    }
    if buf[0] != 0x07 || buf[1] != 0x35 {
        return false;
    }
    let n = buf[6] as usize;
    if n < 1 {
        return false;
    }
    if buf.len() != MOVER_HEADER_LEN + WAYPOINT_LEN * n + AUTH_TAIL_LEN {
        return false;
    }
    let tail = MOVER_HEADER_LEN + WAYPOINT_LEN * n;
    buf[tail..tail + 5] == AUTH_FOOT && buf[tail + 5] == AUTH_END
}

/// Decode an authority frame's records `(p, x, y, w)`; `None` if grammar fails.
pub fn parse_authority(buf: &[u8]) -> Option<(u32, Vec<Waypoint>)> {
    if !is_authority_frame(buf) {
        return None;
    }
    let entity = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
    let n = buf[6] as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = MOVER_HEADER_LEN + WAYPOINT_LEN * i;
        out.push(Waypoint {
            p: buf[off],
            x: i32::from_le_bytes([buf[off + 1], buf[off + 2], buf[off + 3], buf[off + 4]]),
            y: i32::from_le_bytes([buf[off + 5], buf[off + 6], buf[off + 7], buf[off + 8]]),
            w: i32::from_le_bytes([buf[off + 9], buf[off + 10], buf[off + 11], buf[off + 12]]),
        });
    }
    Some((entity, out))
}

// ---------------------------------------------------------------------------
// Minimal std-only JSON parser (contract loader; integers, no floats)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum JVal {
    Num(i64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

fn jnum(v: &JVal) -> Option<i64> {
    match v {
        JVal::Num(n) => Some(*n),
        _ => None,
    }
}

fn parse_json(text: &str) -> Result<JVal, String> {
    let b = text.as_bytes();
    let mut pos = 0;
    skip_ws(b, &mut pos);
    let v = parse_value(b, &mut pos)?;
    skip_ws(b, &mut pos);
    if pos != b.len() {
        return Err(format!("trailing bytes at offset {pos}"));
    }
    Ok(v)
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

fn parse_value(b: &[u8], pos: &mut usize) -> Result<JVal, String> {
    skip_ws(b, pos);
    match b.get(*pos) {
        Some(b'{') => parse_obj(b, pos),
        Some(b'[') => parse_arr(b, pos),
        Some(b'"') => parse_str(b, pos).map(JVal::Str),
        Some(c) if *c == b'-' || c.is_ascii_digit() => parse_num(b, pos).map(JVal::Num),
        Some(_) => Err(format!("unexpected byte at offset {pos}")),
        None => Err("unexpected end of input".to_string()),
    }
}

fn parse_obj(b: &[u8], pos: &mut usize) -> Result<JVal, String> {
    *pos += 1; // {
    let mut kv = Vec::new();
    loop {
        skip_ws(b, pos);
        match b.get(*pos) {
            Some(b'}') => {
                *pos += 1;
                return Ok(JVal::Obj(kv));
            }
            Some(b'"') => {
                let k = parse_str(b, pos)?;
                skip_ws(b, pos);
                if b.get(*pos) != Some(&b':') {
                    return Err(format!("expected ':' at offset {pos}"));
                }
                *pos += 1;
                let v = parse_value(b, pos)?;
                kv.push((k, v));
                skip_ws(b, pos);
                match b.get(*pos) {
                    Some(b',') => {
                        *pos += 1;
                    }
                    Some(b'}') => {}
                    _ => return Err(format!("expected ',' or '}}' at offset {pos}")),
                }
            }
            _ => return Err(format!("expected key or '}}' at offset {pos}")),
        }
    }
}

fn parse_arr(b: &[u8], pos: &mut usize) -> Result<JVal, String> {
    *pos += 1; // [
    let mut out = Vec::new();
    loop {
        skip_ws(b, pos);
        match b.get(*pos) {
            Some(b']') => {
                *pos += 1;
                return Ok(JVal::Arr(out));
            }
            None => return Err("unterminated array".to_string()),
            _ => {
                out.push(parse_value(b, pos)?);
                skip_ws(b, pos);
                match b.get(*pos) {
                    Some(b',') => {
                        *pos += 1;
                    }
                    Some(b']') => {}
                    _ => return Err(format!("expected ',' or ']' at offset {pos}")),
                }
            }
        }
    }
}

fn parse_str(b: &[u8], pos: &mut usize) -> Result<String, String> {
    *pos += 1; // "
    let mut out = String::new();
    loop {
        match b.get(*pos) {
            None => return Err("unterminated string".to_string()),
            Some(b'"') => {
                *pos += 1;
                return Ok(out);
            }
            Some(b'\\') => {
                *pos += 1;
                match b.get(*pos) {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'n') => out.push('\n'),
                    Some(c) => return Err(format!("bad escape {c} at offset {pos}")),
                    None => return Err("unterminated escape".to_string()),
                }
                *pos += 1;
            }
            Some(c) => {
                out.push(*c as char);
                *pos += 1;
            }
        }
    }
}

/// Integer parser: optional `-`, digits; a `.fraction`/`e` suffix is skipped
/// (contract is integer raw units; sim keeps the E4 no-float rule).
fn parse_num(b: &[u8], pos: &mut usize) -> Result<i64, String> {
    let mut neg = false;
    if b.get(*pos) == Some(&b'-') {
        neg = true;
        *pos += 1;
    }
    let start = *pos;
    while *pos < b.len() && b[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if start == *pos {
        return Err(format!("expected digits at offset {pos}"));
    }
    let mut val: i64 = std::str::from_utf8(&b[start..*pos])
        .map_err(|e| e.to_string())?
        .parse::<i64>()
        .map_err(|e| e.to_string())?;
    // Skip fraction/exponent without float math.
    if b.get(*pos) == Some(&b'.') {
        *pos += 1;
        while *pos < b.len() && b[*pos].is_ascii_digit() {
            *pos += 1;
        }
    }
    if matches!(b.get(*pos), Some(b'e') | Some(b'E')) {
        *pos += 1;
        if matches!(b.get(*pos), Some(b'+') | Some(b'-')) {
            *pos += 1;
        }
        while *pos < b.len() && b[*pos].is_ascii_digit() {
            *pos += 1;
        }
    }
    if neg {
        val = -val;
    }
    Ok(val)
}

// ---------------------------------------------------------------------------
// Tests (offline, vector-based; 3-sample fixture per task — the slice JSON
// does not exist yet, so tests do NOT block on it, but from_slice_json is
// covered against the contract shape directly).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const AVATAR: u32 = 0x4065_9770; // fixture avatar entity

    /// 3-sample Townston fixture: spawn + two 502-quantum walk legs.
    fn fixture_table() -> Vec<HeightSample> {
        vec![
            HeightSample {
                x: 22970,
                y: 122378,
                w: 150000,
            },
            HeightSample {
                x: 23472,
                y: 122378,
                w: 150100,
            },
            HeightSample {
                x: 23472,
                y: 122880,
                w: 150100,
            },
        ]
    }

    fn fixture_bbox() -> BBox {
        BBox::new(22000, 121000, 24500, 124000)
    }

    fn fixture_model() -> MovementModel {
        // Directed gate east of the walk line: x=23600, y 122400..123400,
        // blocking eastward (+x) crossings. Walk legs stay west of it.
        let gates = vec![Gate::new(23600, 122400, 23600, 123400, 1, 0)];
        MovementModel::new(
            fixture_table(),
            fixture_bbox(),
            Spawn {
                x: 22970,
                y: 122378,
                w: 0,
            },
            AVATAR,
        )
        .with_gates(gates)
    }

    fn prev_at(x: i32, y: i32) -> PrevState {
        PrevState {
            avatar_entity: AVATAR,
            x,
            y,
            dt_ticks: 1,
        }
    }

    /// Encode a raw c2s 07,34 frame (mirrors the reference mover builder).
    fn raw_0734(entity: u32, wps: &[(u8, i32, i32, i32)]) -> Vec<u8> {
        let mut out = vec![0x07, 0x34];
        out.extend_from_slice(&entity.to_le_bytes());
        out.push(wps.len() as u8);
        for (p, x, y, w) in wps {
            out.push(*p);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&(*w as u32).to_le_bytes());
        }
        out
    }

    fn walk_burst() -> Vec<(u8, i32, i32, i32)> {
        // Recorded-walk shape: en-route p00 legs + p01 arrival duplicating
        // prev (X,Y,W) as last-in-msg (V5 walk-stop cycles).
        vec![
            (0x00, 22970, 122378, 150000),
            (0x00, 23472, 122378, 150100),
            (0x00, 23472, 122880, 150100),
            (0x01, 23472, 122880, 150100),
        ]
    }

    #[test]
    fn same_inputs_give_identical_trace_twice() {
        let run = || {
            let mut m = fixture_model();
            let mut av = AvatarState::spawn(AVATAR, &m.spawn);
            let raw = raw_0734(AVATAR, &walk_burst());
            let rep = parse_report(&raw).unwrap();
            let prev = prev_at(22970, 122378);
            let v = m.validate(&rep, &prev);
            assert!(v.accepted(), "fixture burst must validate: {v:?}");
            let bytes = m.step_then_emit(
                &mut av,
                &rep.waypoints,
                &StepObs {
                    queue_depth_before: 1,
                    lag_frames: 10,
                },
            );
            (bytes, av, m.meters)
        };
        let (b1, a1, m1) = run();
        let (b2, a2, m2) = run();
        assert_eq!(b1, b2);
        assert_eq!(a1, a2);
        assert_eq!(m1, m2);
    }

    #[test]
    fn validation_accepts_recorded_walk_burst() {
        let mut m = fixture_model();
        let raw = raw_0734(AVATAR, &walk_burst());
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(v.disposition, Disposition::Accept);
        assert!(
            v.v1_grammar_ok
                && v.v2_plausible
                && v.v3_speed_ok
                && v.v4_height_ok
                && v.v5_pflags_ok
                && v.v6_input_foot_ignored
                && v.v7_sequenced
                && v.v8_epoch_ok
        );
        assert!(!v.flags.speed_clamp && !v.flags.w_mismatch && !v.flags.w_unmapped);
        assert_eq!(v.trailing_bytes, 0);
        // Meters-first: residuals recorded per waypoint, HP unfired.
        assert_eq!(m.meters.w_residuals, vec![0, 0, 0, 0]);
        assert_eq!(m.meters.hp_divergences, 0);
    }

    #[test]
    fn validation_rejects_wall_clip_gate_crossing() {
        let mut m = fixture_model();
        // Short eastward leg across the x=23600 gate (dist 200 ≤ cap, W exact,
        // in-bbox): only the directed gate can reject it.
        let raw = raw_0734(AVATAR, &[(0x00, 23700, 122500, 150100)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(23500, 122500));
        assert_eq!(v.disposition, Disposition::Reject(RejectReason::WallClip));
        assert!(!v.v2_plausible);
        assert_eq!(m.meters.wall_clips, 1);
    }

    #[test]
    fn validation_rejects_oob() {
        let mut m = fixture_model();
        let raw = raw_0734(AVATAR, &[(0x00, 30000, 122378, 150100)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(v.disposition, Disposition::Reject(RejectReason::Oob));
        assert_eq!(m.meters.oob_rejects, 1);
    }

    #[test]
    fn validation_rejects_speed_hack() {
        let mut m = fixture_model();
        // 3000-raw leg along the walk line: in-bbox+margin, mapped W, no gate
        // on this line — only the speed cap can reject it.
        let raw = raw_0734(AVATAR, &[(0x00, 25970, 122378, 150100)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(v.disposition, Disposition::Reject(RejectReason::SpeedHack));
        assert!(!v.v3_speed_ok && v.flags.speed_clamp);
        assert_eq!(m.meters.speed_clamps, 1);
    }

    #[test]
    fn validation_rejects_vertical_event() {
        let mut m = fixture_model();
        // dW!=0 at dX=dY=0 (marks1 f1511-1513 shape): never modelled.
        let raw = raw_0734(AVATAR, &[(0x00, 22970, 122378, 155000)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(
            v.disposition,
            Disposition::Reject(RejectReason::VerticalEvent)
        );
        assert_eq!(m.meters.vertical_events, 1);
    }

    #[test]
    fn w_settle_glide_accepts_but_large_in_place_dw_rejects() {
        // ANCHOR1 live: client standing still at (46264,149667) reports W
        // gliding ~500/leg (burst W=[69216,68713,68210,67707]). In-place
        // legs with 0 < |dW| <= W_SETTLE_MAX_RAW accept with `w_settle`
        // (+`w_settles`); a huge in-place dW still rejects as VerticalEvent.
        assert_eq!(W_SETTLE_MAX_RAW, 1024);
        let mut m = MovementModel::new(
            vec![HeightSample {
                x: 46264,
                y: 149667,
                w: 69216,
            }],
            BBox::new(-100000, -100000, 200000, 300000),
            Spawn {
                x: 46264,
                y: 149667,
                w: 0,
            },
            AVATAR,
        );
        let prev = PrevState {
            avatar_entity: AVATAR,
            x: 46264,
            y: 149667,
            dt_ticks: 1,
        };
        let raw = raw_0734(
            AVATAR,
            &[
                (0x00, 46264, 149667, 69216),
                (0x00, 46264, 149667, 68713),
                (0x00, 46264, 149667, 68210),
                (0x00, 46264, 149667, 67707),
            ],
        );
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev);
        assert_eq!(v.disposition, Disposition::Accept);
        assert!(v.flags.w_settle && v.steppable());
        assert_eq!(m.meters.w_settles, 1);
        assert_eq!(m.meters.vertical_events, 0);
        // Huge in-place dW still rejects (never modelled).
        let raw = raw_0734(AVATAR, &[(0x00, 46264, 149667, 69216 + 5000)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev);
        assert_eq!(
            v.disposition,
            Disposition::Reject(RejectReason::VerticalEvent)
        );
        assert!(!v.flags.w_settle);
        assert_eq!(m.meters.vertical_events, 1);
    }

    #[test]
    fn validation_reanchors_teleport_as_new_epoch() {
        // Big-bbox model so the jump is in-bounds: Δ≫cap with entity,
        // bbox+margin, p01 and (non-teleport) speed all valid reanchors as
        // a new epoch anchor (SIM-005 portal semantics) instead of
        // rejecting.
        let mut m = MovementModel::new(
            vec![HeightSample { x: 0, y: 0, w: 7 }],
            BBox::new(-100000, -100000, 100000, 100000),
            Spawn { x: 0, y: 0, w: 0 },
            AVATAR,
        );
        let raw = raw_0734(AVATAR, &[(0x00, 50000, 0, 7)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(0, 0));
        assert_eq!(
            v.disposition,
            Disposition::Reanchor(ReanchorReason::TeleportReanchor)
        );
        assert_eq!(
            ReanchorReason::TeleportReanchor.as_str(),
            "teleport_reanchor"
        );
        assert!(!v.accepted() && v.steppable());
        assert!(v.v2_plausible && v.v3_speed_ok && v.v5_pflags_ok && v.v8_epoch_ok);
        // Portal vector metered: incoming prev (0,0) → first post-jump
        // waypoint (50000,0); the old epoch counter stays quiet.
        assert_eq!(m.meters.teleport_reanchors, 1);
        assert_eq!(
            (m.meters.last_teleport_dx, m.meters.last_teleport_dy),
            (50000, 0)
        );
        assert_eq!(m.meters.epoch_rejects, 0);
        // Reanchored steps STEP to the new location (input X/Y/W emitted).
        let mut av = AvatarState::spawn(AVATAR, &m.spawn);
        let bytes = m.step_then_emit(
            &mut av,
            &rep.waypoints,
            &StepObs {
                queue_depth_before: 1,
                lag_frames: 10,
            },
        );
        assert!(is_authority_frame(&bytes));
        let (_, recs) = parse_authority(&bytes).unwrap();
        assert_eq!((recs[0].x, recs[0].y, recs[0].w), (50000, 0, 7));
        assert_eq!((av.x, av.y, av.w), (50000, 0, 7));
        // Follow-up continuing from the new anchor validates cleanly: no
        // reject cascade.
        let raw2 = raw_0734(AVATAR, &[(0x00, 50400, 0, 7)]);
        let rep2 = parse_report(&raw2).unwrap();
        let prev2 = PrevState {
            avatar_entity: AVATAR,
            x: 50000,
            y: 0,
            dt_ticks: 1,
        };
        assert_eq!(m.validate(&rep2, &prev2).disposition, Disposition::Accept);
        // Non-teleport defects still reject: a speed-hack leg ...
        let raw = raw_0734(AVATAR, &[(0x00, 3000, 0, 7)]);
        let rep = parse_report(&raw).unwrap();
        assert_eq!(
            m.validate(&rep, &prev_at(0, 0)).disposition,
            Disposition::Reject(RejectReason::SpeedHack)
        );
        // ... a teleport-scale jump outside bbox+margin (bbox first) ...
        let raw = raw_0734(AVATAR, &[(0x00, 200000, 0, 7)]);
        let rep = parse_report(&raw).unwrap();
        assert_eq!(
            m.validate(&rep, &prev_at(0, 0)).disposition,
            Disposition::Reject(RejectReason::Oob)
        );
        // ... and an entity mismatch combined with a speed defect stays
        // EntityMismatch (a clean burst from an unexpected entity would
        // relatch instead — see entity_switch_relatch_* tests).
        let raw = raw_0734(AVATAR + 1, &[(0x00, 3000, 0, 7)]);
        let rep = parse_report(&raw).unwrap();
        assert_eq!(
            m.validate(&rep, &prev_at(0, 0)).disposition,
            Disposition::Reject(RejectReason::EntityMismatch)
        );
    }

    #[test]
    fn teleport_reanchor_records_live_portal_vector() {
        // Recurring live portal vector +86016 X (0x15000): a seamless mouth
        // translation with Y/W continuous must reanchor (not cascade) and
        // meter the exact vector for portal mapping.
        let mut m = MovementModel::new(
            vec![HeightSample {
                x: 10000,
                y: 179472,
                w: 42,
            }],
            BBox::new(-100000, 100000, 200000, 300000),
            Spawn {
                x: 10000,
                y: 179472,
                w: 0,
            },
            AVATAR,
        );
        let prev = PrevState {
            avatar_entity: AVATAR,
            x: 10000,
            y: 179472,
            dt_ticks: 1,
        };
        let raw = raw_0734(AVATAR, &[(0x00, 96016, 179576, 42)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev);
        assert_eq!(
            v.disposition,
            Disposition::Reanchor(ReanchorReason::TeleportReanchor)
        );
        assert_eq!(m.meters.teleport_reanchors, 1);
        assert_eq!(
            (m.meters.last_teleport_dx, m.meters.last_teleport_dy),
            (86016, 104)
        );
        // Steps land on the new anchor; a follow-up walk from there
        // validates cleanly.
        let mut av = AvatarState::spawn(AVATAR, &m.spawn);
        av.x = 10000;
        av.y = 179472;
        av.w = 42;
        let bytes = m.step_then_emit(
            &mut av,
            &rep.waypoints,
            &StepObs {
                queue_depth_before: 1,
                lag_frames: 10,
            },
        );
        let (_, recs) = parse_authority(&bytes).unwrap();
        assert_eq!((recs[0].x, recs[0].y, recs[0].w), (96016, 179576, 42));
        assert_eq!((av.x, av.y, av.w), (96016, 179576, 42));
        let raw2 = raw_0734(AVATAR, &[(0x00, 96416, 179576, 42)]);
        let rep2 = parse_report(&raw2).unwrap();
        let prev2 = PrevState {
            avatar_entity: AVATAR,
            x: 96016,
            y: 179576,
            dt_ticks: 1,
        };
        assert_eq!(m.validate(&rep2, &prev2).disposition, Disposition::Accept);
    }

    #[test]
    fn w_plane_shift_medium_jump_stays_reject() {
        // W-plane residual (> band*4) paired with a medium jump ABOVE
        // SNAP_MAX_RAW stays Reject(Teleport): ONLY single-leg jumps beyond
        // TELEPORT_THRESHOLD_RAW reanchor (reference parity contract —
        // threshold-only triggers Reanchor, the plane-shift branch rejects,
        // and the snap carve-out below SNAP_MAX_RAW accepts instead).
        let mut m = MovementModel::new(
            vec![HeightSample {
                x: 15000,
                y: 0,
                w: 0,
            }],
            BBox::new(-100000, -100000, 100000, 100000),
            Spawn { x: 0, y: 0, w: 0 },
            AVATAR,
        );
        // Δ=15000: above SNAP_MAX_RAW (12288) but below the 20000
        // threshold, with a mapped residual of 20000 > band*4 (10000).
        let raw = raw_0734(AVATAR, &[(0x00, 15000, 0, 20000)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(0, 0));
        assert_eq!(v.disposition, Disposition::Reject(RejectReason::Teleport));
        assert!(!v.steppable() && !v.v8_epoch_ok);
        assert_eq!(m.meters.epoch_rejects, 1);
        assert_eq!(m.meters.teleport_reanchors, 0);
    }

    #[test]
    fn snap_jog_accepts_with_vector_meters() {
        // ANCHOR1 live: post-halt the client continued with a ~6156-dist
        // leg carrying a large W residual (recurring ±6144 grid-snap
        // family). At/below SNAP_MAX_RAW it accepts with `snap` (+`snap_dx`/
        // `snap_dy` leg vector); a 15k plane-shift still rejects; a >20k
        // jump still reanchors.
        assert_eq!(SNAP_MAX_RAW, 12_288);
        fn snap_model() -> MovementModel {
            MovementModel::new(
                vec![
                    HeightSample {
                        x: 6156,
                        y: 0,
                        w: 0,
                    },
                    HeightSample {
                        x: 15000,
                        y: 0,
                        w: 0,
                    },
                    HeightSample {
                        x: 50000,
                        y: 0,
                        w: 0,
                    },
                ],
                BBox::new(-100000, -100000, 100000, 100000),
                Spawn { x: 0, y: 0, w: 0 },
                AVATAR,
            )
        }
        // 6156 jog + W-plane shift (residual 20000 > band*4): accept.
        let mut m = snap_model();
        let raw = raw_0734(AVATAR, &[(0x00, 6156, 0, 20000)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(0, 0));
        assert_eq!(v.disposition, Disposition::Accept);
        assert!(v.flags.snap && v.steppable());
        assert_eq!((m.meters.snap_dx, m.meters.snap_dy), (6156, 0));
        assert_eq!(m.meters.epoch_rejects, 0);
        // 15k plane-shift (same residual shape, above SNAP_MAX): reject.
        let mut m = snap_model();
        let raw = raw_0734(AVATAR, &[(0x00, 15000, 0, 20000)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(0, 0));
        assert_eq!(v.disposition, Disposition::Reject(RejectReason::Teleport));
        assert!(!v.flags.snap && !v.steppable());
        assert_eq!(m.meters.epoch_rejects, 1);
        assert_eq!((m.meters.snap_dx, m.meters.snap_dy), (0, 0));
        // >20k threshold jump: still reanchors (threshold-only triggers
        // Reanchor; the plane-shift branch never reanchors).
        let mut m = snap_model();
        let raw = raw_0734(AVATAR, &[(0x00, 50000, 0, 0)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(0, 0));
        assert_eq!(
            v.disposition,
            Disposition::Reanchor(ReanchorReason::TeleportReanchor)
        );
        assert!(!v.flags.snap);
        assert_eq!(m.meters.teleport_reanchors, 1);
        assert_eq!(
            (m.meters.last_teleport_dx, m.meters.last_teleport_dy),
            (50000, 0)
        );
    }

    #[test]
    fn entity_switch_relatches_when_clean_and_steps() {
        // Live: after a zone transfer the client reports as a NEW entity
        // (0x406510e6); later the avatar id flips spontaneously
        // (0x406586ad->0x416586ad). A valid burst from the wrong entity
        // relatches (steps like accept; caller switches its latch) with
        // metered old/new ids.
        const NEW_ENT: u32 = 0x4065_10e6;
        let mut m = fixture_model();
        let prev = prev_at(22970, 122378);
        let raw = raw_0734(NEW_ENT, &[(0x00, 23472, 122378, 150100)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev);
        match v.disposition {
            Disposition::Relatch(RelatchReason::EntitySwitch {
                old_entity,
                new_entity,
            }) => {
                assert_eq!(old_entity, AVATAR);
                assert_eq!(new_entity, NEW_ENT);
            }
            other => panic!("expected Relatch, got {other:?}"),
        }
        assert_eq!(
            RelatchReason::EntitySwitch {
                old_entity: AVATAR,
                new_entity: NEW_ENT,
            }
            .as_str(),
            "entity_switch"
        );
        assert!(v.steppable() && !v.accepted());
        assert!(!v.v2_plausible && v.v3_speed_ok && v.v5_pflags_ok && v.v8_epoch_ok);
        assert_eq!(m.meters.entity_switches, 1);
        assert_eq!(
            (m.meters.last_entity_old, m.meters.last_entity_new),
            (AVATAR, NEW_ENT)
        );
        // Relatch steps the avatar to the reported waypoints; the caller
        // then switches its latched entity (model latch + prev + avatar —
        // the interleaving guard is the caller's job).
        let mut av = AvatarState::spawn(AVATAR, &m.spawn);
        let bytes = m.step_then_emit(
            &mut av,
            &rep.waypoints,
            &StepObs {
                queue_depth_before: 1,
                lag_frames: 10,
            },
        );
        assert!(is_authority_frame(&bytes));
        assert_eq!((av.x, av.y, av.w), (23472, 122378, 150100));
        m.avatar_entity = NEW_ENT;
        av.entity = NEW_ENT;
        assert_eq!(av.entity, NEW_ENT);
        // Follow-up from the new latch validates cleanly: no halt cascade.
        let raw2 = raw_0734(NEW_ENT, &[(0x00, 23472, 122880, 150100)]);
        let rep2 = parse_report(&raw2).unwrap();
        let prev2 = PrevState {
            avatar_entity: NEW_ENT,
            x: 23472,
            y: 122378,
            dt_ticks: 1,
        };
        assert_eq!(m.validate(&rep2, &prev2).disposition, Disposition::Accept);
    }

    #[test]
    fn entity_switch_with_speed_defect_stays_reject() {
        // Entity mismatch COMBINED with any other defect stays
        // Reject(EntityMismatch) — never relatch. The specific defect's
        // meter stays quiet (masked onto the entity meter).
        const NEW_ENT: u32 = 0x4065_10e6;
        let mut m = fixture_model();
        let raw = raw_0734(NEW_ENT, &[(0x00, 25970, 122378, 150100)]);
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(
            v.disposition,
            Disposition::Reject(RejectReason::EntityMismatch)
        );
        assert!(!v.steppable());
        assert_eq!(m.meters.entity_switches, 0);
        assert_eq!(m.meters.oob_rejects, 1);
        assert_eq!(m.meters.speed_clamps, 0);
    }

    #[test]
    fn p01_violation_rejects_but_p02_anywhere_accepts() {
        let mut m = fixture_model();
        // p01 mid-message (not last): reject.
        let raw = raw_0734(
            AVATAR,
            &[(0x01, 22970, 122378, 150000), (0x00, 23472, 122378, 150100)],
        );
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(
            v.disposition,
            Disposition::Reject(RejectReason::P01Violation)
        );
        assert!(!v.v5_pflags_ok);
        // p01 last but not duplicating prev: reject.
        let raw = raw_0734(
            AVATAR,
            &[(0x00, 22970, 122378, 150000), (0x01, 23472, 122378, 150100)],
        );
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(
            v.disposition,
            Disposition::Reject(RejectReason::P01Violation)
        );
        // p02 lead/replan mid-burst (NOT start-only): accepted.
        let raw = raw_0734(
            AVATAR,
            &[(0x00, 22970, 122378, 150000), (0x02, 23472, 122378, 150100)],
        );
        let rep = parse_report(&raw).unwrap();
        let v = m.validate(&rep, &prev_at(22970, 122378));
        assert_eq!(v.disposition, Disposition::Accept);
    }

    #[test]
    fn short_0752_passthrough_and_bad_grammar_drop() {
        let mut m = fixture_model();
        // Short 07,52 len-6/10 shapes pass through undecoded (PROTO-004).
        assert_eq!(
            m.validate_frame(
                &[0x07, 0x52, 0x5e, 0x93, 0x01, 0x56],
                &prev_at(22970, 122378)
            ),
            None
        );
        // Truncated mover: drop + count.
        let mut bad = raw_0734(AVATAR, &walk_burst());
        bad.truncate(10);
        let v = m.validate_frame(&bad, &prev_at(22970, 122378)).unwrap();
        assert_eq!(v.disposition, Disposition::Drop(DropReason::Grammar));
        assert!(!v.v1_grammar_ok);
        assert_eq!(m.meters.grammar_drops, 1);
    }

    #[test]
    fn authority_frame_parses_under_0735_grammar() {
        let mut m = fixture_model();
        let mut av = AvatarState::spawn(AVATAR, &m.spawn);
        let raw = raw_0734(AVATAR, &walk_burst());
        let rep = parse_report(&raw).unwrap();
        let prev = prev_at(22970, 122378);
        assert!(m.validate(&rep, &prev).accepted());
        let bytes = m.step_then_emit(
            &mut av,
            &rep.waypoints,
            &StepObs {
                queue_depth_before: 1,
                lag_frames: 10,
            },
        );
        // Grammar: exact 13+13n, [07 35] prefix, foot+06 tail.
        assert!(is_authority_frame(&bytes));
        assert_eq!(bytes.len(), 13 + 13 * 4);
        assert_eq!(&bytes[0..2], &[0x07, 0x35]);
        assert_eq!(bytes[6], 4);
        assert_eq!(
            &bytes[bytes.len() - 6..],
            &[0x02, 0x00, 0xEC, 0x09, 0x00, 0x06]
        );
        // Semantics: entity + p preserved, W from table (never input echo).
        let (ent, recs) = parse_authority(&bytes).unwrap();
        assert_eq!(ent, AVATAR);
        assert_eq!(
            recs.iter().map(|w| w.p).collect::<Vec<_>>(),
            vec![0x00, 0x00, 0x00, 0x01]
        );
        assert_eq!(
            recs.iter().map(|w| (w.x, w.y, w.w)).collect::<Vec<_>>(),
            vec![
                (22970, 122378, 150000),
                (23472, 122378, 150100),
                (23472, 122880, 150100),
                (23472, 122880, 150100),
            ]
        );
        // Step-then-emit: avatar advanced to the arrival snap.
        assert_eq!((av.x, av.y), (23472, 122880));
        // HP store default 2540.0 in the foot (E1-assumption constant).
        assert_eq!(av.hp_raw(), HP_DEFAULT_RAW);
        assert_eq!(m.meters.hp_divergences, 0);
        // Lag policy metered: histogram recorded, nothing tuned.
        assert_eq!(m.meters.lag_histogram, vec![10]);
        assert_eq!(m.meters.drain_counts, vec![4]);
    }

    #[test]
    fn still_frame_emits_dw_zero() {
        let mut m = fixture_model();
        let mut av = AvatarState::spawn(AVATAR, &m.spawn);
        // p01 arrival already at the avatar: no motion, still emits dW=0.
        let raw = raw_0734(AVATAR, &[(0x01, 22970, 122378, 150000)]);
        let rep = parse_report(&raw).unwrap();
        assert!(m.validate(&rep, &prev_at(22970, 122378)).accepted());
        let bytes = m.step_then_emit(
            &mut av,
            &rep.waypoints,
            &StepObs {
                queue_depth_before: 1,
                lag_frames: 8,
            },
        );
        let (_, recs) = parse_authority(&bytes).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!((recs[0].x, recs[0].y, recs[0].w), (22970, 122378, 150000));
        assert_eq!((av.x, av.y), (22970, 122378));
    }

    #[test]
    fn directed_gate_fires_on_crossing_ignores_radial_straddle() {
        let g = Gate::new(23600, 122400, 23600, 123400, 1, 0);
        // Perpendicular crossing in the blocked (+x) direction: fires.
        assert!(g.crossed_by(23500, 122500, 23700, 122500));
        // Same-segment reverse crossing (−x, unblocked direction): silent.
        assert!(!g.crossed_by(23700, 122500, 23500, 122500));
        // Radial straddle: both endpoints near the gate midpoint on the same
        // (west) side — a radial model would misfire on the distance, the
        // directed gate stays silent (no plane crossing).
        assert!(!g.crossed_by(23550, 122500, 23590, 122600));
        // Strafe parallel to the gate face (dot == 0): silent even on touch.
        assert!(!g.crossed_by(23600, 122500, 23600, 122700));
        // Pass fully south of the gate span: silent.
        assert!(!g.crossed_by(23500, 122350, 23700, 122350));
    }

    #[test]
    fn slice_json_contract_parses() {
        let text = r#"{"heightfield": [[22970,122378,150000],[23472,122378,150100],[23472,122880,150100]],
            "bbox": [22000,121000,24500,124000], "spawn": [22970,122378],
            "blocked": [[23600,122400,23600,123400,1,0]]}"#;
        let mut m = MovementModel::from_slice_json(text, AVATAR).unwrap();
        assert_eq!(m.table.len(), 3);
        assert_eq!(m.bbox, fixture_bbox());
        assert_eq!(
            m.spawn,
            Spawn {
                x: 22970,
                y: 122378,
                w: 0
            }
        );
        assert_eq!(m.gates.len(), 1);
        assert!(m
            .validate(
                &parse_report(&raw_0734(AVATAR, &walk_burst())).unwrap(),
                &prev_at(22970, 122378)
            )
            .accepted());
    }

    #[test]
    fn slice_json_rejects_bad_shapes() {
        assert!(MovementModel::from_slice_json("{}", AVATAR).is_err());
        assert!(MovementModel::from_slice_json(
            r#"{"heightfield": [], "bbox": [0,0,1,1], "spawn": [0,0]}"#,
            AVATAR
        )
        .is_err());
        assert!(MovementModel::from_slice_json(
            r#"{"heightfield": [[0,0]], "bbox": [0,0,1,1], "spawn": [0,0]}"#,
            AVATAR
        )
        .is_err());
    }

    #[test]
    fn fifo_backlog_depth_metered_never_fast_forwarded() {
        let mut m = fixture_model();
        assert!(m.backlog.is_empty());
        for _ in 0..3 {
            m.enqueue(parse_report(&raw_0734(AVATAR, &[(0x00, 22970, 122378, 150000)])).unwrap());
        }
        assert_eq!(m.backlog.len(), 3);
        assert_eq!(m.meters.queue_depth_samples, vec![1, 2, 3]);
        // Drain is strictly FIFO: reports come out in enqueue order.
        let first = m.backlog.pop_front().unwrap();
        assert_eq!(first.waypoints[0].x, 22970);
        assert_eq!(m.backlog.len(), 2);
    }
}

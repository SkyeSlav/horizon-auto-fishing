//! Auto Fishing for Horizon (native .hmod): reels in when a fish bites,
//! then casts again — with break protection and hotbar/inventory rod
//! swapping.
//!
//! The game thread calls [`tick`] once per client tick on a server, and
//! [`command`] for the mod's own commands. All state lives behind one
//! mutex; the hooks in `lib.rs` do the game's I/O (settings files, the
//! UseItem packet, chat lines, container clicks).
//!
//! The idea is the same one as the well-known auto-fishing mods: watch
//! for the bite, reel on it, recast after a delay, don't break rods.
//! This is a clean-room reimplementation in Rust — no code from any
//! other mod is used.
//!
//! The mod never casts on its own. A session starts only when the
//! player casts a rod by hand; after that the mod reels, recasts,
//! swaps rods and recovers a lost bobber until the player reels in
//! by hand (or turns the mod off), which ends the session.
//!
//! How this build differs from the .hnmod (Starling 0.8.0): the mod API
//! has no client read of the bobber — no nibble flag, no bobber entity
//! list on a remote client — so the bite comes from the splash sound
//! (`minecraft:entity.fishing_bobber.splash`, heard through
//! `play_level_sound_at_position` and noted with [`note_bite`]). The
//! sound cannot prove whose bobber made it, so the first splash after
//! a cast anchors the bobber's landing spot (in range, in the cast
//! direction) and only a later splash near that anchor counts as a
//! bite; a bobber lost with no bite is recovered by a long wait in
//! `Waiting` instead of by seeing the bobber vanish. Reel-in and
//! recast go out as `UseItem` packets through
//! `ClientHost::send_packet`.

use std::sync::{LazyLock, Mutex, MutexGuard};

pub const ID: &str = "auto_fishing";
pub const NAME: &str = "Auto Fishing";
pub const VERSION: &str = "1.0.1";
pub const DESCRIPTION: &str = "Reels in when a fish bites and casts again, with break protection, inventory rod swapping and human-like delays.";

/// Recast-delay choices the settings page cycles through, in ticks.
const RECAST_DELAYS: [u64; 7] = [0, 5, 10, 17, 25, 40, 60];
/// Catch-delay choices the settings page cycles through, in ticks.
const CATCH_DELAYS: [u64; 5] = [0, 2, 5, 10, 20];
/// Jitter-range choices the settings page cycles through, in ticks.
const JITTER_MAXES: [u64; 5] = [0, 2, 5, 10, 20];

/// Settings keys, one file per key under the mod's data folder.
const KEY_ENABLED: &str = "enabled";
const KEY_MODE: &str = "mode";
const KEY_RECAST_DELAY: &str = "recast_delay";
const KEY_CATCH_DELAY: &str = "catch_delay";
const KEY_JITTER_ENABLED: &str = "jitter_enabled";
const KEY_JITTER_MAX: &str = "jitter_max";
const KEY_JUNK_ENABLED: &str = "junk_enabled";
const KEY_JUNK: &str = "junk";

/// Modes, mirroring the behaviour of the originals:
/// 1 = current rod, no break protection;
/// 2 = current rod, with break protection;
/// 3 = all hotbar rods, no break protection;
/// 4 = all hotbar rods, with break protection.
/// Modes 3 and 4 also look for rods in the rest of the inventory.
const MODE_NAMES: [&str; 4] = [
    "Current rod",
    "Current rod + break protection",
    "All hotbar rods",
    "All hotbar rods + break protection",
];

/// With break protection on, a rod is not used once 5 or fewer
/// durability remain — it is swapped for a fresh rod (modes 3 and 4)
/// or left alone (mode 2).
const PROTECT_KEEP: i32 = 5;

/// A splash heard within this many ticks of a cast is a candidate for
/// the bobber landing, not a bite: a cast's bobber lands within about
/// 100 ticks, and a real bite waits at least 100 ticks after that.
/// A splash later than this after a cast cannot be the landing, so it
/// sets no anchor and is ignored.
const ANCHOR_WINDOW_TICKS: u64 = 100;
/// A bite counts only this many ticks after the bobber landed: the
/// lure's wait is 100-600 ticks, then 20-80 ticks to the hook.
const BITE_EARLIEST_TICKS: u64 = 100;
/// A landing splash is accepted as this session's bobber only within
/// this many blocks of where the player cast from (a cast lands within
/// roughly 30 blocks).
const CAST_RANGE: f64 = 32.0;
/// A landing splash must also fall inside this cone around the cast
/// direction (half-angle, in cos): the bobber arcs, drops below the
/// eye-level ray and drifts, so the cone is wide, but a splash from a
/// fisher casting another way is not this bobber's landing.
const CAST_CONE_COS: f64 = 0.766; // cos(40 degrees)
/// A bite splash must land within this many blocks of the recorded
/// landing position: a bobber stays put in still water, so a splash
/// farther out is another bobber, not a bite on this line.
const ANCHOR_RADIUS: f64 = 3.0;
/// Waiting this long with no bite means the bobber is lost (the server
/// killed it, or the cast never landed in water): cast again. The mod
/// API has no bobber read on a client, so this wait is the only
/// lost-bobber signal this build has.
const WAITING_TIMEOUT: u64 = 1200;
/// A noted manual reel-in is forgotten if the line is still believed
/// out this many ticks later (the use never reached the server).
const MANUAL_REEL_TIMEOUT: u64 = 10;

/// The mod's settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub enabled: bool,
    pub mode: u8,
    pub recast_delay: u64,
    pub catch_delay: u64,
    /// Human-like delays: each delay gets a random extra 0..=jitter_max ticks.
    pub jitter_enabled: bool,
    pub jitter_max: u64,
    /// Throw away listed junk items automatically (off by default).
    pub junk_enabled: bool,
    pub junk: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: 2,
            recast_delay: 17,
            catch_delay: 0,
            jitter_enabled: true,
            jitter_max: 5,
            junk_enabled: false,
            junk: Vec::new(),
        }
    }
}

impl Config {
    fn all_rods(&self) -> bool {
        self.mode >= 3
    }

    fn protect(&self) -> bool {
        self.mode % 2 == 0
    }

    /// Whether `item` is on the junk list, ignoring a `minecraft:` prefix.
    fn is_junk(&self, item: &str) -> bool {
        let item = item.strip_prefix("minecraft:").unwrap_or(item);
        self.junk.iter().any(|j| {
            j.strip_prefix("minecraft:").unwrap_or(j.as_str()) == item
        })
    }

    fn load(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        let bool = |k: &str, default: bool| match get(k).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => default,
        };
        let num = |k: &str, default: u64| {
            get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)
        };
        let mode = num(KEY_MODE, d.mode as u64);
        let junk = get(KEY_JUNK)
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .take(32)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            enabled: bool(KEY_ENABLED, d.enabled),
            mode: if (1..=4).contains(&mode) { mode as u8 } else { d.mode },
            recast_delay: num(KEY_RECAST_DELAY, d.recast_delay).min(100),
            catch_delay: num(KEY_CATCH_DELAY, d.catch_delay).min(20),
            jitter_enabled: bool(KEY_JITTER_ENABLED, d.jitter_enabled),
            jitter_max: num(KEY_JITTER_MAX, d.jitter_max).min(20),
            junk_enabled: bool(KEY_JUNK_ENABLED, d.junk_enabled),
            junk,
        }
    }

    fn stored(&self) -> Vec<(&'static str, String)> {
        vec![
            (KEY_ENABLED, self.enabled.to_string()),
            (KEY_MODE, self.mode.to_string()),
            (KEY_RECAST_DELAY, self.recast_delay.to_string()),
            (KEY_CATCH_DELAY, self.catch_delay.to_string()),
            (KEY_JITTER_ENABLED, self.jitter_enabled.to_string()),
            (KEY_JITTER_MAX, self.jitter_max.to_string()),
            (KEY_JUNK_ENABLED, self.junk_enabled.to_string()),
            (KEY_JUNK, self.junk.join(",")),
        ]
    }

    /// The settings page's buttons, in order.
    fn buttons(&self) -> Vec<String> {
        vec![
            format!("Auto Fishing: {}", if self.enabled { "On" } else { "Off" }),
            format!("Mode: {} - {}", self.mode, MODE_NAMES[(self.mode - 1) as usize]),
            format!("Recast delay: {} ticks", self.recast_delay),
            format!("Catch delay: {} ticks", self.catch_delay),
            if self.jitter_enabled {
                format!("Human-like delays: On (up to {} ticks)", self.jitter_max)
            } else {
                "Human-like delays: Off".to_string()
            },
            format!("Jitter range: {} ticks", self.jitter_max),
            format!(
                "Junk filter: {} ({} items)",
                if self.junk_enabled { "On" } else { "Off" },
                self.junk.len()
            ),
        ]
    }
}

/// A fishing rod the adapter saw: its damage and its maximum durability.
/// `max_damage` 0 means unbreakable (or unknown) — always usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rod {
    pub damage: i32,
    pub max_damage: i32,
}

/// One slot holding a rod.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RodSlot {
    pub slot: usize,
    pub rod: Rod,
}

/// One inventory slot holding a junk-listed item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JunkSlot {
    pub slot: usize,
    pub item: String,
}

/// What the adapter saw this tick. There is no bobber in here: the mod
/// API has no client read of the bobber, so the line's state lives in
/// the engine (set by the player's own casts and reels and by the
/// mod's uses), and the bite arrives separately through [`note_bite`].
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// The rod in the hand that acts, if it holds one.
    pub held: Option<Rod>,
    /// The rods in hotbar slots 0..9, in slot order.
    pub hotbar: Vec<RodSlot>,
    /// The rods in the rest of the inventory, slots 9..36.
    pub inventory_rods: Vec<RodSlot>,
    /// Every stack in the inventory, in slot order, for the junk filter.
    pub junk: Vec<JunkSlot>,
    /// The selected hotbar slot.
    pub selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No session: the mod does nothing, not even cast.
    Idle,
    /// The player cast by hand; the bobber has not shown up yet.
    Awaiting,
    /// The line is out, waiting for the bite.
    Waiting,
    ReelPending { due: u64 },
    Recast { due: u64 },
}

struct State {
    config: Config,
    /// Settings writes the adapter still has to persist.
    store: Vec<(String, String)>,
    tick: u64,
    phase: Phase,
    /// Whether a session is running (started by the player's cast).
    session: bool,
    /// Whether the engine believes the line is out.
    line_out: bool,
    /// A bite heard since the last tick, waiting to be acted on.
    pending_bite: bool,
    /// The tick the line last went out (a cast by hand or a recast).
    last_cast_tick: u64,
    /// The tick the current wait for a bite started.
    waiting_since: u64,
    /// Where the player's cast went out from, and the look direction
    /// it went in (a unit vector), for the landing-splash check.
    cast_origin: [f64; 3],
    cast_dir: [f64; 3],
    /// Where this session's bobber landed, from its landing splash:
    /// a later splash counts as a bite only near here. None until a
    /// landing splash is heard, and no bite is accepted without one.
    anchor: Option<[f64; 3]>,
    /// The tick the anchor was set (the landing tick).
    anchor_tick: u64,
    /// The UseItem sequence, counted up per use.
    sequence: i32,
    /// Whether the no-usable-rod warning was already shown this idle stretch.
    warned_no_rod: bool,
    /// The player reeled in by hand; the line is expected to be gone.
    manual_reel: bool,
    manual_reel_tick: u64,
    /// The deterministic jitter RNG's state (xorshift64star).
    rng: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            config: Config::default(),
            store: Vec::new(),
            tick: 0,
            phase: Phase::Idle,
            session: false,
            line_out: false,
            pending_bite: false,
            last_cast_tick: 0,
            waiting_since: 0,
            cast_origin: [0.0; 3],
            cast_dir: [0.0, 0.0, 1.0],
            anchor: None,
            anchor_tick: 0,
            sequence: 0,
            warned_no_rod: false,
            manual_reel: false,
            manual_reel_tick: 0,
            rng: 0x9E3779B97F4A7C15,
        }
    }
}

/// Which rod a cast uses, and what the adapter has to do first.
struct CastPlan {
    select_slot: Option<usize>,
    /// (inventory slot holding the rod, hotbar slot to swap it into)
    swap: Option<(usize, usize)>,
}

impl State {
    fn queue_config(&mut self) {
        for (k, v) in self.config.stored() {
            self.store.push((k.to_string(), v));
        }
    }

    /// Whether this rod may still be used under the current mode: with
    /// break protection on, only while more than 5 durability remain.
    fn usable(&self, rod: Rod) -> bool {
        if rod.max_damage <= 0 {
            return true;
        }
        if rod.damage >= rod.max_damage {
            return false;
        }
        !self.config.protect() || rod.damage + PROTECT_KEEP < rod.max_damage
    }

    /// The least-damaged usable rod, hotbar first on a tie: (slot, from inventory).
    fn best_rod(&self, snap: &Snapshot) -> Option<(usize, bool)> {
        snap.hotbar
            .iter()
            .map(|s| (s.slot, false, s.rod))
            .chain(snap.inventory_rods.iter().map(|s| (s.slot, true, s.rod)))
            .filter(|(_, _, rod)| self.usable(*rod))
            .min_by_key(|(_, _, rod)| rod.damage)
            .map(|(slot, from_inventory, _)| (slot, from_inventory))
    }

    /// Which rod a cast in this session uses, if any.
    fn plan_cast(&self, snap: &Snapshot) -> Option<CastPlan> {
        if snap.held.is_some_and(|r| self.usable(r)) {
            return Some(CastPlan {
                select_slot: None,
                swap: None,
            });
        }
        if !self.config.all_rods() {
            return None;
        }
        let (slot, from_inventory) = self.best_rod(snap)?;
        if from_inventory {
            Some(CastPlan {
                select_slot: None,
                swap: Some((slot, snap.selected)),
            })
        } else {
            Some(CastPlan {
                select_slot: if slot != snap.selected {
                    Some(slot)
                } else {
                    None
                },
                swap: None,
            })
        }
    }

    /// A delay with the human-like jitter: base plus a random 0..=max.
    fn jittered(&mut self, base: u64) -> u64 {
        if !self.config.jitter_enabled || self.config.jitter_max == 0 {
            return base;
        }
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        base + x.wrapping_mul(0x2545F4914F6CDD1D) % (self.config.jitter_max + 1)
    }

    fn end_session(&mut self) {
        self.session = false;
        self.line_out = false;
        self.manual_reel = false;
        self.phase = Phase::Idle;
        self.warned_no_rod = false;
        self.anchor = None;
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read the settings from the game's files.
pub fn load(get: &dyn Fn(&str) -> Option<String>) {
    state().config = Config::load(get);
}

/// Settings writes the adapter still has to persist.
pub fn take_store() -> Vec<(String, String)> {
    std::mem::take(&mut state().store)
}

/// What a tick does: whether to use the rod (cast or reel), the UseItem
/// sequence for it, a hotbar slot to select first, an inventory rod to
/// swap into the hand first, a junk slot to throw away, and any chat lines.
#[derive(Default, Debug)]
pub struct TickOut {
    pub use_rod: bool,
    pub sequence: i32,
    /// Whether the use is a (re)cast, rather than a reel-in: the
    /// adapter records the cast's origin and direction for it, so the
    /// landing splash can anchor the new bobber.
    pub cast_out: bool,
    pub select_slot: Option<usize>,
    pub swap: Option<(usize, usize)>,
    pub drop_slot: Option<usize>,
    pub lines: Vec<String>,
}

impl TickOut {
    fn use_now(s: &mut State) -> TickOut {
        s.sequence += 1;
        TickOut {
            use_rod: true,
            sequence: s.sequence,
            cast_out: false,
            select_slot: None,
            swap: None,
            drop_slot: None,
            lines: Vec::new(),
        }
    }
}

/// The player's own right-click with the rod, noted by the adapter's
/// use hook: `reeling` when the line was out (the player reeled in),
/// a cast otherwise. `origin` is where the player stood and `dir` the
/// unit look vector of the cast, for the landing-splash check. The
/// mod's own uses never come through here — they go out as packets —
/// so they are never mistaken for the player's.
pub fn note_player_use(reeling: bool, origin: [f64; 3], dir: [f64; 3]) {
    let mut s = state();
    if !s.config.enabled {
        return;
    }
    if reeling {
        s.manual_reel = true;
        s.manual_reel_tick = s.tick;
        s.line_out = false;
        s.anchor = None;
    } else {
        // A cast by hand: a session starts, and the pending recast (if
        // any) is cancelled — the player's cast replaces it. The line
        // is out from here: the mod API has no bobber read, so the
        // engine tracks the line from the uses themselves.
        s.session = true;
        s.phase = Phase::Awaiting;
        s.line_out = true;
        s.last_cast_tick = s.tick;
        s.warned_no_rod = false;
        s.cast_origin = origin;
        s.cast_dir = dir;
        s.anchor = None;
    }
}

/// A cast the mod itself sent (a recast): the adapter records where
/// it went out from and in which direction, so the new bobber's
/// landing splash can anchor it. Called only once the packet is out.
pub fn note_cast(origin: [f64; 3], dir: [f64; 3]) {
    let mut s = state();
    if !s.session {
        return;
    }
    s.cast_origin = origin;
    s.cast_dir = dir;
    s.anchor = None;
}

/// Whether the engine believes the line is out. The adapter reads this
/// to tell the player's own cast (line not out) from a reel-in (line
/// out), since the mod API has no bobber read.
pub fn line_out() -> bool {
    state().line_out
}

/// A splash at `position` (the bobber's splash sound, heard through
/// the adapter's sound hook). The mod API has no client read of the
/// bobber, so the sound cannot prove whose bobber made it — instead
/// the first splash after a cast anchors the bobber, and only a later
/// splash near that anchor counts as a bite:
///
/// - with no anchor yet, a splash within the anchor window that lands
///   within range and inside the cast-direction cone is taken as this
///   bobber's landing, and becomes the anchor; anything else is
///   ignored, so a splash from another fisher's bobber nearby does
///   not reel this line;
/// - with an anchor, a splash counts as a bite only within a few
///   blocks of the anchor and at least 100 ticks after the landing.
///   No anchor, no bite — there is no fall back to accepting any
///   splash in range.
///
/// The tick consumes a bite once, so one splash reels once.
pub fn note_bite(position: [f64; 3]) {
    let mut s = state();
    if !s.config.enabled || !s.session {
        return;
    }
    if s.phase != Phase::Waiting {
        return;
    }
    match s.anchor {
        None => {
            // No anchor yet: only a splash soon after the cast, in
            // range and in the cast direction, can be this bobber
            // landing.
            if s.tick - s.last_cast_tick > ANCHOR_WINDOW_TICKS {
                return;
            }
            let dx = position[0] - s.cast_origin[0];
            let dy = position[1] - s.cast_origin[1];
            let dz = position[2] - s.cast_origin[2];
            if dx * dx + dy * dy + dz * dz > CAST_RANGE * CAST_RANGE {
                return;
            }
            if dy > 3.0 {
                return;
            }
            // The cone, on the horizontal plane: a splash almost
            // straight below the cast needs no direction.
            let hx = dx;
            let hz = dz;
            let hlen = (hx * hx + hz * hz).sqrt();
            if hlen >= 1.0 {
                let dir_h = (s.cast_dir[0] * s.cast_dir[0]
                    + s.cast_dir[2] * s.cast_dir[2])
                    .sqrt();
                if dir_h > 0.0 {
                    let cos = (hx * s.cast_dir[0] + hz * s.cast_dir[2]) / (hlen * dir_h);
                    if cos < CAST_CONE_COS {
                        return;
                    }
                }
            }
            s.anchor = Some(position);
            s.anchor_tick = s.tick;
        }
        Some(anchor) => {
            if s.tick - s.anchor_tick < BITE_EARLIEST_TICKS {
                return;
            }
            let dx = position[0] - anchor[0];
            let dy = position[1] - anchor[1];
            let dz = position[2] - anchor[2];
            if dx * dx + dy * dy + dz * dz <= ANCHOR_RADIUS * ANCHOR_RADIUS {
                s.pending_bite = true;
            }
        }
    }
}

/// A cast the adapter could not send (the rod swap or the slot change
/// failed, or the packet was refused): the line never went out, so try
/// again after the recast delay instead of waiting for a bite that
/// cannot come.
pub fn cast_failed() {
    let mut s = state();
    if !s.session {
        return;
    }
    s.line_out = false;
    s.anchor = None;
    let delay = s.config.recast_delay;
    let due = s.tick + s.jittered(delay);
    s.phase = Phase::Recast { due };
}

/// Reset the session, e.g. on joining a server.
pub fn reset() {
    state().end_session();
}

/// Cast the rod for a recast, if a usable rod is there.
fn cast_for_recast(s: &mut State, snap: &Snapshot) -> TickOut {
    let mut out = TickOut::default();
    match s.plan_cast(snap) {
        Some(plan) => {
            out = TickOut::use_now(s);
            out.cast_out = true;
            out.select_slot = plan.select_slot;
            out.swap = plan.swap;
            s.line_out = true;
            s.last_cast_tick = s.tick;
            s.phase = Phase::Waiting;
            s.waiting_since = s.tick;
            s.warned_no_rod = false;
            // The old anchor belongs to the old bobber; the new cast
            // earns its own from its landing splash.
            s.anchor = None;
        }
        None => {
            // No usable rod: wait out another recast delay, then try again.
            let delay = s.config.recast_delay;
            let due = s.tick + s.jittered(delay);
            s.phase = Phase::Recast { due };
            if !s.warned_no_rod {
                s.warned_no_rod = true;
                out.lines.push(
                    "[AutoFish] No usable fishing rod: hold a rod, or turn break protection off."
                        .to_string(),
                );
            }
        }
    }
    out
}

/// Reel in on the bite, after the catch delay if there is one.
fn on_bite(s: &mut State, out: &mut TickOut) {
    // The bite is reeled in, so the bobber is gone: the anchor dies
    // with it, and the next cast earns a fresh one.
    s.anchor = None;
    if s.config.catch_delay == 0 {
        *out = TickOut::use_now(s);
        s.line_out = false;
        let delay = s.config.recast_delay;
        let due = s.tick + s.jittered(delay);
        s.phase = Phase::Recast { due };
    } else {
        let delay = s.config.catch_delay;
        let due = s.tick + s.jittered(delay);
        s.phase = Phase::ReelPending { due };
    }
}

/// One client tick on a server.
pub fn tick(snap: Snapshot) -> TickOut {
    let mut out = TickOut::default();
    let mut s = state();
    s.tick += 1;
    if !s.config.enabled {
        s.phase = Phase::Idle;
        s.session = false;
        s.line_out = false;
        s.pending_bite = false;
        s.warned_no_rod = false;
        s.manual_reel = false;
        s.anchor = None;
        return out;
    }
    // A noted manual reel-in that never took effect was not a reel
    // after all (the use never reached the server): forget it.
    if s.manual_reel && s.tick - s.manual_reel_tick > MANUAL_REEL_TIMEOUT {
        s.manual_reel = false;
    }
    // A bite heard since the last tick: consumed here, once.
    let biting = std::mem::take(&mut s.pending_bite);

    match s.phase {
        Phase::Idle => {
            // No session: do nothing. Without a bobber read, a line
            // the mod did not see cast cannot be adopted — the player
            // casts again by hand to start.
        }
        Phase::Awaiting => {
            // The player's cast put the line out; wait for the bite.
            s.phase = Phase::Waiting;
            s.waiting_since = s.tick;
            if biting {
                on_bite(&mut s, &mut out);
            }
        }
        Phase::Waiting => {
            if s.manual_reel {
                // The player reeled in by hand: the session ends, and
                // the mod must not recast until they cast again.
                s.end_session();
            } else if biting {
                on_bite(&mut s, &mut out);
            } else if s.tick - s.waiting_since > WAITING_TIMEOUT {
                // No bite for a long while: the bobber is lost (the
                // server killed it, or the cast never landed in water).
                // Cast again after the delay.
                s.line_out = false;
                let delay = s.config.recast_delay;
                let due = s.tick + s.jittered(delay);
                s.phase = Phase::Recast { due };
            }
        }
        Phase::ReelPending { due } => {
            if s.manual_reel {
                s.end_session();
            } else if s.tick >= due {
                out = TickOut::use_now(&mut s);
                s.line_out = false;
                let delay = s.config.recast_delay;
                let due = s.tick + s.jittered(delay);
                s.phase = Phase::Recast { due };
            }
            // With a manual reel noted, never reel again here: the
            // player's reel is already on its way, and another use
            // would cast a fresh line instead.
        }
        Phase::Recast { due } => {
            if s.tick >= due {
                out = cast_for_recast(&mut s, &snap);
            }
        }
    }
    // The junk filter runs on its own, session or not: one item per
    // tick from the first listed stack, never while a rod swap is
    // happening this tick (one container action per tick).
    if out.swap.is_none()
        && s.config.junk_enabled
        && !s.config.junk.is_empty()
        && let Some(j) = snap.junk.iter().find(|j| s.config.is_junk(&j.item))
    {
        out.drop_slot = Some(j.slot);
    }
    out
}

/// What a manual command does: any chat lines for the player.
#[derive(Default)]
pub struct CommandOut {
    pub lines: Vec<String>,
}

/// The usages `register_client_commands` declares, one per form
/// `command()` answers. Kept here, next to the parser, so a test can
/// check the chat tree stays in sync with what the mod actually runs.
/// Argument kinds are the ones Starling 0.9.0's `Command` offers
/// (`integer`, `word`); `Command::new` makes each level 0, the only
/// level a client command may have.
pub const COMMAND_USAGES: &[&str] = &[
    "autofish",
    "autofish status",
    "autofish help",
    "autofish toggle",
    "autofish mode <mode:integer>",
    "autofish delay <ticks:integer>",
    "autofish catch <ticks:integer>",
    "autofish jitter <ticks:integer>",
    "autofish jitter <state:word>",
    "autofish jitter",
    "autofish junk",
    "autofish junk on",
    "autofish junk off",
    "autofish junk add <item:word>",
    "autofish junk remove <item:word>",
    "autofish junk list",
    "autofish junk clear",
    "autofish junk preset",
];

const HELP: &[&str] = &[
    "Cast a rod by hand to start — the mod never casts on its own. Reel in by hand to stop.",
    "/autofish - status",
    "/autofish toggle - turn auto fishing on/off",
    "/autofish mode <1-4> - 1 current rod, 2 + break protection, 3 all hotbar + inventory rods, 4 + break protection",
    "/autofish delay <ticks> - wait before recasting after a catch (0-100, default 17)",
    "/autofish catch <ticks> - wait after the bite before reeling in (0-20, default 0)",
    "/autofish jitter <ticks> - random extra delay, up to N ticks (0-20, default 5)",
    "/autofish jitter on/off - human-like delays on/off",
    "/autofish junk on/off - throw away listed junk (default off)",
    "/autofish junk add <item> / remove <item> / list / clear / preset - manage the junk list",
    "/autofish status - mode, delays, junk, whether a session is running",
];

fn on_off(b: bool) -> &'static str {
    if b {
        "On"
    } else {
        "Off"
    }
}

/// A command the player typed, without its slash. None when it is not
/// one of this mod's commands.
pub fn command(line: &str) -> Option<CommandOut> {
    let line = line.strip_prefix('/').unwrap_or(line);
    let mut words = line.split_whitespace();
    if words.next()? != "autofish" {
        return None;
    }
    let mut out = CommandOut::default();
    let mut s = state();
    let args: Vec<&str> = words.collect();
    match args.as_slice() {
        [] | ["status"] => {
            out.lines.push(format!(
                "[AutoFish] {} - mode {} ({}), recast delay {} ticks, catch delay {} ticks, jitter {} (up to {} ticks), junk filter {} ({} items), {}",
                on_off(s.config.enabled),
                s.config.mode,
                MODE_NAMES[(s.config.mode - 1) as usize],
                s.config.recast_delay,
                s.config.catch_delay,
                on_off(s.config.jitter_enabled),
                s.config.jitter_max,
                on_off(s.config.junk_enabled),
                s.config.junk.len(),
                if s.session {
                    "session running — cast by hand to start a new one"
                } else {
                    "no session — cast a rod by hand to start"
                },
            ));
        }
        ["help"] => out.lines.extend(HELP.iter().map(|l| l.to_string())),
        ["toggle"] => {
            s.config.enabled = !s.config.enabled;
            if !s.config.enabled {
                s.phase = Phase::Idle;
                s.session = false;
                s.line_out = false;
                s.manual_reel = false;
                s.anchor = None;
            }
            s.queue_config();
            out.lines.push(format!(
                "[AutoFish] Auto fishing {}.",
                on_off(s.config.enabled)
            ));
        }
        ["mode", n] => match n.parse::<u8>() {
            Ok(m) if (1..=4).contains(&m) => {
                s.config.mode = m;
                s.queue_config();
                out.lines.push(format!(
                    "[AutoFish] Mode {} ({}).",
                    m, MODE_NAMES[(m - 1) as usize]
                ));
            }
            _ => out.lines.push(
                "[AutoFish] Mode is 1-4: 1 current rod, 2 + break protection, 3 all hotbar + inventory rods, 4 + break protection."
                    .to_string(),
            ),
        },
        ["delay", n] => match n.parse::<u64>() {
            Ok(d) if d <= 100 => {
                s.config.recast_delay = d;
                s.queue_config();
                out.lines.push(format!("[AutoFish] Recast delay: {d} ticks."));
            }
            _ => out.lines.push(
                "[AutoFish] Recast delay is 0-100 ticks (default 17).".to_string(),
            ),
        },
        ["catch", n] => match n.parse::<u64>() {
            Ok(d) if d <= 20 => {
                s.config.catch_delay = d;
                s.queue_config();
                out.lines.push(format!("[AutoFish] Catch delay: {d} ticks."));
            }
            _ => out.lines.push(
                "[AutoFish] Catch delay is 0-20 ticks (default 0).".to_string(),
            ),
        },
        ["jitter", n] => match *n {
            "on" => {
                s.config.jitter_enabled = true;
                s.queue_config();
                out.lines.push("[AutoFish] Human-like delays on.".to_string());
            }
            "off" => {
                s.config.jitter_enabled = false;
                s.queue_config();
                out.lines.push("[AutoFish] Human-like delays off.".to_string());
            }
            _ => match n.parse::<u64>() {
                Ok(d) if d <= 20 => {
                    s.config.jitter_max = d;
                    s.queue_config();
                    out.lines
                        .push(format!("[AutoFish] Jitter range: up to {d} ticks."));
                }
                _ => out.lines.push(
                    "[AutoFish] Jitter is 0-20 ticks (default 5), or on/off.".to_string(),
                ),
            },
        },
        ["jitter"] => {
            s.config.jitter_enabled = !s.config.jitter_enabled;
            s.queue_config();
            out.lines.push(format!(
                "[AutoFish] Human-like delays {}.",
                on_off(s.config.jitter_enabled)
            ));
        }
        ["junk", "on"] => {
            s.config.junk_enabled = true;
            s.queue_config();
            out.lines.push(format!(
                "[AutoFish] Junk filter on ({} items listed).",
                s.config.junk.len()
            ));
        }
        ["junk", "off"] => {
            s.config.junk_enabled = false;
            s.queue_config();
            out.lines
                .push("[AutoFish] Junk filter off.".to_string());
        }
        ["junk", "add", item] => {
            let item = item.trim().to_string();
            if item.is_empty() {
                out.lines.push(
                    "[AutoFish] Give an item, e.g. /autofish junk add minecraft:rotten_flesh."
                        .to_string(),
                );
            } else if s.config.junk.iter().any(|j| j == &item) {
                out.lines
                    .push(format!("[AutoFish] {item} is already on the junk list."));
            } else {
                s.config.junk.push(item.clone());
                s.queue_config();
                out.lines
                    .push(format!("[AutoFish] Added {item} to the junk list."));
            }
        }
        ["junk", "remove", item] => {
            let before = s.config.junk.len();
            s.config.junk.retain(|j| j != item);
            if s.config.junk.len() == before {
                out.lines
                    .push(format!("[AutoFish] {item} is not on the junk list."));
            } else {
                s.queue_config();
                out.lines
                    .push(format!("[AutoFish] Removed {item} from the junk list."));
            }
        }
        ["junk", "list"] | ["junk"] => {
            if s.config.junk.is_empty() {
                out.lines.push(
                    "[AutoFish] Junk list is empty. Try /autofish junk preset, or add items with /autofish junk add <item>."
                        .to_string(),
                );
            } else {
                out.lines.push(format!(
                    "[AutoFish] Junk list ({}): {}.",
                    s.config.junk.len(),
                    s.config.junk.join(", ")
                ));
            }
        }
        ["junk", "clear"] => {
            s.config.junk.clear();
            s.queue_config();
            out.lines
                .push("[AutoFish] Junk list cleared.".to_string());
        }
        ["junk", "preset"] => {
            let mut added = Vec::new();
            for item in [
                "minecraft:rotten_flesh",
                "minecraft:lily_pad",
                "minecraft:leather_boots",
            ] {
                if !s.config.junk.iter().any(|j| j == item) {
                    s.config.junk.push(item.to_string());
                    added.push(item);
                }
            }
            s.queue_config();
            if added.is_empty() {
                out.lines.push(
                    "[AutoFish] The usual junk is already on the list.".to_string(),
                );
            } else {
                out.lines.push(format!(
                    "[AutoFish] Added to the junk list: {}. Turn it on with /autofish junk on.",
                    added.join(", ")
                ));
            }
        }
        _ => out.lines.extend(HELP.iter().map(|l| l.to_string())),
    }
    Some(out)
}

/// The settings page's buttons, in order.
pub fn settings_buttons() -> Vec<String> {
    state().config.buttons()
}

/// A click on the settings page: the button's setting moves to its next value.
pub fn settings_click(button: usize) {
    let mut s = state();
    match button {
        0 => {
            s.config.enabled = !s.config.enabled;
            if !s.config.enabled {
                s.phase = Phase::Idle;
                s.session = false;
                s.line_out = false;
                s.manual_reel = false;
                s.anchor = None;
            }
        }
        1 => s.config.mode = if s.config.mode >= 4 { 1 } else { s.config.mode + 1 },
        2 => {
            let cur = s.config.recast_delay;
            s.config.recast_delay = RECAST_DELAYS
                .iter()
                .find(|&&d| d > cur)
                .copied()
                .unwrap_or(RECAST_DELAYS[0]);
        }
        3 => {
            let cur = s.config.catch_delay;
            s.config.catch_delay = CATCH_DELAYS
                .iter()
                .find(|&&d| d > cur)
                .copied()
                .unwrap_or(CATCH_DELAYS[0]);
        }
        4 => s.config.jitter_enabled = !s.config.jitter_enabled,
        5 => {
            let cur = s.config.jitter_max;
            s.config.jitter_max = JITTER_MAXES
                .iter()
                .find(|&&d| d > cur)
                .copied()
                .unwrap_or(JITTER_MAXES[0]);
        }
        6 => s.config.junk_enabled = !s.config.junk_enabled,
        _ => return,
    }
    s.queue_config();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rod(damage: i32) -> Rod {
        Rod {
            damage,
            max_damage: 64,
        }
    }

    fn snap(held: Option<Rod>) -> Snapshot {
        Snapshot {
            held,
            hotbar: Vec::new(),
            inventory_rods: Vec::new(),
            junk: Vec::new(),
            selected: 0,
        }
    }

    fn reset() {
        let mut s = state();
        *s = State::default();
    }

    fn set_mode(mode: u8) {
        state().config.mode = mode;
    }

    fn set_delay(ticks: u64) {
        state().config.recast_delay = ticks;
    }

    fn no_jitter() {
        state().config.jitter_enabled = false;
    }

    /// Run `n` ticks with the same snapshot, returning the last output.
    fn wait_ticks(n: u64, held: Option<Rod>) -> TickOut {
        let mut out = TickOut::default();
        for _ in 0..n {
            out = tick(snap(held));
        }
        out
    }

    /// Start a session the way the game does: the player casts by hand.
    /// The cast goes out from the origin, looking along `DIR`.
    const ORIGIN: [f64; 3] = [0.0, 64.0, 0.0];
    const DIR: [f64; 3] = [0.0, 0.0, 1.0];
    /// Where the bobber lands: 10 blocks ahead of the cast, in range
    /// and in the cast direction.
    const LAND: [f64; 3] = [0.0, 64.0, 10.0];

    fn manual_cast(held: Option<Rod>) {
        note_player_use(false, ORIGIN, DIR);
        assert!(!tick(snap(held)).use_rod);
        assert!(state().session);
    }

    /// The bobber's landing splash: sets the anchor.
    fn land() {
        note_bite(LAND);
        assert_eq!(state().anchor, Some(LAND));
    }

    /// Hear the bite's splash (anchored, after the 100-tick wait) and
    /// run one tick, returning its output.
    fn hear_bite(held: Option<Rod>) -> TickOut {
        land();
        wait_ticks(105, held);
        note_bite(LAND);
        tick(snap(held))
    }

    #[test]
    fn idle_with_a_rod_casts_nothing() {
        reset();
        set_mode(1);
        // Enabled, rod held, no cast: the mod must not cast on its own.
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!state().session);
    }

    #[test]
    fn manual_cast_then_bite_reels_then_recasts() {
        reset();
        set_mode(1);
        set_delay(3);
        no_jitter();
        manual_cast(Some(rod(0)));
        // Line out, no bite yet: nothing.
        assert!(!wait_ticks(5, Some(rod(0))).use_rod);
        // The splash: reel in.
        let out = hear_bite(Some(rod(0)));
        assert!(out.use_rod);
        assert_eq!(out.sequence, 1);
        // The delay counts down, then recast.
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
    }

    #[test]
    fn manual_reel_in_ends_the_session() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // The player reels in by hand, no bite.
        note_player_use(true, ORIGIN, DIR);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!state().session);
        // And it stays stopped: no recast ever follows.
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        // Until the player casts again by hand.
        manual_cast(Some(rod(0)));
        assert!(hear_bite(Some(rod(0))).use_rod);
    }

    /// Hear the bite's splash when the anchor is already set (no new
    /// landing splash), and run one tick.
    fn hear_bite_no_land(held: Option<Rod>) -> TickOut {
        wait_ticks(105, held);
        note_bite(LAND);
        tick(snap(held))
    }

    #[test]
    fn bite_before_100_ticks_after_landing_is_ignored() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        land();
        // A splash near the anchor too soon is not a bite yet.
        wait_ticks(50, Some(rod(0)));
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        // 100 ticks after the landing, the same splash reels.
        wait_ticks(55, Some(rod(0)));
        note_bite(LAND);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn bite_far_from_anchor_is_ignored() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        land();
        wait_ticks(105, Some(rod(0)));
        // Another bobber splashing 10 blocks away: not this line.
        note_bite([0.0, 64.0, 20.0]);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
        // This bobber's own splash still reels.
        note_bite(LAND);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn anchor_candidate_outside_the_cone_is_ignored() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // A splash off to the side, 90 degrees from the cast: not
        // this bobber landing, so no anchor is set.
        note_bite([10.0, 64.0, 0.0]);
        assert_eq!(state().anchor, None);
        // And with no anchor, no bite is ever accepted.
        wait_ticks(105, Some(rod(0)));
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
    }

    #[test]
    fn first_splash_after_the_anchor_window_sets_no_anchor() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // The first splash arrives 105 ticks after the cast: too late
        // to be the landing, so it sets no anchor and reels nothing.
        wait_ticks(105, Some(rod(0)));
        note_bite(LAND);
        assert_eq!(state().anchor, None);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
    }

    #[test]
    fn anchor_cleared_on_reel_recast_and_manual_reel() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        land();
        // Reel on the bite: the bite clears the anchor (the bobber is
        // gone with the catch).
        assert!(hear_bite_no_land(Some(rod(0))).use_rod);
        assert_eq!(state().anchor, None);
        // The recast goes out, then a splash at the old spot only
        // anchors the new bobber — it does not reel.
        assert!(tick(snap(Some(rod(0)))).use_rod);
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert_eq!(state().anchor, Some(LAND));
        // Manual reel clears the anchor too (and ends the session).
        note_player_use(true, ORIGIN, DIR);
        assert_eq!(state().anchor, None);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!state().session);
    }

    #[test]
    fn long_wait_without_a_bite_recasts() {
        reset();
        set_mode(1);
        set_delay(2);
        no_jitter();
        manual_cast(Some(rod(0)));
        // No bite for a long while: the bobber is lost, so the mod
        // recasts after the wait, and the session keeps going.
        let mut recast = false;
        for _ in 0..1300 {
            if tick(snap(Some(rod(0)))).use_rod {
                recast = true;
                break;
            }
        }
        assert!(recast);
        assert!(state().session);
    }

    #[test]
    fn manual_reel_during_catch_delay_does_not_reel_again() {
        reset();
        set_mode(1);
        set_delay(0);
        state().config.catch_delay = 2;
        no_jitter();
        manual_cast(Some(rod(0)));
        land();
        wait_ticks(105, Some(rod(0)));
        // Bite: the catch delay starts counting.
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        // The player reels by hand during the delay: no second use from
        // the mod (that would cast a fresh line), and the session ends.
        note_player_use(true, ORIGIN, DIR);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!state().session);
    }

    #[test]
    fn toggle_off_ends_the_session() {
        reset();
        set_mode(1);
        manual_cast(Some(rod(0)));
        command("/autofish toggle").unwrap();
        assert!(!state().session);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn enabled_mid_cast_does_nothing_until_a_cast() {
        reset();
        set_mode(1);
        // The mod was enabled after the player had already cast: with
        // no bobber read, that line cannot be adopted — no bite is
        // acted on until the player casts again by hand.
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!state().session);
        manual_cast(Some(rod(0)));
        assert!(hear_bite(Some(rod(0))).use_rod);
    }

    #[test]
    fn a_second_splash_does_not_reel_twice() {
        reset();
        set_mode(1);
        set_delay(17);
        no_jitter();
        manual_cast(Some(rod(0)));
        assert!(hear_bite(Some(rod(0))).use_rod);
        // Already reeling/recasting: another splash does nothing.
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn splash_right_after_a_cast_is_the_landing_not_a_bite() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // A splash in the first ticks after the cast is the bobber
        // landing: it anchors the bobber, it does not reel.
        land();
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
        // The bite, heard 100 ticks after the landing, reels.
        assert!(hear_bite_no_land(Some(rod(0))).use_rod);
    }

    #[test]
    fn break_protection_stops_at_five_remaining() {
        reset();
        set_mode(2);
        // Damage 58 of 64: 6 left — still usable.
        manual_cast(Some(rod(58)));
        assert!(hear_bite(Some(rod(58))).use_rod);
        reset();
        set_mode(2);
        // Damage 59: 5 left — protected, no cast on recast.
        manual_cast(Some(rod(59)));
        assert!(hear_bite(Some(rod(59))).use_rod);
        set_delay(0);
        no_jitter();
        assert!(!tick(snap(Some(rod(59)))).use_rod);
    }

    #[test]
    fn without_protection_uses_a_worn_rod() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(63)));
        assert!(hear_bite(Some(rod(63))).use_rod);
        assert!(tick(snap(Some(rod(63)))).use_rod);
        reset();
        set_mode(1);
        // Already broken: never.
        assert!(!tick(snap(Some(rod(64)))).use_rod);
    }

    #[test]
    fn all_rods_mode_switches_to_a_fresh_hotbar_rod() {
        reset();
        set_mode(4);
        set_delay(0);
        no_jitter();
        let mut s = snap(Some(rod(59)));
        s.hotbar = vec![
            RodSlot { slot: 0, rod: rod(59) },
            RodSlot { slot: 3, rod: rod(5) },
        ];
        s.selected = 0;
        // The worn rod is held, so the manual cast is the player's; the
        // bite is reeled with it, then the recast swaps to the fresh rod.
        note_player_use(false, ORIGIN, DIR);
        tick(snap(Some(rod(59))));
        land();
        wait_ticks(105, Some(rod(59)));
        note_bite(LAND);
        assert!(tick(snap(Some(rod(59)))).use_rod);
        let out = tick(s);
        assert!(out.use_rod);
        assert_eq!(out.select_slot, Some(3));
        assert_eq!(out.swap, None);
    }

    #[test]
    fn inventory_rod_is_swapped_into_the_hand() {
        reset();
        set_mode(4);
        set_delay(0);
        no_jitter();
        // Worn rod held, fresh rod only in the main inventory (slot 12).
        let mut s = snap(Some(rod(59)));
        s.hotbar = vec![RodSlot { slot: 0, rod: rod(59) }];
        s.inventory_rods = vec![RodSlot { slot: 12, rod: rod(2) }];
        s.selected = 0;
        note_player_use(false, ORIGIN, DIR);
        tick(snap(Some(rod(59))));
        land();
        wait_ticks(105, Some(rod(59)));
        note_bite(LAND);
        assert!(tick(snap(Some(rod(59)))).use_rod);
        let out = tick(s);
        assert!(out.use_rod);
        assert_eq!(out.swap, Some((12, 0)));
        assert_eq!(out.select_slot, None);
    }

    #[test]
    fn current_rod_mode_never_switches() {
        reset();
        set_mode(2);
        set_delay(0);
        no_jitter();
        let mut s = snap(Some(rod(59)));
        s.hotbar = vec![
            RodSlot { slot: 0, rod: rod(59) },
            RodSlot { slot: 3, rod: rod(5) },
        ];
        s.inventory_rods = vec![RodSlot { slot: 12, rod: rod(2) }];
        s.selected = 0;
        note_player_use(false, ORIGIN, DIR);
        tick(snap(Some(rod(59))));
        land();
        wait_ticks(105, Some(rod(59)));
        note_bite(LAND);
        tick(snap(Some(rod(59))));
        let out = tick(s);
        assert!(!out.use_rod);
        assert_eq!(out.select_slot, None);
        assert_eq!(out.swap, None);
    }

    #[test]
    fn disabled_does_nothing() {
        reset();
        state().config.enabled = false;
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        note_player_use(false, ORIGIN, DIR);
        assert!(!state().session);
    }

    #[test]
    fn catch_delay_waits_before_reeling() {
        reset();
        set_mode(1);
        set_delay(0);
        state().config.catch_delay = 2;
        no_jitter();
        manual_cast(Some(rod(0)));
        land();
        wait_ticks(105, Some(rod(0)));
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn cast_failed_retries_after_the_delay() {
        reset();
        set_mode(1);
        set_delay(3);
        no_jitter();
        manual_cast(Some(rod(0)));
        assert!(hear_bite(Some(rod(0))).use_rod);
        // The recast goes out after the delay, but the adapter could not send it.
        let out = wait_ticks(3, Some(rod(0)));
        assert!(out.use_rod);
        cast_failed();
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
    }

    #[test]
    fn reset_clears_the_session() {
        reset();
        set_mode(1);
        manual_cast(Some(rod(0)));
        reset();
        assert!(!state().session);
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn jitter_stays_within_range_and_varies() {
        reset();
        set_mode(1);
        set_delay(10);
        state().config.jitter_enabled = true;
        state().config.jitter_max = 5;
        // Run many bite->reel->recast cycles and record each recast delay.
        let mut delays = Vec::new();
        for _ in 0..12 {
            manual_cast(Some(rod(0)));
            assert!(hear_bite(Some(rod(0))).use_rod);
            let mut waited = 0;
            loop {
                waited += 1;
                if tick(snap(Some(rod(0)))).use_rod {
                    break;
                }
                assert!(waited <= 20, "recast took too long");
            }
            delays.push(waited);
            // End the session so the next cycle starts clean.
            note_player_use(true, ORIGIN, DIR);
            tick(snap(Some(rod(0))));
        }
        assert!(delays.iter().all(|&d| (10..=15).contains(&d)));
        assert!(
            delays.iter().any(|&d| d != delays[0]),
            "jitter never varied: {delays:?}"
        );
    }

    #[test]
    fn jitter_off_gives_exact_delays() {
        reset();
        set_mode(1);
        set_delay(3);
        no_jitter();
        manual_cast(Some(rod(0)));
        assert!(hear_bite(Some(rod(0))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn junk_filter_drops_listed_items_only_when_on() {
        reset();
        let mut s = snap(Some(rod(0)));
        s.junk = vec![
            JunkSlot { slot: 10, item: "minecraft:rotten_flesh".into() },
            JunkSlot { slot: 11, item: "minecraft:diamond".into() },
        ];
        // Off by default: nothing dropped.
        assert_eq!(tick(s.clone()).drop_slot, None);
        // On, with rotten flesh listed: the first listed stack drops.
        state().config.junk_enabled = true;
        state().config.junk = vec!["minecraft:rotten_flesh".into()];
        assert_eq!(tick(s.clone()).drop_slot, Some(10));
        // Bare names match too.
        state().config.junk = vec!["rotten_flesh".into()];
        assert_eq!(tick(s).drop_slot, Some(10));
    }

    #[test]
    fn command_parses_mode_delay_jitter_and_junk() {
        reset();
        let out = command("/autofish mode 3").unwrap();
        assert_eq!(state().config.mode, 3);
        assert!(out.lines.iter().any(|l| l.contains("Mode 3")));
        command("/autofish delay 25").unwrap();
        assert_eq!(state().config.recast_delay, 25);
        command("/autofish jitter 12").unwrap();
        assert_eq!(state().config.jitter_max, 12);
        command("/autofish jitter off").unwrap();
        assert!(!state().config.jitter_enabled);
        command("/autofish jitter").unwrap();
        assert!(state().config.jitter_enabled);
        command("/autofish junk add minecraft:rotten_flesh").unwrap();
        command("/autofish junk preset").unwrap();
        assert!(state().config.junk.contains(&"minecraft:lily_pad".to_string()));
        assert_eq!(state().config.junk.len(), 3);
        command("/autofish junk remove minecraft:lily_pad").unwrap();
        assert_eq!(state().config.junk.len(), 2);
        command("/autofish junk on").unwrap();
        assert!(state().config.junk_enabled);
        assert!(command("/autofish nope").unwrap().lines.iter().any(|l| l.contains("/autofish")));
        assert!(command("/autologin").is_none());
    }

    #[test]
    fn registered_usages_are_answered() {
        // Every usage the chat tree declares must be one `command()`
        // answers, or typing it would complete and then do nothing.
        reset();
        for usage in COMMAND_USAGES {
            let sample = usage
                .replace("<mode:integer>", "2")
                .replace("<ticks:integer>", "5")
                .replace("<state:word>", "on")
                .replace("<item:word>", "minecraft:rotten_flesh");
            assert!(
                command(&format!("/{sample}")).is_some(),
                "registered usage not answered: {usage}"
            );
        }
    }

    #[test]
    fn settings_buttons_cycle() {
        reset();
        settings_click(1);
        assert_eq!(state().config.mode, 3);
        settings_click(1);
        assert_eq!(state().config.mode, 4);
        settings_click(1);
        assert_eq!(state().config.mode, 1);
        settings_click(2);
        assert_eq!(state().config.recast_delay, 25);
        settings_click(4);
        assert!(!state().config.jitter_enabled);
        settings_click(5);
        assert_eq!(state().config.jitter_max, 10);
        settings_click(6);
        assert!(state().config.junk_enabled);
        settings_click(0);
        assert!(!state().config.enabled);
    }

    #[test]
    fn config_load_clamps_bad_values() {
        reset();
        load(&|k| match k {
            "mode" => Some("9".to_string()),
            "recast_delay" => Some("500".to_string()),
            "catch_delay" => Some("99".to_string()),
            "jitter_max" => Some("99".to_string()),
            "junk" => Some("minecraft:rotten_flesh,, minecraft:lily_pad".to_string()),
            _ => None,
        });
        assert_eq!(state().config.mode, 2);
        assert_eq!(state().config.recast_delay, 100);
        assert_eq!(state().config.catch_delay, 20);
        assert_eq!(state().config.jitter_max, 20);
        assert_eq!(
            state().config.junk,
            vec!["minecraft:rotten_flesh", "minecraft:lily_pad"]
        );
    }
}

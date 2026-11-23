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
//! sound cannot prove whose bobber made it, so a splash counts as a
//! bite only when it fits this session's own cast: in range, inside
//! the cast-direction cone, and at least 100 ticks after the cast.
//! There is no landing splash to anchor on — neither the engine nor a
//! server plays one when a bobber enters the water; the sound is the
//! bite's alone — so the cast itself is the anchor. A bobber lost
//! with no bite is recovered by a long wait in `Waiting` instead of
//! by seeing the bobber vanish. Reel-in and recast go out as
//! `UseItem` packets through `ClientHost::send_packet`.

use std::sync::{LazyLock, Mutex, MutexGuard};

pub const ID: &str = "auto_fishing";
pub const NAME: &str = "Auto Fishing";
pub const VERSION: &str = "1.1.0";
pub const DESCRIPTION: &str = "Automates fishing input: reels in on the bite and casts again, with break protection, inventory rod swapping and random delays. Many servers forbid automated fishing — check the rules first.";

/// Recast-delay choices the settings page cycles through, in ticks.
const RECAST_DELAYS: [u64; 7] = [0, 5, 10, 17, 25, 40, 60];
/// Catch-delay choices the settings page cycles through, in ticks.
const CATCH_DELAYS: [u64; 5] = [0, 2, 5, 10, 20];
/// Jitter-range choices the settings page cycles through, in ticks.
const JITTER_MAXES: [u64; 5] = [0, 2, 5, 10, 20];
/// Protect-at choices the settings page cycles through, in durability.
const PROTECT_ATS: [u64; 6] = [1, 2, 3, 5, 10, 20];

/// Settings keys, one file per key under the mod's data folder.
const KEY_ENABLED: &str = "enabled";
/// The mode as 1.0.3 kept it: the slider's number (1..4). 1.0.5 keeps
/// this key registered as a range only so a value saved by 1.0.3 can
/// still be read — the host answers None for a key no setting is
/// registered under — and migrates it to `KEY_MODE_V2` on load.
const KEY_MODE: &str = "mode";
/// The mode as the settings page's choice index (0..3), the key 1.0.5
/// introduced. The slider and the choice are two live controls for
/// the same mode and are kept in sync (see `resolve_mode`).
const KEY_MODE_V2: &str = "mode_v2";
const KEY_PROTECT_AT: &str = "protect_at";
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

/// With break protection on, a rod is not used once the protect-at
/// threshold (default 5) or fewer durability remain — it is swapped
/// for a fresh rod (modes 3 and 4) or left alone (mode 2). The
/// threshold is a setting, so the player can fish a rod down to its
/// last durability.
const PROTECT_AT_DEFAULT: u64 = 5;

/// A bite counts only this many ticks after the cast: the bobber
/// takes a few ticks to land and the lure's wait is 100-600 ticks,
/// then 20-80 ticks to the hook, so nothing this side of 100 ticks
/// after a cast can be this line's bite.
const BITE_EARLIEST_TICKS: u64 = 100;
/// A splash counts as this session's bite only within this many
/// blocks of where the player cast from (a cast lands within
/// roughly 30 blocks).
const CAST_RANGE: f64 = 32.0;
/// A bite splash must also fall inside this cone around the cast
/// direction (half-angle, in cos): the bobber arcs, drops below the
/// eye-level ray and drifts, so the cone is wide, but a splash from a
/// fisher casting another way is not this bobber's bite.
const CAST_CONE_COS: f64 = 0.766; // cos(40 degrees)
/// Waiting this long with no bite means the bobber is lost (the server
/// killed it, or the cast never landed in water): cast again. The mod
/// API has no bobber read on a client, so this wait is the only
/// lost-bobber signal this build has. 750 ticks is past the longest
/// real wait for a bite (lure wait 600 + hooked wait 80 = 680 ticks),
/// so it never fires on a live bobber — and a wiped bobber is
/// recast in ~37s instead of 60s.
const WAITING_TIMEOUT: u64 = 750;
/// A noted manual reel-in is forgotten if the line is still believed
/// out this many ticks later (the use never reached the server).
const MANUAL_REEL_TIMEOUT: u64 = 10;

/// The mod's settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub enabled: bool,
    pub mode: u8,
    /// Swap/stop using a rod once this much durability remains.
    pub protect_at: u64,
    pub recast_delay: u64,
    pub catch_delay: u64,
    /// Random delays: each delay gets a random extra 0..=jitter_max ticks.
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
            protect_at: PROTECT_AT_DEFAULT,
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

    /// The mode is passed in, already resolved from the two controls
    /// by [`load`]: the engine never reads it raw, so a value saved
    /// by 1.0.3's slider cannot read shifted the way 1.0.4's choice
    /// read did.
    fn load(get: &dyn Fn(&str) -> Option<String>, mode: u8) -> Self {
        let d = Self::default();
        let bool = |k: &str, default: bool| match get(k).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => default,
        };
        let num = |k: &str, default: u64| {
            get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)
        };
        let protect_at = num(KEY_PROTECT_AT, d.protect_at);
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
            mode,
            protect_at: if (1..=20).contains(&protect_at) {
                protect_at
            } else {
                d.protect_at
            },
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
            // Both mode keys, so the two controls stay in sync: the
            // slider keeps the mode number, the choice keeps the
            // option index (mode - 1). A command writes both at once.
            (KEY_MODE, self.mode.to_string()),
            (KEY_MODE_V2, (self.mode - 1).to_string()),
            (KEY_PROTECT_AT, self.protect_at.to_string()),
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
            format!("Protect at: {} durability", self.protect_at),
            format!("Recast delay: {} ticks", self.recast_delay),
            format!("Catch delay: {} ticks", self.catch_delay),
            if self.jitter_enabled {
                format!("Random delays: On (up to {} ticks)", self.jitter_max)
            } else {
                "Random delays: Off".to_string()
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
    /// The mode the two controls last read as, so `load` can tell
    /// which one the player just moved: the slider's number and the
    /// choice's index (+1), as modes 1..4. None until the first load
    /// of this session.
    seen_mode_legacy: Option<u8>,
    seen_mode_choice: Option<u8>,
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
    /// it went in (a unit vector): the cast is the anchor a later
    /// splash is checked against, since no landing splash exists
    /// to anchor the bobber on.
    cast_origin: [f64; 3],
    cast_dir: [f64; 3],
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
            seen_mode_legacy: None,
            seen_mode_choice: None,
            tick: 0,
            phase: Phase::Idle,
            session: false,
            line_out: false,
            pending_bite: false,
            last_cast_tick: 0,
            waiting_since: 0,
            cast_origin: [0.0; 3],
            cast_dir: [0.0, 0.0, 1.0],
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
    /// break protection on, only while more than `protect_at`
    /// durability remain.
    fn usable(&self, rod: Rod) -> bool {
        if rod.max_damage <= 0 {
            return true;
        }
        if rod.damage >= rod.max_damage {
            return false;
        }
        !self.config.protect() || rod.damage + (self.config.protect_at as i32) < rod.max_damage
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

    /// A delay with random jitter: base plus a random 0..=max.
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
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The mode from the two controls `load` just read, as modes 1..4:
/// `legacy` is the 1.0.3 slider's number under `mode`, `choice` the
/// 1.0.5 choice's index under `mode_v2` (+1). The second value is the
/// one key to sync afterwards, if any — the control that lost is
/// written to match the winner, so the two agree on the next load
/// and the migration never runs twice.
///
/// The slider and the choice are two live controls for the same mode,
/// so a load has to tell which one the player just moved, and the
/// first load of a session has to tell a 1.0.3 save from a later
/// edit:
///
/// - First load: the controls agree, or the choice is still its
///   default — the slider is the truth. That is the 1.0.3 migration:
///   a save from 1.0.3 holds only the slider (the choice never
///   existed), so the slider's number is the player's mode and the
///   choice is synced to it. When the controls disagree and the
///   choice is set, the choice wins: a choice edit made on the title
///   screen, or after a sync write was lost.
/// - Later loads: whichever control moved since last seen wins, and
///   the other is synced to it. Neither moved: the mode stays as the
///   engine has it (a command may have set it — commands write both
///   keys at once). Both moved at once cannot happen through the page
///   (each edit loads) or a command (it writes both consistently),
///   so the choice wins as the tiebreak.
///
/// Out-of-range or unparseable values clamp to a mode, never crash:
/// a slider below 1 reads as 1, above 4 as 4; a choice index above 3
/// as mode 4. A value stored by 1.0.4 (a choice index under `mode`)
/// is indistinguishable from a 1.0.3 slider value — a range stores
/// 2.0 as "2" and a choice stores index 2 as "2" — so 1.0.4 users
/// migrate as if it were 1.0.3 and should check the status line
/// once; 1.0.4 never reached the registry, so that is nobody on the
/// registry's population.
fn resolve_mode(
    seen_legacy: Option<u8>,
    seen_choice: Option<u8>,
    legacy: u8,
    choice: u8,
    current: u8,
) -> (u8, Option<(&'static str, String)>) {
    let sync_choice = |mode: u8| Some((KEY_MODE_V2, (mode - 1).to_string()));
    let sync_legacy = |mode: u8| Some((KEY_MODE, mode.to_string()));
    match (seen_legacy, seen_choice) {
        (None, None) => {
            if legacy == choice {
                (legacy, None)
            } else if choice == 2 {
                // The choice is still its default (index 1 = mode 2):
                // the slider is a 1.0.3 save migrating.
                (legacy, sync_choice(legacy))
            } else {
                (choice, sync_legacy(choice))
            }
        }
        (Some(seen_legacy), Some(seen_choice)) => {
            let legacy_moved = legacy != seen_legacy;
            let choice_moved = choice != seen_choice;
            match (legacy_moved, choice_moved) {
                (true, false) => (legacy, sync_choice(legacy)),
                (false, true) => (choice, sync_legacy(choice)),
                (true, true) => {
                    if legacy == choice {
                        (legacy, None)
                    } else {
                        (choice, sync_legacy(choice))
                    }
                }
                (false, false) => (current, None),
            }
        }
        // Unreachable: the two are always set together.
        _ => (current, None),
    }
}

/// Read the settings from the game's files. Runs at the start of
/// every tick and on every command and setting change, before any
/// automation runs, so a migrated mode is in force from the first
/// tick after the update.
pub fn load(get: &dyn Fn(&str) -> Option<String>) {
    let mut s = state();
    // The slider's number (1..4), defaulting to mode 2 when nothing
    // was ever stored; the choice's index (+1), defaulting to mode 2.
    let legacy = get(KEY_MODE)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(2.0)
        .clamp(1.0, 4.0) as u8;
    let choice = get(KEY_MODE_V2)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .min(3) as u8
        + 1;
    let (mode, sync) = resolve_mode(
        s.seen_mode_legacy,
        s.seen_mode_choice,
        legacy,
        choice,
        s.config.mode,
    );
    s.seen_mode_legacy = Some(legacy);
    s.seen_mode_choice = Some(choice);
    if let Some((key, value)) = sync {
        s.store.push((key.to_string(), value));
    }
    s.config = Config::load(get, mode);
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
    /// adapter records the cast's origin and direction for it, so a
    /// later splash can be checked against the cast.
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
/// unit look vector of the cast, for the bite check. The
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
    }
}

/// A cast the mod itself sent (a recast): the adapter records where
/// it went out from and in which direction, so a later splash can be
/// checked against it. Called only once the packet is out.
pub fn note_cast(origin: [f64; 3], dir: [f64; 3]) {
    let mut s = state();
    if !s.session {
        return;
    }
    s.cast_origin = origin;
    s.cast_dir = dir;
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
/// the splash is checked against this session's own cast:
///
/// - at least 100 ticks after the cast (a real bite cannot come
///   sooner: the bobber takes a few ticks to land and the lure
///   waits at least 100 ticks after that);
/// - within range of where the player cast from, and inside the
///   cast-direction cone (the bobber lands along the cast ray).
///
/// A splash that fails any of those is ignored, so a splash from
/// another fisher's bobber — behind the player, off to the side, or
/// too soon after the cast — does not reel this line. No cast, no
/// bite: without a cast on record there is nothing to check against.
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
    // Too soon after the cast to be this line's bite.
    if s.tick - s.last_cast_tick < BITE_EARLIEST_TICKS {
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
    // The cone, on the horizontal plane: a splash almost straight
    // below the cast needs no direction.
    let hx = dx;
    let hz = dz;
    let hlen = (hx * hx + hz * hz).sqrt();
    if hlen >= 1.0 {
        let dir_h = (s.cast_dir[0] * s.cast_dir[0] + s.cast_dir[2] * s.cast_dir[2]).sqrt();
        if dir_h > 0.0 {
            let cos = (hx * s.cast_dir[0] + hz * s.cast_dir[2]) / (hlen * dir_h);
            if cos < CAST_CONE_COS {
                return;
            }
        }
    }
    s.pending_bite = true;
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
    // The junk filter runs on its own, session or not: one whole
    // stack per tick from the first listed stack, never while a rod
    // swap is happening this tick (one container action per tick).
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
    "autofish protect <durability:integer>",
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
    "/autofish protect <durability> - swap/stop using a rod once this much durability remains (1-20, default 5)",
    "/autofish delay <ticks> - wait before recasting after a catch (0-100, default 17)",
    "/autofish catch <ticks> - wait after the bite before reeling in (0-20, default 0)",
    "/autofish jitter <ticks> - random extra delay, up to N ticks (0-20, default 5)",
    "/autofish jitter on/off - random delays on/off",
    "/autofish junk on/off - throw away listed junk, whole stacks (default off)",
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
                "[AutoFish] {} - mode {} ({}), protect at {} durability, recast delay {} ticks, catch delay {} ticks, jitter {} (up to {} ticks), junk filter {} ({} items), {}",
                on_off(s.config.enabled),
                s.config.mode,
                MODE_NAMES[(s.config.mode - 1) as usize],
                s.config.protect_at,
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
        ["protect", n] => match n.parse::<u64>() {
            Ok(d) if (1..=20).contains(&d) => {
                s.config.protect_at = d;
                s.queue_config();
                out.lines.push(format!(
                    "[AutoFish] Protect at: {d} durability remaining."
                ));
            }
            _ => out.lines.push(
                "[AutoFish] Protect at is 1-20 durability (default 5).".to_string(),
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
                out.lines.push("[AutoFish] Random delays on.".to_string());
            }
            "off" => {
                s.config.jitter_enabled = false;
                s.queue_config();
                out.lines.push("[AutoFish] Random delays off.".to_string());
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
                "[AutoFish] Random delays {}.",
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
            }
        }
        1 => s.config.mode = if s.config.mode >= 4 { 1 } else { s.config.mode + 1 },
        2 => {
            let cur = s.config.protect_at;
            s.config.protect_at = PROTECT_ATS
                .iter()
                .find(|&&d| d > cur)
                .copied()
                .unwrap_or(PROTECT_ATS[0]);
        }
        3 => {
            let cur = s.config.recast_delay;
            s.config.recast_delay = RECAST_DELAYS
                .iter()
                .find(|&&d| d > cur)
                .copied()
                .unwrap_or(RECAST_DELAYS[0]);
        }
        4 => {
            let cur = s.config.catch_delay;
            s.config.catch_delay = CATCH_DELAYS
                .iter()
                .find(|&&d| d > cur)
                .copied()
                .unwrap_or(CATCH_DELAYS[0]);
        }
        5 => s.config.jitter_enabled = !s.config.jitter_enabled,
        6 => {
            let cur = s.config.jitter_max;
            s.config.jitter_max = JITTER_MAXES
                .iter()
                .find(|&&d| d > cur)
                .copied()
                .unwrap_or(JITTER_MAXES[0]);
        }
        7 => s.config.junk_enabled = !s.config.junk_enabled,
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

    /// Hear the bite's splash (at the landing spot, after the
    /// 100-tick wait from the cast) and run one tick, returning its
    /// output. No landing splash is heard first: none exists.
    fn hear_bite(held: Option<Rod>) -> TickOut {
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

    /// Hear the bite's splash after the 100-tick wait from the cast,
    /// and run one tick.
    fn hear_bite_after_wait(held: Option<Rod>) -> TickOut {
        wait_ticks(105, held);
        note_bite(LAND);
        tick(snap(held))
    }

    #[test]
    fn bite_before_100_ticks_after_cast_is_ignored() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // A splash at the landing spot too soon after the cast is
        // not a bite yet.
        wait_ticks(50, Some(rod(0)));
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        // 100 ticks after the cast, the same splash reels.
        wait_ticks(55, Some(rod(0)));
        note_bite(LAND);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn bite_off_the_cast_line_is_ignored() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        wait_ticks(105, Some(rod(0)));
        // Another bobber splashing 10 blocks to the side, off the
        // cast line: not this line.
        note_bite([10.0, 64.0, 10.0]);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
        // This bobber's own splash still reels.
        note_bite(LAND);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn bite_beyond_cast_range_is_ignored() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        wait_ticks(105, Some(rod(0)));
        // A splash 40 blocks out, past the cast's range: not this line.
        note_bite([0.0, 64.0, 40.0]);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
        // This bobber's own splash still reels.
        note_bite(LAND);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn first_bite_after_the_cast_reels_without_a_landing_splash() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // No landing splash is ever heard — none exists — and the
        // first splash, 105 ticks after the cast at the landing
        // spot, still reels. (1.0.1 needed that splash to anchor
        // first, so it missed the bite.)
        wait_ticks(105, Some(rod(0)));
        note_bite(LAND);
        assert!(tick(snap(Some(rod(0)))).use_rod);
    }

    #[test]
    fn recast_re_anchors_on_the_new_cast() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // Reel on the bite.
        assert!(hear_bite_after_wait(Some(rod(0))).use_rod);
        // The recast goes out, and a splash at the old spot too soon
        // after the new cast does not reel.
        assert!(tick(snap(Some(rod(0)))).use_rod);
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        // 100 ticks after the recast, the same splash reels.
        wait_ticks(105, Some(rod(0)));
        note_bite(LAND);
        assert!(tick(snap(Some(rod(0)))).use_rod);
        // The recast goes out, then a manual reel ends the session.
        assert!(tick(snap(Some(rod(0)))).use_rod);
        note_player_use(true, ORIGIN, DIR);
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
        for _ in 0..800 {
            if tick(snap(Some(rod(0)))).use_rod {
                recast = true;
                break;
            }
        }
        assert!(recast);
        assert!(state().session);
    }

    #[test]
    fn lost_bobber_recasts_within_750_ticks() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // Nothing for 700 ticks: the bobber may still be out, and no
        // recast may fire before the 750-tick lost-bobber wait ends.
        assert!(!wait_ticks(700, Some(rod(0))).use_rod);
        // The wait ends at 750 ticks: a recast follows, and the
        // session keeps going.
        let mut recast = false;
        for _ in 0..60 {
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
    fn splash_right_after_a_cast_is_not_a_bite() {
        reset();
        set_mode(1);
        set_delay(0);
        no_jitter();
        manual_cast(Some(rod(0)));
        // A splash in the first ticks after the cast cannot be this
        // line's bite yet: it is ignored, and the session keeps going.
        note_bite(LAND);
        assert!(!tick(snap(Some(rod(0)))).use_rod);
        assert!(state().session);
        // The bite, heard 100 ticks after the cast, reels.
        assert!(hear_bite_after_wait(Some(rod(0))).use_rod);
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
    fn protect_threshold_is_configurable() {
        reset();
        set_mode(2);
        set_delay(0);
        no_jitter();
        // Damage 59 of 64: 5 left — protected at the default 5, so no
        // recast with the worn rod.
        manual_cast(Some(rod(59)));
        assert!(hear_bite(Some(rod(59))).use_rod);
        assert!(!tick(snap(Some(rod(59)))).use_rod);
        // Set the threshold to 1: the same rod is usable again.
        command("/autofish protect 1").unwrap();
        assert_eq!(state().config.protect_at, 1);
        assert!(tick(snap(Some(rod(59)))).use_rod);
        // But 1 durability left is still protected at 1.
        reset();
        set_mode(2);
        set_delay(0);
        no_jitter();
        command("/autofish protect 1").unwrap();
        manual_cast(Some(rod(63)));
        assert!(hear_bite(Some(rod(63))).use_rod);
        assert!(!tick(snap(Some(rod(63)))).use_rod);
        // And 0 is refused, 21 is refused.
        command("/autofish protect 0").unwrap();
        assert_eq!(state().config.protect_at, 1);
        command("/autofish protect 21").unwrap();
        assert_eq!(state().config.protect_at, 1);
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
        command("/autofish protect 1").unwrap();
        assert_eq!(state().config.protect_at, 1);
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
                .replace("<durability:integer>", "1")
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
        assert_eq!(state().config.protect_at, 10);
        settings_click(3);
        assert_eq!(state().config.recast_delay, 25);
        settings_click(5);
        assert!(!state().config.jitter_enabled);
        settings_click(6);
        assert_eq!(state().config.jitter_max, 10);
        settings_click(7);
        assert!(state().config.junk_enabled);
        settings_click(0);
        assert!(!state().config.enabled);
    }

    /// A fake host settings store for the migration tests: `load`
    /// reads from it, and the queued store writes go back into it the
    /// way the adapter's persist would.
    fn host_store(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn apply_store(map: &mut std::collections::HashMap<String, String>) {
        for (k, v) in take_store() {
            map.insert(k, v);
        }
    }

    /// Every mode a 1.0.3 slider could have saved migrates to the
    /// same mode under the choice key, and the second load changes
    /// nothing: the migration runs once and never shifts again.
    #[test]
    fn legacy_mode_values_migrate_to_the_same_mode() {
        for (saved, mode) in [("1", 1u8), ("2", 2), ("3", 3), ("4", 4)] {
            reset();
            let mut map = host_store(&[("mode", saved)]);
            load(&|k| map.get(k).cloned());
            assert_eq!(state().config.mode, mode, "saved {saved}");
            // The choice key is synced to the migrated mode — except
            // mode 2, which the choice already defaults to.
            if mode == 2 {
                assert!(take_store().is_empty(), "saved {saved}: no sync needed");
            } else {
                assert!(
                    take_store()
                        .iter()
                        .any(|(k, v)| k == "mode_v2" && v == &(mode - 1).to_string()),
                    "saved {saved}: expected a mode_v2 sync write"
                );
                apply_store(&mut map);
            }
            // The next load sees both controls agreeing: no more
            // writes, and the mode does not shift again.
            load(&|k| map.get(k).cloned());
            assert_eq!(state().config.mode, mode, "saved {saved}, second load");
            assert!(take_store().is_empty(), "saved {saved}: second load wrote");
        }
    }

    #[test]
    fn no_saved_mode_defaults_to_mode_2() {
        reset();
        let map = host_store(&[]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 2);
        assert!(take_store().is_empty());
    }

    #[test]
    fn choice_change_wins_over_the_slider() {
        reset();
        let mut map = host_store(&[("mode", "2"), ("mode_v2", "1")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 2);
        assert!(take_store().is_empty());
        // The player moves the choice to mode 4: the slider is synced.
        map.insert("mode_v2".to_string(), "3".to_string());
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 4);
        assert!(take_store().iter().any(|(k, v)| k == "mode" && v == "4"));
        apply_store(&mut map);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 4);
        assert!(take_store().is_empty());
    }

    #[test]
    fn slider_change_wins_over_the_choice() {
        reset();
        let mut map = host_store(&[("mode", "2"), ("mode_v2", "1")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 2);
        // The player moves the slider to 4: the choice is synced.
        map.insert("mode".to_string(), "4".to_string());
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 4);
        assert!(take_store().iter().any(|(k, v)| k == "mode_v2" && v == "3"));
        apply_store(&mut map);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 4);
        assert!(take_store().is_empty());
    }

    #[test]
    fn first_load_with_a_set_choice_favours_the_choice() {
        reset();
        // The slider says 2, the choice says mode 4: a choice edit
        // that landed while the slider was stale (title screen, or a
        // lost sync write). The choice is the newer control, so it wins.
        let map = host_store(&[("mode", "2"), ("mode_v2", "3")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 4);
        assert!(take_store().iter().any(|(k, v)| k == "mode" && v == "4"));
    }

    #[test]
    fn out_of_range_mode_values_clamp() {
        reset();
        let map = host_store(&[("mode", "9")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 4);
        reset();
        let map = host_store(&[("mode", "0")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 1);
        reset();
        let map = host_store(&[("mode", "abc")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 2);
        reset();
        let map = host_store(&[("mode_v2", "9")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 4);
    }

    #[test]
    fn command_mode_writes_both_keys() {
        reset();
        let map = host_store(&[("mode", "2"), ("mode_v2", "1")]);
        load(&|k| map.get(k).cloned());
        assert_eq!(state().config.mode, 2);
        let _ = take_store();
        let out = command("/autofish mode 4").expect("mode command");
        assert!(out.lines.iter().any(|l| l.contains("Mode 4")));
        let stored = take_store();
        assert!(stored.iter().any(|(k, v)| k == "mode" && v == "4"));
        assert!(stored.iter().any(|(k, v)| k == "mode_v2" && v == "3"));
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
        // A slider above 4 clamps to mode 4, not the default.
        assert_eq!(state().config.mode, 4);
        assert_eq!(state().config.recast_delay, 100);
        assert_eq!(state().config.catch_delay, 20);
        assert_eq!(state().config.jitter_max, 20);
        assert_eq!(
            state().config.junk,
            vec!["minecraft:rotten_flesh", "minecraft:lily_pad"]
        );
    }
}

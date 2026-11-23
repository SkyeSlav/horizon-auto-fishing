//! Auto Fishing for Horizon (native .hmod): the hooks that feed the
//! pure-logic engine in `engine.rs`.
//!
//! The engine keeps all state behind one mutex; the hooks do the game's I/O:
//! - `client_tick_pre` reads the local player's inventory from the open
//!   menu, runs the engine, and acts on its answer: hotbar selection,
//!   rod swaps and junk throws through menu clicks, reel-in/recast
//!   through a raw `UseItem` packet;
//! - `play_level_sound_at_position` hears the bobber's splash and notes
//!   it to the engine — the mod API has no client read of the bobber, so
//!   the splash sound is the bite signal (the .hnmod reads a nibble flag
//!   instead), and the engine checks the splash against the cast that
//!   went out (in range, in the cast direction, at least 100 ticks
//!   after it) so a nearby fisher's splash does not reel this line;
//!   there is no landing splash to anchor on — the sound is the
//!   bite's alone;
//! - `right_click_item` notes the player's own casts and reels, so a
//!   session starts only on a cast by hand and ends on a reel by hand;
//! - `command` answers `/autofish`;
//! - `register_client_commands` declares `/autofish` and its usages to
//!   the chat tree, so typing it colours and completes like the game's
//!   own commands instead of showing red as unknown;
//! - `register` declares the eight settings on the mod's page;
//! - `client_player_logging_in` resets the session per connection.
//!
//! The seven registered settings hold the values the settings page
//! edits. The junk list lives in the mod's data files
//! (`horizon-mod-data/auto_fishing/`): `junk` holds the comma-joined
//! list, so the commands can still change it.

use mod_api::{
    self, ClientHost, ClientPlayerLoggingInEvent, ClientTickPreEvent, Command, CommandEvent,
    ConfigPath, Flow, Hooks, HostSide, Identifier, ItemKey, MenuClick,
    PlayLevelSoundAtPositionEvent, RegisterClientCommandsEvent, RegistrationContext, Registrar,
    RightClickItemEvent, ServerboundPlayPacket, SettingChangedEvent, SettingSpec, SwapWith, Text,
};
use std::sync::Mutex;

mod engine;

/// The settings the mod's page edits; the junk list lives in the
/// `junk` data file, so the commands can still change it.
const REGISTERED_SETTINGS: [&str; 8] = [
    "enabled",
    "mode",
    "protect_at",
    "recast_delay",
    "catch_delay",
    "jitter_enabled",
    "jitter_max",
    "junk_enabled",
];

const JUNK_FILE: &str = "junk";

/// The player's own inventory menu, as the client shows it: 46 slots
/// (crafting result, grid and armour, then the main inventory and the
/// hotbar).
const INVENTORY_MENU_SLOTS: usize = 46;

/// The `UseItem` packet's hand: the main hand.
const MAIN_HAND: u32 = 0;

#[derive(Default)]
struct Mod {
    /// Whether a failed packet send has been told to the player this
    /// session, so a refused send does not spam chat every tick.
    send_failed: Mutex<bool>,
}

/// One data file as text, if it exists and is UTF-8.
fn read_data(host: &dyn ClientHost, name: &str) -> Option<String> {
    let path = ConfigPath::new(name).ok()?;
    let bytes = host.data_files().read(&path).ok()?;
    String::from_utf8(bytes).ok()
}

/// Replace one data file with `text`.
fn write_data(host: &dyn ClientHost, name: &str, text: &str) {
    if let Ok(path) = ConfigPath::new(name) {
        let _ = host.data_files().write(&path, text.as_bytes());
    }
}

/// Read the settings and data files into the engine. The registered settings
/// come from the host's typed reads; the junk list comes from its data file.
fn load(host: &dyn ClientHost) {
    let junk = read_data(host, JUNK_FILE);
    engine::load(&|key| match key {
        "enabled" => host.setting_bool(key).map(|b| b.to_string()),
        "jitter_enabled" => host.setting_bool(key).map(|b| b.to_string()),
        "junk_enabled" => host.setting_bool(key).map(|b| b.to_string()),
        // The mode is a choice on the settings page: the host keeps
        // the option index, and the engine's modes are index + 1.
        "mode" => host.setting_choice(key).map(|i| format!("{}", i)),
        "protect_at" => host.setting_f64(key).map(|f| format!("{}", f as u64)),
        "recast_delay" => host.setting_f64(key).map(|f| format!("{}", f as u64)),
        "catch_delay" => host.setting_f64(key).map(|f| format!("{}", f as u64)),
        "jitter_max" => host.setting_f64(key).map(|f| format!("{}", f as u64)),
        "junk" => junk.clone(),
        _ => None,
    });
}

/// Write the engine's queued store writes: registered settings through the
/// host, the junk list through its data file.
fn persist(host: &dyn ClientHost) {
    let store = engine::take_store();
    if store.is_empty() {
        return;
    }
    for (key, value) in store {
        if key == JUNK_FILE {
            write_data(host, JUNK_FILE, &value);
        } else if REGISTERED_SETTINGS.contains(&key.as_str()) {
            host.set_setting(engine::ID, &key, &value);
        }
    }
}

/// A `minecraft:` id, or None when it does not parse.
fn ident(text: &str) -> Option<Identifier> {
    Identifier::parse(text).ok()
}

/// The fishing rod's item key, if the registry knows it.
fn rod_key(host: &dyn ClientHost) -> Option<ItemKey> {
    host.item_key(&ident("minecraft:fishing_rod")?)
}

/// An item's id (`minecraft:rotten_flesh`), for the junk list.
fn item_id(host: &dyn ClientHost, key: ItemKey) -> Option<String> {
    Some(key.id(host))
}

/// An engine inventory slot (0..9 hotbar, 9..36 the rest) as a slot of
/// the player's inventory menu: the hotbar sits at 36..45, the main
/// inventory at 9..36.
fn menu_slot(slot: usize) -> usize {
    if slot < 9 {
        36 + slot
    } else {
        slot
    }
}

/// What the adapter saw this tick: the rods and junk of the local
/// player's inventory, read from the open menu. Empty when no player
/// is in a world, or when another menu is open (a chest, a furnace):
/// that menu's slots are not the player's inventory, so nothing is
/// read from it.
fn snapshot(host: &dyn ClientHost) -> engine::Snapshot {
    let mut snap = engine::Snapshot::default();
    let Some(player) = host.local_player() else {
        return snap;
    };
    let Some(selected) = player.selected_slot(&*host) else {
        return snap;
    };
    snap.selected = selected;
    let Some(menu) = player.container_menu(&*host) else {
        return snap;
    };
    if menu.slot_count(&*host) != Some(INVENTORY_MENU_SLOTS) {
        return snap;
    }
    let Some(rod) = rod_key(host) else {
        return snap;
    };
    for slot in 0..36usize {
        let Some(Some(stack)) = menu.slot(&*host, menu_slot(slot)) else {
            continue;
        };
        if let Some(item) = item_id(host, stack.item()) {
            snap.junk.push(engine::JunkSlot { slot, item });
        }
        if stack.item() == rod {
            let rod = engine::Rod {
                damage: stack.damage() as i32,
                max_damage: host.max_damage(stack.item()).unwrap_or(0) as i32,
            };
            if slot < 9 {
                snap.hotbar.push(engine::RodSlot { slot, rod });
            } else {
                snap.inventory_rods.push(engine::RodSlot { slot, rod });
            }
            if slot == selected {
                snap.held = Some(rod);
            }
        }
    }
    snap
}

/// Whether the player holds a fishing rod in the selected slot.
fn held_is_rod(host: &dyn ClientHost) -> bool {
    let Some(player) = host.local_player() else {
        return false;
    };
    let Some(selected) = player.selected_slot(&*host) else {
        return false;
    };
    let Some(menu) = player.container_menu(&*host) else {
        return false;
    };
    if menu.slot_count(&*host) != Some(INVENTORY_MENU_SLOTS) {
        return false;
    }
    let Some(rod) = rod_key(host) else {
        return false;
    };
    matches!(menu.slot(&*host, menu_slot(selected)), Some(Some(stack)) if stack.item() == rod)
}

/// The player's look (yaw, pitch) from the camera, for the `UseItem`
/// packet's aim; (0, 0) when the camera cannot be read.
fn look(host: &dyn ClientHost) -> (f32, f32) {
    let Some(camera) = host.camera_entity() else {
        return (0.0, 0.0);
    };
    let Some(rotation) = camera.rotation(&*host) else {
        return (0.0, 0.0);
    };
    (rotation[0], rotation[1])
}

/// The camera's position; (0, 0, 0) when it cannot be read.
fn camera_pos(host: &dyn ClientHost) -> [f64; 3] {
    host.camera_entity()
        .and_then(|camera| camera.position(&*host))
        .unwrap_or([0.0; 3])
}

/// The look direction as a unit vector, from the camera's yaw and
/// pitch (yaw 0 faces +Z, pitch positive looks down). Used for the
/// bite check: the bobber lands in the cast direction.
fn look_dir(host: &dyn ClientHost) -> [f64; 3] {
    let (yaw, pitch) = look(host);
    let (yaw, pitch) = (yaw.to_radians(), pitch.to_radians());
    [
        (-yaw.sin() * pitch.cos()) as f64,
        (-pitch.sin()) as f64,
        (yaw.cos() * pitch.cos()) as f64,
    ]
}

/// A Minecraft protocol VarInt (LEB128, seven bits a byte).
fn write_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Send the reel-in / recast as a raw `UseItem` packet: main hand,
/// the engine's sequence, and the player's look. False when the
/// packet was refused; the first refusal of a session is told to the
/// player once, the rest stay silent.
fn send_use_item(host: &mut dyn ClientHost, sequence: i32, reported: &Mutex<bool>) -> bool {
    let Some(id) = ident("minecraft:use_item") else {
        return false;
    };
    let (yaw, pitch) = look(host);
    // Vanilla `ServerboundUseItemPacket`: VarInt hand, VarInt sequence,
    // big-endian float yaw, big-endian float pitch.
    let mut body = Vec::with_capacity(16);
    write_varint(&mut body, MAIN_HAND);
    write_varint(&mut body, sequence as u32);
    body.extend_from_slice(&yaw.to_be_bytes());
    body.extend_from_slice(&pitch.to_be_bytes());
    match host.send_packet(ServerboundPlayPacket::new(id, body)) {
        Ok(()) => true,
        Err(error) => {
            let mut reported = reported.lock().unwrap_or_else(|e| e.into_inner());
            if !*reported {
                *reported = true;
                let _ = host.chat_line(Text::literal(format!(
                    "[AutoFish] Could not send the fishing packet: {error:?}"
                )));
            }
            false
        }
    }
}

/// Swap the rod in inventory slot `from` into hotbar slot `to`, after
/// checking both slots still hold what the snapshot said they hold.
/// False when anything does not match, or the click was refused.
fn do_swap(host: &mut dyn ClientHost, snap: &engine::Snapshot, from: usize, to: usize) -> bool {
    let Some(player) = host.local_player() else {
        return false;
    };
    let Some(menu) = player.container_menu(&*host) else {
        return false;
    };
    if menu.slot_count(&*host) != Some(INVENTORY_MENU_SLOTS) {
        return false;
    }
    let Some(expected) = snap.inventory_rods.iter().find(|r| r.slot == from) else {
        return false;
    };
    let Some(rod) = rod_key(host) else {
        return false;
    };
    let source_ok = matches!(menu.slot(&*host, menu_slot(from)), Some(Some(stack)) if stack.item() == rod && stack.damage() as i32 == expected.rod.damage);
    if !source_ok {
        return false;
    }
    // The hotbar slot it lands in must hold what the snapshot says the
    // hand holds: the held rod, a listed stack, or nothing.
    let dest_ok = match menu.slot(&*host, menu_slot(to)) {
        Some(Some(stack)) => {
            if stack.item() == rod {
                snap.held.is_some_and(|held| held.damage == stack.damage() as i32)
            } else {
                snap.junk.iter().any(|j| {
                    j.slot == snap.selected
                        && item_id(host, stack.item()).as_ref() == Some(&j.item)
                })
            }
        }
        Some(None) => {
            snap.held.is_none() && !snap.junk.iter().any(|j| j.slot == snap.selected)
        }
        None => false,
    };
    if !dest_ok {
        return false;
    }
    menu.click(
        &mut *host,
        &[MenuClick::Swap {
            slot: menu_slot(from),
            with: SwapWith::Hotbar(to as u8),
        }],
    )
}

/// Throw away the junk in inventory slot `slot`, after checking it
/// still holds the listed item. False when anything does not match,
/// or the click was refused.
fn do_drop(host: &mut dyn ClientHost, snap: &engine::Snapshot, slot: usize) -> bool {
    let Some(player) = host.local_player() else {
        return false;
    };
    let Some(menu) = player.container_menu(&*host) else {
        return false;
    };
    if menu.slot_count(&*host) != Some(INVENTORY_MENU_SLOTS) {
        return false;
    }
    let Some(expected) = snap.junk.iter().find(|j| j.slot == slot) else {
        return false;
    };
    let matches = match menu.slot(&*host, menu_slot(slot)) {
        Some(Some(stack)) => item_id(host, stack.item()).as_ref() == Some(&expected.item),
        _ => false,
    };
    if !matches {
        return false;
    }
    menu.click(
        &mut *host,
        &[MenuClick::Throw {
            slot: menu_slot(slot),
            whole_stack: true,
        }],
    )
}

#[mod_api::horizon_mod]
impl Hooks for Mod {
    fn register(&self, r: &mut dyn Registrar, _cx: &RegistrationContext) {
        let _ = r.register_setting(SettingSpec::toggle("enabled", "Auto Fishing", true));
        // The mode is a choice, not a slider: the page's button cycles
        // the named modes, so the player can see what each one does.
        // Default index 1 = mode 2, the previous default.
        let _ = r.register_setting(SettingSpec::choice(
            "mode",
            "Mode",
            [
                "Current rod",
                "Current rod + break protection",
                "All rods",
                "All rods + break protection",
            ],
            1,
        ));
        let _ = r.register_setting(SettingSpec::range(
            "protect_at",
            "Protect at",
            1.0,
            20.0,
            1.0,
            5.0,
        ));
        let _ = r.register_setting(SettingSpec::range(
            "recast_delay",
            "Recast delay",
            0.0,
            100.0,
            1.0,
            17.0,
        ));
        let _ = r.register_setting(SettingSpec::range(
            "catch_delay",
            "Catch delay",
            0.0,
            20.0,
            1.0,
            0.0,
        ));
        let _ = r.register_setting(SettingSpec::toggle(
            "jitter_enabled",
            "Random delays",
            true,
        ));
        let _ = r.register_setting(SettingSpec::range(
            "jitter_max",
            "Jitter range",
            0.0,
            20.0,
            1.0,
            5.0,
        ));
        let _ = r.register_setting(SettingSpec::toggle("junk_enabled", "Junk filter", false));
    }

    fn client_tick_pre(&self, host: &mut dyn ClientHost, _event: &mut ClientTickPreEvent) {
        if !host.on(engine::ID) {
            // Off: forget the session, so turning it back on does not
            // mistake the next cast by hand for a reel.
            engine::reset();
            return;
        }
        load(host);
        let snap = snapshot(host);
        let out = engine::tick(snap.clone());
        let mut failed = false;
        if let Some(slot) = out.select_slot {
            if host.select_hotbar_slot(slot as u8).is_err() {
                failed = true;
            }
        }
        if !failed {
            if let Some((from, to)) = out.swap {
                if !do_swap(host, &snap, from, to) {
                    failed = true;
                }
            }
        }
        if !failed && out.use_rod {
            if send_use_item(host, out.sequence, &self.send_failed) {
                if out.cast_out {
                    // The recast went out: record where from and in
                    // which direction, so a later splash can be
                    // checked against the cast.
                    engine::note_cast(camera_pos(host), look_dir(host));
                }
            } else {
                failed = true;
            }
        }
        if failed && out.use_rod {
            // The cast never went out: try again after the recast delay
            // instead of waiting for a bite that cannot come.
            engine::cast_failed();
        }
        if let Some(slot) = out.drop_slot {
            do_drop(host, &snap, slot);
        }
        for line in out.lines {
            let _ = host.chat_line(Text::literal(line));
        }
        persist(host);
    }

    fn play_level_sound_at_position(
        &self,
        host: HostSide<'_>,
        event: &mut PlayLevelSoundAtPositionEvent,
    ) -> Flow {
        // A both-sides hook: this mod only runs on the client.
        let HostSide::Client(host) = host else {
            return Flow::Continue;
        };
        if !host.on(engine::ID) {
            return Flow::Continue;
        }
        let Some(splash) = ident("minecraft:entity.fishing_bobber.splash") else {
            return Flow::Continue;
        };
        if *event.sound != splash {
            return Flow::Continue;
        }
        // The engine decides whether this splash fits this session's
        // cast (in range, in the cast direction, at least 100 ticks
        // after it): a splash from another player's bobber is
        // ignored.
        engine::note_bite(event.position);
        Flow::Continue
    }

    fn right_click_item(&self, host: HostSide<'_>, _event: &mut RightClickItemEvent) -> Flow {
        // A both-sides hook: this mod only runs on the client.
        let HostSide::Client(host) = host else {
            return Flow::Continue;
        };
        if !host.on(engine::ID) {
            return Flow::Continue;
        }
        load(host);
        if !held_is_rod(host) {
            return Flow::Continue;
        }
        // The player's own use: a reel when the line is out, a cast
        // otherwise. The mod's own uses go out as packets, never
        // through here, so they are never mistaken for the player's.
        // The cast's origin and look direction go with it, for the
        // bite check.
        engine::note_player_use(engine::line_out(), camera_pos(host), look_dir(host));
        Flow::Continue
    }

    fn register_client_commands(
        &self,
        _host: &mut dyn ClientHost,
        event: &mut RegisterClientCommandsEvent<'_>,
    ) {
        // Declare every usage to the chat tree (Starling 0.9.0's
        // `register_client_commands`, NeoForge's
        // `RegisterClientCommandsEvent`): without this the client has
        // no `/autofish` in its tree, so typing it shows red as an
        // unknown command with no completion, even though the
        // `command` hook below still answers it after send. The
        // usages come from the engine, so the tree cannot drift from
        // what the mod actually runs. Asked at connect, never per
        // frame; level 0 throughout, the only level a client command
        // may have.
        event
            .commands
            .extend(engine::COMMAND_USAGES.iter().map(|u| Command::new(u)));
    }

    fn command(&self, host: &mut dyn ClientHost, event: &mut CommandEvent<'_>) -> Flow {
        if !host.on(engine::ID) {
            return Flow::Continue;
        }
        load(host);
        let Some(out) = engine::command(&event.command) else {
            return Flow::Continue;
        };
        for line in out.lines {
            let _ = host.chat_line(Text::literal(line));
        }
        persist(host);
        Flow::Cancel
    }

    fn setting_changed(&self, host: &mut dyn ClientHost, _event: &mut SettingChangedEvent<'_>) {
        if !host.on(engine::ID) {
            return;
        }
        load(host);
    }

    fn client_player_logging_in(
        &self,
        host: &mut dyn ClientHost,
        _event: &mut ClientPlayerLoggingInEvent,
    ) {
        if let Ok(mut reported) = self.send_failed.lock() {
            *reported = false;
        }
        if !host.on(engine::ID) {
            return;
        }
        load(host);
        engine::reset();
    }
}

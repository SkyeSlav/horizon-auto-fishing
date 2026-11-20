// Each client tick on a server: the bobber's nibble flag is read here,
// and a bite reels the rod in (or a cast goes out after the delay).
//
// The bite comes from the game's own fishing state: the client's bobber
// is `Game.hook`, and the server syncs its biting flag into `nibble`
// (`remote_hook_data`, DATA_BITING). `hook_status()` is the read path —
// the `nibble` field itself is private to the fishing module. No bobber
// (`hook_status()` is None) while the line is believed out means the
// server killed the bobber, so the engine recasts after the delay.
//
// Reeling in and casting both go out as the rod's own UseItem packet —
// the same packet the game's remote use path sends (`remote_use_item`),
// with the game's own yaw and pitch and a sequence counted up by the
// engine. The packet skips the client's local prediction, so the bobber
// coming back and the rod's damage wait for the server's reply.
//
// A rod swap from the main inventory goes out as one container click
// (Swap: the number-key swap of the inventory screen), and a junk
// throw as one container click (Throw: Q over the slot). Both run
// through the game's own predicted menu engine, so a click the engine
// refuses is never sent. They only run while the player is actually
// playing with the inventory menu open (or not open at all) — never
// while another container is open, and never while a screen is up.
fn autofish_tick(game: &mut crate::game::Game<'_>) {
    use autofish_engine as af;
    let Some(r) = game.remote.as_ref() else {
        return;
    };
    if !game.native_on(af::ID) || !r.joined || r.reconfiguring.is_some() {
        return;
    }
    {
        let game = &*game;
        af::load(&|k| game.native_setting(af::ID, k));
    }
    // The bobber's nibble flag, or None when no bobber is out.
    let nibble = game
        .hook_status()
        .and_then(|v| v.get("nibble")?.as_i64())
        .map(|n| n as i32);
    // A fishing rod in a stack, with its damage against its maximum
    // durability.
    let rod_of = |stack: &horizon_client::items::ItemStack| -> Option<af::Rod> {
        if &*stack.item != "minecraft:fishing_rod" {
            return None;
        }
        let max_damage = game
            .items()
            .get(&stack.item)
            .map_or(0, |i| i.max_damage as i32);
        Some(af::Rod {
            damage: stack.damage as i32,
            max_damage,
        })
    };
    let held = game.held().and_then(rod_of);
    // The rods in the hotbar and in the rest of the inventory, and every
    // stack's item id for the junk filter. `inventory` is 36 slots:
    // 0..9 the hotbar, 9..36 the main inventory.
    let mut hotbar: Vec<af::RodSlot> = Vec::new();
    let mut inventory_rods: Vec<af::RodSlot> = Vec::new();
    let mut junk: Vec<af::JunkSlot> = Vec::new();
    for (slot, stack) in game.inventory.iter().enumerate() {
        let Some(stack) = stack.as_ref() else {
            continue;
        };
        if let Some(rod) = rod_of(stack) {
            let rod_slot = af::RodSlot { slot, rod };
            if slot < 9 {
                hotbar.push(rod_slot);
            } else {
                inventory_rods.push(rod_slot);
            }
        }
        junk.push(af::JunkSlot {
            slot,
            item: stack.item.to_string(),
        });
    }
    let snap = af::Snapshot {
        nibble,
        held,
        hotbar,
        inventory_rods,
        junk,
        selected: game.selected,
    };
    let out = af::tick(snap);
    // Container clicks only while playing with the inventory menu: a
    // chest or another container open would send the click to the wrong
    // slots, and a screen up means the player is moving items by hand.
    let can_menu = game.screen == crate::game::Screen::Playing
        && game.remote_menu().1 == "inventory";
    let mut use_rod = out.use_rod;
    // Swap the fresh inventory rod into the hand's hotbar slot first.
    // If the swap cannot run (or the engine refuses it), the use does
    // not go out either: casting with the worn rod would break the
    // protection the swap was for. The engine retries after the delay.
    if let Some((from, to)) = out.swap {
        let swapped = can_menu
            && game.remote_menu_ops(&[(
                Some(from),
                to as i32,
                horizon_client::menu::Input::Swap,
            )]);
        if swapped {
            game.selected = to;
        } else {
            use_rod = false;
        }
    }
    // Select the fresh rod first: the game sends the slot change itself
    // on the next tick, before the use packet's tick runs.
    if let Some(slot) = out.select_slot {
        game.selected = slot;
    }
    if use_rod {
        game.swing_arm();
        game.send(horizon_protocol::packets::play::Serverbound::UseItem(
            horizon_protocol::packets::play::UseItem {
                hand: 0,
                sequence: out.sequence,
                yaw: game.yaw,
                pitch: game.pitch,
            },
        ));
    }
    // The junk filter: one throw per tick, never in the same tick as a
    // rod swap (one container action per tick).
    if out.swap.is_none()
        && let Some(slot) = out.drop_slot
        && can_menu
    {
        game.remote_menu_ops(&[(
            Some(slot),
            0,
            horizon_client::menu::Input::Throw,
        )]);
    }
    for line in out.lines {
        game.native_chat(&line);
    }
    for (k, v) in af::take_store() {
        game.native_set_setting(af::ID, &k, &v);
    }
}

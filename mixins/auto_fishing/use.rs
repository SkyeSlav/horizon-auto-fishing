// The player's own right-click with the rod, in the game's
// multiplayer use path: a cast when no bobber is out, a reel-in when
// one is. Noting it here is what lets the engine tell the player's
// own reel-in (which ends the session) from the server killing the
// bobber (which recasts). The mod's own uses never come through here
// — they go out as raw UseItem packets — so they are never mistaken
// for the player's.
fn autofish_note_use(game: &mut crate::game::Game<'_>) {
    use autofish_engine as af;
    if !game.native_on(af::ID) {
        return;
    }
    let Some(r) = game.remote.as_ref() else {
        return;
    };
    if !r.joined || r.reconfiguring.is_some() {
        return;
    }
    // The main hand's rod only: that is the rod the mod fishes with.
    if !game
        .held()
        .is_some_and(|s| &*s.item == "minecraft:fishing_rod")
    {
        return;
    }
    let reeling = game.hook_status().is_some();
    af::note_player_use(reeling);
}

// `/autofish ...`: the mod's own commands, answered in the chat. The
// command arrives without its slash.
fn autofish_command(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    use autofish_engine as af;
    if *ran || !game.native_on(af::ID) {
        return;
    }
    {
        let game = &*game;
        af::load(&|k| game.native_setting(af::ID, k));
    }
    let Some(out) = af::command(command) else {
        return;
    };
    *ran = true;
    for line in out.lines {
        game.native_chat(&line);
    }
    for (k, v) in af::take_store() {
        game.native_set_setting(af::ID, &k, &v);
    }
}

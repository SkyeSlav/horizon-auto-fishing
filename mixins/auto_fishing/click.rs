// A click on the settings page: the button's setting moves to its next value.
fn autofish_click(game: &mut crate::game::Game<'_>, mod_id: &str, button: usize) {
    use autofish_engine as af;
    if mod_id != af::ID {
        return;
    }
    {
        let game = &*game;
        af::load(&|k| game.native_setting(af::ID, k));
    }
    af::settings_click(button);
    for (k, v) in af::take_store() {
        game.native_set_setting(af::ID, &k, &v);
    }
}

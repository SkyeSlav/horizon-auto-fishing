// The mod's settings page: the toggle, the mode, the two delays.
fn autofish_page(game: &mut crate::game::Game<'_>, mod_id: &str, buttons: &mut Vec<String>) {
    use autofish_engine as af;
    if mod_id != af::ID {
        return;
    }
    {
        let game = &*game;
        af::load(&|k| game.native_setting(af::ID, k));
    }
    buttons.extend(af::settings_buttons());
}

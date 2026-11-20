// The mod's row in Options > Mods.
fn autofish_listed(mods: &mut Vec<crate::game::native_hooks::NativeMod>) {
    use autofish_engine as af;
    mods.push(crate::game::native_hooks::NativeMod {
        id: af::ID.into(),
        title: format!("{} {}", af::NAME, af::VERSION),
        description: af::DESCRIPTION.into(),
        keys: Vec::new(),
    });
}

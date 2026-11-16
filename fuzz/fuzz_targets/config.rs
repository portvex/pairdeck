//! config.toml / state.toml parsing: bad files must produce errors, not panics.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    if let Ok(cfg) = toml::from_str::<pairdeck::config::Config>(s) {
        let (_map, _errors) = pairdeck::keymap::KeyMap::new(&cfg.keys);
        let _ = pairdeck::theme::Theme::by_name(&cfg.theme);
    }
    let _ = toml::from_str::<pairdeck::config::State>(s);
});

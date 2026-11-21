//! `key.fullbright` (#18700): Horizon's fullbright switch has a rebindable key of its own, and
//! the key does nothing where another server's world holds the extra off.
use crate::game::{Game, Screen, game_tests::tests::server_game};

/// A game at play whose fullbright key is F7, its settings saved into a scratch folder (the
/// key's press saves them, as the Video settings' switch does).
fn with_playing_game(test: impl FnOnce(&mut Game<'_>, &std::path::Path)) {
    let scratch = std::env::temp_dir().join(format!(
        "horizon-fullbright-key-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    if horizon_client::logging::player_dir_override().is_none() {
        horizon_client::logging::set_player_dir(scratch.clone());
    }
    let mut resources = None;
    let mut game = server_game(&mut resources);
    game.screen = Screen::Playing;
    game.settings.keys.set("key.fullbright", "key.keyboard.f7");
    test(&mut game, &horizon_client::logging::player_dir());
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
#[ignore = "daily"]
fn the_fullbright_key_toggles_the_extra_and_does_nothing_where_the_server_holds_it_off() {
    with_playing_game(|game, dir| {
        assert!(!game.fullbright(), "fullbright starts off");
        assert!(game.fullbright_key_press("key.keyboard.f7"));
        assert!(game.settings.extras.fullbright);
        assert!(game.fullbright());
        let extras = std::fs::read_to_string(dir.join(horizon_client::extras::FILE)).unwrap();
        assert!(extras.contains("fullbright:true"), "{extras}");
        // The same key puts it back.
        assert!(game.fullbright_key_press("key.keyboard.f7"));
        assert!(!game.settings.extras.fullbright);
        assert!(!game.fullbright());
        // A key the player did not bind to it is not this one's.
        assert!(!game.fullbright_key_press("key.keyboard.f8"));
        assert!(!game.settings.extras.fullbright);

        // On another's server fullbright is held off (#3823, #14335): the key takes nothing
        // from the switch, and is still the fullbright key's press rather than the game's.
        game.settings.fullbright_held = true;
        assert!(!game.fullbright(), "the player sees it off");
        assert!(game.fullbright_key_press("key.keyboard.f7"));
        assert!(!game.settings.extras.fullbright, "the switch stayed off");
        assert!(!game.set_fullbright(true), "a mod's switch is refused too");
        assert!(!game.fullbright());
    });
}
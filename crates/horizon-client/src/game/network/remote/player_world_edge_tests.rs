//! `Player.tick`'s world edge at the end of the local player's own tick (#17227).
use super::*;

/// Run one player tick from `at` and read where it left the player.
fn tick_from(game: &mut Game<'_>, at: Vec3) -> Vec3 {
    game.player.as_mut().unwrap().position = at;
    tick(game);
    game.player.as_ref().unwrap().position
}

#[test]
#[ignore = "daily"]
fn a_spectators_tick_holds_x_and_z_at_the_world_edge_and_leaves_y_alone() {
    with_client_game(|game| {
        let server = connect(game);
        // A spectator: Player.tick derives noPhysics from the game mode, so no
        // collision pulls the move back and only the tick's own clamp does.
        let mut player = PlayerMotion::new(game.motion, Vec3::new(8.0, 64.0, 8.0)).unwrap();
        player.spectator = true;
        player.flying = true;
        player.ground.on_ground = true;
        game.player = Some(player);
        // Vanilla's 2.9999999E7, written out: a spectator past the edge comes
        // back to it instead of flying on to the far lands.
        assert_eq!(
            tick_from(game, Vec3::new(30_000_000.0, 64.0, 8.0)),
            Vec3::new(29_999_999.0, 64.0, 8.0),
            "the positive X edge"
        );
        assert_eq!(
            tick_from(game, Vec3::new(8.0, 64.0, -30_000_000.0)),
            Vec3::new(8.0, 64.0, -29_999_999.0),
            "the negative Z edge"
        );
        // Y has no clamp: the world is not bounded above.
        assert_eq!(
            tick_from(game, Vec3::new(8.0, 30_000_000.0, 8.0)),
            Vec3::new(8.0, 30_000_000.0, 8.0),
            "Y is left alone"
        );
        // A player inside the world keeps the position its move gave it.
        assert_eq!(
            tick_from(game, Vec3::new(8.0, 64.0, 8.0)),
            Vec3::new(8.0, 64.0, 8.0),
            "a position inside the world"
        );
        server.shutdown();
    });
}

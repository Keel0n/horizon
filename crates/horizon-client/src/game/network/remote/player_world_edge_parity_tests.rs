//! The Rust side of tools/test_player_tick_world_edge_parity.py: with
//! `HORIZON_PLAYER_TICK_TRACE` pointing at a file, one remote player tick from each
//! start position `VanillaPlayerTickOracle` runs leaves the player where vanilla's
//! `Player.tick` leaves it, printed in that oracle's format.
use super::*;

/// `VanillaPlayerTickOracle`'s cases, in its order: inside the edge, past it on each
/// axis, on it, at the clamp, above it, and past it on both negative axes.
const CASES: [(&str, f64, f64, f64); 9] = [
    ("inside_x", 29_999_980.0, 64.0, 8.0),
    ("past_x", 30_000_020.0, 64.0, 8.0),
    ("past_negative_x", -30_000_020.0, 64.0, -8.0),
    ("past_z", 8.0, 64.0, 30_000_020.0),
    ("past_negative_z", 8.0, 64.0, -30_000_020.0),
    ("on_the_edge", 30_000_000.0, 64.0, 8.0),
    ("at_the_clamp", 29_999_999.0, 64.0, 8.0),
    ("high_y", 8.0, 30_000_000.0, 8.0),
    ("past_negative_x_and_z", -30_000_020.0, 64.0, -30_000_020.0),
];

/// The fresh spectator `player_world_edge_tests` ticks: flying, still, its position put
/// there by the fixture rather than by a move, so only the tick's clamp acts on it.
fn spectator_at(game: &mut Game<'_>, at: Vec3) {
    let mut player = PlayerMotion::new(game.motion, at).unwrap();
    player.spectator = true;
    player.flying = true;
    player.ground.on_ground = true;
    player.velocity = Vec3::ZERO;
    game.player = Some(player);
}

#[test]
#[ignore = "parity harness: tools/test_player_tick_world_edge_parity.py runs it"]
fn a_tick_from_the_world_edge_lands_like_vanilla() {
    let Some(trace) = std::env::var_os("HORIZON_PLAYER_TICK_TRACE") else {
        return;
    };
    with_client_game(|game| {
        let server = connect(game);
        let mut rows = Vec::new();
        for &(name, x, y, z) in &CASES {
            let at = Vec3::new(x, y, z);
            // The column the tick starts in, lit as a server's arrival lights it: the
            // remote tick stops at the gate without it and never reaches `Player.tick`.
            let (cx, cz) = ((at.x.floor() as i32) >> 4, (at.z.floor() as i32) >> 4);
            game.streaming
                .insert_remote(&mut game.world, horizon_client::chunk::Chunk::empty(cx, cz));
            spectator_at(game, at);
            game.tick_remote_player_body(true, false).unwrap();
            let end = game.player.as_ref().unwrap().position;
            rows.push(format!(
                "{{\"case\":\"{name}\",\"x\":\"{:016x}\",\"y\":\"{:016x}\",\"z\":\"{:016x}\"}}",
                end.x.to_bits(),
                end.y.to_bits(),
                end.z.to_bits()
            ));
        }
        server.shutdown();
        std::fs::write(&trace, format!("[{}]\n", rows.join(","))).expect("the trace is written");
    });
}

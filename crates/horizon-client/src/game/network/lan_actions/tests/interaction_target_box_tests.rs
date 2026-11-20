#![cfg(test)]
use super::*;
use crate::game::{Audio, Options, Progress, Settings, load_resources};
use horizon_client::{
    chunk::Chunk,
    lan::{Joined, NetConfig, Out, PlayerSender, guest::Guest, hub::HubConfig},
};
use horizon_core::position::ChunkPos;
use horizon_protocol::packets::configuration::ClientInformation;
use std::sync::{Arc, atomic::AtomicI32, mpsc};

/// A server's game on the prepared resources with the chunk at the origin
/// loaded, or None without them (CI carries no vanilla data).
fn server_game<'a>(
    resources: &'a mut Option<Arc<crate::game::Resources>>,
) -> Option<Box<Game<'a>>> {
    let root = std::env::var_os("HORIZON_TEST_RESOURCES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.local/horizon-client")
                .join(horizon_core::VANILLA_VERSION)
        });
    if !root.join("movement-properties.nbt").exists() {
        return None;
    }
    let settings = Settings::default();
    let loaded = load_resources(
        horizon_assets::resource_source::ResourceRoot::directory(root.clone()),
        Vec::new(),
        None,
        &root,
        &settings,
        std::time::Instant::now(),
        false,
        crate::game::execution_role::ExecutionRole::IntegratedServer,
        &Progress::new(),
        &mut |_, _| unreachable!(),
    )
    .ok()?;
    let mut options = Options::parse(std::iter::empty()).ok()??;
    options.resources = root.clone();
    options.execution_role = crate::game::execution_role::ExecutionRole::IntegratedServer;
    let res = resources.insert(loaded.resources);
    let mut game = Box::new(
        Game::new(
            res,
            loaded.pack,
            loaded.gui?,
            options,
            settings,
            Audio::silent(&horizon_assets::resource_source::ResourceSource::directory(
                root.clone(),
            )),
            None,
        )
        .ok()?,
    );
    game.reset_entity_residency();
    game.native_lifecycle
        .begin_open(mod_api::Authority::IntegratedServer);
    game.creative = false;
    game.options.dedicated = Some(Vec::new());
    Some(game)
}

/// The level's own `minecraft:interaction` is a guest's click target: the box
/// `ServerGamePacketListenerImpl.handleInteract` measures the 3-block range
/// from is `Interaction.makeBoundingBox` of its saved width and height, so a
/// right click reaches `Player.interactOn` instead of being refused as an
/// invalid target. #19010
#[test]
fn a_guests_click_reaches_the_levels_own_interaction() {
    let mut resources = None;
    let Some(mut game) = server_game(&mut resources) else {
        return;
    };
    game.build_registries();
    game.set_allow_commands(true);
    let stone = game
        .pack
        .assets
        .registry
        .default_state("minecraft:stone")
        .unwrap()
        .0;
    let mut chunk = Chunk::empty(0, 0);
    for x in 0..16 {
        for z in 0..16 {
            chunk.set(x, 99, z, stone);
        }
    }
    game.world.chunks.insert((0, 0), Arc::new(chunk));
    game.block_ticks
        .add_empty_chunk(ChunkPos::new(0, 0))
        .unwrap();
    assert!(game.run_command("forceload add 0 0"));
    assert!(game.run_command("tick freeze"));
    // A 2 × 3 interaction: `Interaction.makeBoundingBox` is `x ± width / 2`,
    // `y .. y + height`, `z ± width / 2` from its position, so its box is
    // [-0.5, 100, -0.5] .. [1.5, 103, 1.5].
    assert!(
        game.run_command(
            "summon minecraft:interaction 0.5 100 0.5 {width:2f,height:3f,response:1b}"
        )
    );
    let interaction = game
        .others
        .iter()
        .find(|o| o.kind == "minecraft:interaction")
        .expect("the summoned interaction")
        .id as i32;
    game.lan_host(
        super::super::lan::LanSettings {
            port: 0,
            guest_commands: true,
            force_game_mode: false,
        },
        NetConfig {
            listen: false,
            announce: false,
            ..NetConfig::loopback(0, "interaction-box-unit".into(), 2)
        },
        HubConfig {
            default_game_mode: 0,
            guest_commands: true,
            force_game_mode: false,
            hardcore: false,
            view_distance: 2,
            simulation_distance: 2,
            dimension: "minecraft:overworld".into(),
            allow_flight: false,
            seed: 0,
            max_players: 2,
            integrated: false,
            levels: Default::default(),
            level_seeds: Default::default(),
        },
    )
    .unwrap();
    let (tx, _rx) = mpsc::channel::<Out>();
    let me = game.take_entity_id() as i32;
    // Three blocks off the box's near face, inside the interaction range.
    let mut guest = Guest::new(
        Joined {
            conn: 1,
            uuid: 1,
            name: "Clicker".into(),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "unit".into(),
            sender: PlayerSender::new(tx, Arc::default()),
            latency: Arc::new(AtomicI32::new(0)),
            address: "127.0.0.1:1".parse().unwrap(),
            expected: false,
        },
        me,
        [0.5, 100.0, 4.5],
        2,
    );
    guest.placed = true;
    guest.loaded = true;
    guest.load_timeout = 0;
    guest.dimension = "minecraft:overworld".into();
    game.lan.as_mut().unwrap().hub.guests.push(guest);
    game.sync_tick_chunks();

    let target = game.guest_target_box(0, interaction).expect("the box");
    let (lo, hi) = (
        target.min_position().components(),
        target.max_position().components(),
    );
    assert_eq!(lo, [-0.5, 100.0, -0.5], "makeBoundingBox min");
    assert_eq!(hi, [1.5, 103.0, 1.5], "makeBoundingBox max");

    // The packet's location is measured from the entity's position; a point on
    // the box's near face, so the click is on the interaction.
    let used = game.guest_interact(1, 0, interaction, Hand::Main, [0.25, 1.5, 1.0]);
    assert!(
        used.is_ok(),
        "the click must not be refused before interactOn: {used:?}"
    );
}

//! The `may_place_on` hook: declarations the mod API generator reads (see `hooks/mod.rs`).
// catalog-order: 0079

pub trait Hooks {
    /// `VegetationBlock.mayPlaceOn` for the plant `state` on `ground` at `ground_pos`, before
    /// the plant answers it: the blocks a mod plant (one whose template is a vanilla plant,
    /// such as a crop) can stand on. Its template's `canSurvive` asks it, so the plant is
    /// refused there and pops off when its ground changes to one it can't stand on — in the
    /// live game and while a feature places it in world generation alike. `ground` is the
    /// block the plant stands on, so a rule that only picks its soil belongs here. Answer
    /// only for your own blocks. Server-side, on the worldgen worker threads and on the
    /// server thread, with the server's worldgen `context`: no `Host`, no writes. A panicking
    /// part is contained and the plant's own rule kept.
    #[hook(
        side = "server",
        contract = "replace",
        out = "may",
        context = "worldgen",
        scope = "block",
        key = "state",
        target = "mod_api::world::survival::native_may_place_on",
        neoforge = "IBlockExtension.canSustainPlant"
    )]
    fn may_place_on();
}

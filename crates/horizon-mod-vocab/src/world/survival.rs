//! Blocks' survival as they are placed (vanilla's `BlockState.canSurvive`) and as a plant
//! picks the ground it stands on (`VegetationBlock.mayPlaceOn`), which the parts of the
//! state's block may answer in `can_survive` and `may_place_on`.
//!
//! Both questions are asked on the worldgen worker threads while a feature places a block,
//! and on the server thread for every hand placement and shape update, so a feature and a
//! player get one answer. There is no `Host` and nothing to write: what a part may read is
//! the event's own arguments (the state, the position, and for a plant its ground).
use crate::{BlockPos, BlockState};

/// `BlockState.canSurvive` is about to be asked for `state` at `pos`: the parts of `state`'s
/// block may answer `survives`, which replaces the block's own rule.
#[inline(never)]
pub fn native_can_survive(
    context: &crate::OwningWorldgen<'_>,
    pos: BlockPos,
    state: BlockState,
    survives: &mut Option<bool>,
) {
    crate::dispatch_can_survive(context, pos, state, survives);
}

/// `VegetationBlock.mayPlaceOn` is about to be asked for the plant `state` on `ground` at
/// `ground_pos`: the parts of `state`'s block may answer `may`, which replaces the plant's own
/// rule.
#[inline(never)]
pub fn native_may_place_on(
    context: &crate::OwningWorldgen<'_>,
    state: BlockState,
    ground: BlockState,
    ground_pos: BlockPos,
    may: &mut Option<bool>,
) {
    crate::dispatch_may_place_on(context, state, ground, ground_pos, may);
}

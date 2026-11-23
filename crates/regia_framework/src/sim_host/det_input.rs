// SPDX-License-Identifier: MPL-2.0
//! [`DetInputHost`]: a [`DetInputSim`] on the lockstep sim host (KI-GP-005, ADR-10062).
//!
//! A blanket `impl<T: DetInputSim<N>> LockstepSim for T` cannot compile: `N` is not a parameter of
//! [`LockstepSim`], and a `DetInputSim` has neither a frame type nor a tick counter for
//! [`FixedStepSim`]. The adapter supplies both: it owns the boundary tick (the number of ticks
//! stepped), steps through [`advance`] (the one step path a replay also uses) and writes frames
//! through the frame type's [`DetInputFrame`] impl.
//!
//! Tick mapping: host tick `t` is the boundary after the sim's tick `t - 1`, so the frame a
//! [`LockstepFeed`](super::LockstepFeed) queues for host tick `t` is a [`TickInputs`] whose
//! [`TickInputs::tick`] is `t - 1` (debug builds assert it).
//!
//! Snapshots: the adapter's [`DetState`] body is the boundary tick, then the sim's own body. Its tag
//! is the sim's tag with the high bit of the last byte flipped ([`adapter_tag`]), so a bare sim
//! snapshot and an adapter snapshot never load as each other (`bad_tag`).

use regia_detsim::hash::{Mix, SystemFold, Systems};
use regia_detsim::replay::{DetInputSim, TickHashes, TickInputs, advance};
use regia_detsim::state::{DetReader, DetState, DetStateError, DetWriter};

use super::{FixedStepSim, LockstepSim};
use crate::sim_bridge::BridgeInput;

/// What a [`DetInputHost`] publishes each tick: the frame type writes itself from the sim.
pub trait DetInputFrame<S>: Default + Send + Sync + 'static {
    /// Overwrite every field from `sim` at boundary tick `tick` (the host recycles frames).
    fn write(&mut self, sim: &S, tick: u64);
}

/// Publishes nothing (a headless lockstep host that only steps, hashes and snapshots).
impl<S> DetInputFrame<S> for () {
    fn write(&mut self, _sim: &S, _tick: u64) {}
}

/// The snapshot tag of a [`DetInputHost`] over a sim tagged `sim_tag`: the high bit of the last
/// byte flipped.
pub const fn adapter_tag(sim_tag: [u8; 8]) -> [u8; 8] {
    let mut tag = sim_tag;
    tag[7] ^= 0x80;
    tag
}

/// A [`DetInputSim`] as a [`LockstepSim`]: start it with
/// [`ThreadedSimHost::start_lockstep`](super::ThreadedSimHost::start_lockstep) (or
/// `start_lockstep_with_desync`) and push one [`TickInputs`] per tick. See the module docs for the
/// tick mapping and the snapshot layout.
#[derive(Debug, Clone)]
pub struct DetInputHost<S, F, const N: usize> {
    sim: S,
    tick: u64,
    last: Option<TickHashes<N>>,
    _frame: std::marker::PhantomData<fn() -> F>,
}

impl<S, F, const N: usize> DetInputHost<S, F, N> {
    /// Wrap `sim`, which sits at boundary tick `tick` (0 for a fresh match).
    pub fn new(sim: S, tick: u64) -> Self {
        Self {
            sim,
            tick,
            last: None,
            _frame: std::marker::PhantomData,
        }
    }

    /// The wrapped sim.
    pub fn sim(&self) -> &S {
        &self.sim
    }

    /// The hashes [`advance`] returned for the last step (`None` before the first step and after a
    /// load).
    pub fn last_hashes(&self) -> Option<TickHashes<N>> {
        self.last
    }

    /// The wrapped sim and its boundary tick.
    pub fn into_parts(self) -> (S, u64) {
        (self.sim, self.tick)
    }
}

impl<S, F, const N: usize> FixedStepSim for DetInputHost<S, F, N>
where
    S: DetInputSim<N> + Send + 'static,
    F: DetInputFrame<S>,
{
    type Frame = F;
    type Event = ();

    /// Never called by a lockstep host (it steps [`LockstepSim::step_inputs`] only); merged
    /// bridge input has no meaning for a `DetInputSim`, so this does nothing.
    fn step(&mut self, _input: &BridgeInput) {}

    fn tick(&self) -> u64 {
        self.tick
    }

    fn write_frame(&self, out: &mut F) {
        out.write(&self.sim, self.tick);
    }
}

impl<S, F, const N: usize> LockstepSim for DetInputHost<S, F, N>
where
    S: DetInputSim<N> + Send + 'static,
    S::Input: Send + 'static,
    F: DetInputFrame<S>,
{
    type Inputs = TickInputs<S::Input>;

    fn step_inputs(&mut self, tick: u64, inputs: &Self::Inputs) {
        debug_assert_eq!(
            inputs.tick() + 1,
            tick,
            "host tick {tick} steps the sim's tick {}; the frame carries tick {}",
            tick - 1,
            inputs.tick()
        );
        self.last = Some(advance(&mut self.sim, inputs));
        self.tick = tick;
    }
}

impl<S, F, const N: usize> DetState for DetInputHost<S, F, N>
where
    S: DetInputSim<N>,
{
    const STATE_TAG: [u8; 8] = adapter_tag(S::STATE_TAG);
    const STATE_VERSION: u32 = S::STATE_VERSION;

    fn save(&self, w: &mut DetWriter) {
        w.u64(self.tick);
        self.sim.save(w);
    }

    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        let tick = r.u64()?;
        let sim = S::load(r)?;
        Ok(Self::new(sim, tick))
    }
}

impl<S, F, const N: usize> SystemFold<N> for DetInputHost<S, F, N>
where
    S: DetInputSim<N>,
{
    const SYSTEMS: Systems<N> = <S as SystemFold<N>>::SYSTEMS;

    fn fold_systems(&self, mix: &mut [Mix; N]) {
        self.sim.fold_systems(mix);
    }

    fn inject(&mut self, system: usize) -> bool {
        self.sim.inject(system)
    }
}

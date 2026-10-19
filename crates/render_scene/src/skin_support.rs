use bevy::prelude::Resource;
use render_frame::skinning::{SkinBatchLimits, SkinRefusal};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default)]
struct SupportState {
    world: frame::WorldStamp,
    device_generation: u64,
    limits: Option<SkinBatchLimits>,
    failure: Option<SkinRefusal>,
}

#[derive(Resource, Clone)]
pub struct SkinSupport {
    state: Arc<Mutex<SupportState>>,
    pub resident_budget: u64,
}

impl Default for SkinSupport {
    fn default() -> Self {
        let resident_budget = std::env::var("IW4L_SKIN_RESIDENT_BYTES")
            .map_or(Ok(1024 * 1024 * 1024), |text| text.parse::<u64>())
            .ok()
            .filter(|bytes| (1024 * 1024..=4 * 1024 * 1024 * 1024).contains(bytes))
            .unwrap_or(0);
        Self {
            state: Arc::new(Mutex::new(SupportState::default())),
            resident_budget,
        }
    }
}

impl SkinSupport {
    pub fn set_world(&self, world: frame::WorldStamp) {
        let mut state = self.state.lock().expect("skin support poisoned");
        if state.world != world {
            state.world = world;
            state.failure = None;
        }
    }

    pub fn report(
        &self,
        world: frame::WorldStamp,
        limits: SkinBatchLimits,
        device_generation: u64,
        failure: Option<SkinRefusal>,
    ) {
        let mut state = self.state.lock().expect("skin support poisoned");
        state.limits = Some(limits);
        state.device_generation = device_generation;
        if state.world == world && state.failure.is_none() {
            state.failure = failure;
        }
    }

    pub fn failure(&self, world: frame::WorldStamp) -> Option<SkinRefusal> {
        let state = self.state.lock().expect("skin support poisoned");
        if self.resident_budget == 0 {
            return Some(SkinRefusal::UnsupportedProduct);
        }
        (state.world == world).then_some(state.failure).flatten()
    }

    pub fn refuse(&self, world: frame::WorldStamp, failure: SkinRefusal) {
        let mut state = self.state.lock().expect("skin support poisoned");
        if state.world == world {
            state.failure = Some(failure);
        }
    }

    pub fn device_generation(&self) -> u64 {
        self.state
            .lock()
            .expect("skin support poisoned")
            .device_generation
    }

    pub fn limits(&self) -> Option<SkinBatchLimits> {
        self.state.lock().expect("skin support poisoned").limits
    }
}

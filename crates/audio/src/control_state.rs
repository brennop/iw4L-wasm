//! Fork: the state the audio control owner keeps between `control_pass` calls, split out of
//! runtime.rs (a child module, so it sees the runtime's private types). The owner is the
//! audio-control thread natively and the browser frame (web_output.rs) on wasm32.

use super::*;

/// Everything the control owner keeps between passes: the audio-control thread
/// natively, the browser frame (web_output.rs) on wasm32.
pub(crate) struct ControlState {
    pub(crate) shared: Arc<RenderShared>,
    pub(super) cue_rx: Receiver<CueRequest>,
    pub(super) sources: Arc<SourceInbox>,
    pub(super) listener: Arc<ListenerState>,
    pub(super) event_context: Arc<crate::event::EventContextState>,
    pub(super) next_id: Arc<AtomicU64>,
    pub(super) rejections: Arc<[AtomicU64; 6]>,
    pub(super) cue_budget: crate::pending::PendingBudget,
    pub(super) resolver: CueResolver,
    pub(super) events: crate::event::EventJournal,
    pub(super) pending_cues: VecDeque<CueWork>,
    pub(super) device_was_active: bool,
    pub(super) null_anchor: Instant,
    pub(super) null_frame: u64,
    pub(super) instances: Vec<LogicalInstance>,
    pub(super) next_voice: u64,
    pub(super) desired: SourceScene,
    pub(super) present_sources: HashSet<(SourceKey, u64)>,
    pub(super) source_cues: HashMap<(SourceKey, u64), Arc<crate::cue::CueState>>,
    pub(super) silence: [[f32; 2]; QUANTUM],
}

impl ControlState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        shared: Arc<RenderShared>,
        cue_rx: Receiver<CueRequest>,
        sources: Arc<SourceInbox>,
        listener: Arc<ListenerState>,
        event_context: Arc<crate::event::EventContextState>,
        next_id: Arc<AtomicU64>,
        rejections: Arc<[AtomicU64; 6]>,
        cue_budget: crate::pending::PendingBudget,
    ) -> Self {
        Self {
            shared,
            cue_rx,
            sources,
            listener,
            event_context,
            next_id,
            rejections,
            cue_budget,
            resolver: CueResolver::new(),
            events: crate::event::EventJournal::new(),
            pending_cues: VecDeque::with_capacity(LOGICAL_INSTANCES),
            device_was_active: false,
            null_anchor: Instant::now(),
            null_frame: 0,
            instances: Vec::with_capacity(LOGICAL_INSTANCES),
            next_voice: 1,
            desired: SourceScene {
                revision: 0,
                sources: Vec::new(),
                asserted: Vec::new(),
            },
            present_sources: HashSet::with_capacity(LOGICAL_INSTANCES),
            source_cues: HashMap::with_capacity(crate::sources::SOURCE_HISTORY),
            silence: [[0.0; 2]; QUANTUM],
        }
    }

    /// Logical instances and how many of them render now (browser stats).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn voices(&self) -> (usize, usize) {
        let started = self
            .instances
            .iter()
            .filter(|logical| logical.request.instance.status() == InstanceStatus::Started)
            .count();
        (self.instances.len(), started)
    }
}

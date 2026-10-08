# Client audio

Bevy supplies cue intentions, listener snapshots and desired source scenes.
Control selects cues, waits for media, admits layers and updates spatial gain.
CueFeedback observes decisions; persistent sources wait for media in control.

```text
presentation / immutable bank requests
                  ↓
       bounded cue intake queue
                  ↓
      AudioControl (audio-control thread)
        → AudioRender (render_core.rs) → CPAL device / null / offline
```

The internal rate is 48 kHz, the quantum 128 stereo frames and the physical
pool 128 slots. Control stores 2048 logical instances and processes at most
64 cue intakes and 64 pending cue steps per pass. Pools refuse work;
physical saturation keeps loops virtual and rejects new one-shots. Cancellation
and epoch invalidation bypass queues; 4096 versioned cue keys preserve Stop/Play.

Cue slots/selectors cap at 2048; media caps 4096 keys/256 jobs, prefetch submits 64/pass.
Entity occurrences and animation marker instances retain distinct identities;
control dedups 8192 identities within 100 ticks; local life invalidates marker cues.
Loaded keys use content/conversion identity; PCM pins 256 MiB; Symphonia/ADPCM scratch reserves 64 MiB.
Control checks all rules and physical reservations before stopping victims.
Weapon one-shots preserve their first 20 ms when replaced by another shot, so
batched rapid-fire events reach output before their tails are cut. Protected
attacks still count toward the physical pool; explicit Stop/epoch cancellation
bypasses this protection.
Cues retain media/deadlines; diagnostics separate pending source layers and voices.

Loops publish desired cue scenes keyed by scope/epoch/object/slot and version.
Sources use model/map/menu/local-life slots and versions, retain virtual cursors and stop
on desired absence. Control ranks eight map voices with a 0.002 gain floor.
Shellshock and heartbeat follow client/life state. Scenes cap at 2048 execution
keys and 4096 assertions/version records; exclusion preserves virtual cues.
Removed versions cannot resurrect; excess keys/refused updates are counted.

The callback reads prepared PCM/slots, resamples and sums fixed blocks. Control
frees retired payloads; media joins run on retirement threads. Stereo gain/pan is atomic;
listener snapshots stay in control; master volume is applied once after mixing.

`audio-device` owns streams/recovery and counts recoverable underruns. It
requests 48 kHz and a 256-frame callback where supported, watches callback
progress and immediately reopens established streams. Failed opens and rapidly
failing streams use bounded exponential backoff. Audio threads use the original
process CPU allowance rather than inheriting the frame owner's narrow mask.
Control uses null while output is unavailable. Device-backed one-shots wait for
output and preserve PCM during short recovery, with start deadlines and
cancellation preventing stale attacks; intentional silent/offline transport
continues to render normally. Virtualization hands the cursor back to
control. Source/device resampling uses linear interpolation. Match/channel ramps
and cue fades/releases use AudioFrame. Match gain preserves cues; mix tokens remain unfinished.
`OfflineRenderer` shares the kernel, with scheduled PCM, status and cursor.
Canonical action identities, cue groups, streaming, DSP buses/tails and
acoustic propagation remain unfinished, as do media budgets and time mappings.

T5 XWMA (BO1's mono 44.1 kHz and stereo 48 kHz profiles) decodes through the
native WMA2 decoder in `asset_audio` into cached s16 PCM.

`IW4L_AUDIO_DIAG=1` enables fire producer identities, cue decisions and request
latency, media preparation timings, slow control stages, thread affinity and
per-instance device/null frame counts. `IW4L_AUDIO_DIAG_PATH=/path/audio.log`
writes them to a separate buffered file. A bounded queue drops diagnostics
instead of waiting for file I/O; its dropped count is flushed at exit. Device
frames mean mixed into output callbacks, not confirmed hardware playback.
`dump` and `clip` include audio readiness, recent decisions and transport totals.
Weapon audio is published with the fire event and submitted in Predict after
owner events, ahead of pose preparation and FX. Cue arrivals wake control
immediately. Prediction occurrences age against the owner command clock;
authoritative and animation occurrences use the snapshot clock. Replay presents
recorded owner fire events; live owner fire remains predicted, with authoritative
duplicates suppressed. Hit sound and HUD rewards stay on the server path.
First-device timings measure mixing into the callback, before device latency.

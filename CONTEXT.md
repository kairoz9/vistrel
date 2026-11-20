# Vistrel

WebRTC Pixel Streaming for Minecraft with Multi-Loader and Multi-Version architecture.

## Language

**Game Instance**:
The running Minecraft game instance executing the mod in headless or host mode.
_Avoid_: Server, Game node, Backend, Host Instance, 宿主机

**Remote Player**:
The end-user playing Minecraft through a modern browser over WebRTC.
_Avoid_: User, Web client, Slave

**RAM Readback**:
Asynchronous host memory readback pipeline (e.g. OpenGL double Pixel Buffer Object ping-pong DMA transfer or Vulkan host-visible GpuBuffer copy) transferring rendered frame buffers from GPU to CPU system RAM without stalling the main render thread.
_Avoid_: Screenshot, FBO copy, GlReadPixels wait, PBO Readback (as generic term)

**Circuit Breaker**:
A latch on the VRAM Direct level holding a consecutive-failure streak and a trip flag that stays set: `Delivered` and `Dropped` reset the streak, `Failed` (a thrown exception folded into `Failed`) increments it, and the level trips at `FAILURE_THRESHOLD = 30`. Tripping is one-way — there is no half-open probe, only a pipeline rebuild's `reset()` — and it lands in a permanent demotion to RAM Readback. **Only VRAM Direct carries one**: RAM Readback is the last resort with nothing to demote to, so it has no breaker, only an exception guard that folds `Throwable` into `Failed` and reports the first one per pipeline lifetime.
_Avoid_: Health monitor, Error counter, Retry budget

**Heartbeat Timeout**:
The stall detector of VRAM Direct: the game side records the encoder actor's heartbeat when pushing a frame and re-checks on every capture tick (including all-busy drop ticks); a heartbeat frozen at the baseline for over 1 second means the encoder thread is wedged — stop pushing VRAM frames and auto-restart the streaming engine (self-heal).
_Avoid_: Fuse, Stall fuse, Watchdog, 熔丝

**Permanent Demotion**:
The one-way fallback after the Circuit Breaker trips: VRAM Direct is released and the session stays on RAM Readback; only a pipeline rebuild (size or codec change) resets the breaker. There is no automatic switch back within the rebuilt pipeline's lifetime.
_Avoid_: Sticky demotion, 粘性降级, Auto fallback retry

**Native Bridge**:
The unified JNI cross-language proxy interface in Java calling into the underlying Rust native dynamic library (`NativeBridge.java`).
_Avoid_: C wrapper, DLL runner

**Native Core**:
The high-performance Rust native engine (`rust-native/`) managing hardware video encoding, WebRTC peer streaming, and audio loopback.
_Avoid_: Native plugin, Rust backend

**Pointer Lock**:
The HTML5 `requestPointerLock()` browser API capturing raw physical mouse deltas (`movementX`/`movementY`) without visual cursor bounds.
_Avoid_: Mouse capture, Cursor lock

**In-Game Software Cursor**:
The GPU-rendered 2D cursor overlay rasterized into Minecraft's framebuffer at render tail when the mouse is ungrabbed, enabling full-time Pointer Lock on Remote Player and eliminating cursor grabbed state synchronization.
_Avoid_: Fake cursor, Browser cursor emulation, Software mouse

**Input Frame**:
The highly-compact binary event payload transmitted over WebRTC DataChannel for raw keyboard and mouse pass-through.
_Avoid_: Input packet, Event JSON

**Input Motion Coalescing**:
The in-queue saturated scalar accumulation of continuous raw relative mouse movements (`0x01`) into a single Input Frame at Native Core ingestion, preserving temporal isolation across non-motion events while eliminating high-frequency queue bloat and JNI overhead.
_Avoid_: Event throttling, Frame dropping, Client mouse debounce

**Lock-Free SPSC Input RingBuffer**:
The strictly single-producer single-consumer circular queue utilizing atomic cursor indices and 64-bit atomic packed event slots to transfer Input Frames between the Tokio WebRTC task and the Minecraft render thread without mutex locks.
_Avoid_: Concurrent queue, Thread-safe vector, Blocking input channel

**Input Adapter**:
The cross-version platform abstraction interface in Common isolating SDL3 (26.3+) and GLFW (1.20~1.21) raw window input event injection.
_Avoid_: Platform input handler, Window listener, Event hook

**Browser Action Pass-Through**:
The downstream signaling (game-to-browser, `open_url` / `clipboard_write` over the signaling WebSocket) intercepting the terminal side effects of in-game UI actions — opening external http/https links and system clipboard writes — while a streaming session is active, so they execute in the Remote Player's browser instead of the host machine; without an active session vanilla behavior stays untouched.
_Avoid_: Click event forwarding, Screen click relay, Clipboard sync

**Reverse Clipboard Pass-Through**:
The upstream browser-to-game paste path: while a streaming session is active, the web client intercepts paste on non-page targets and delivers the clipboard text over the signaling `text_input` channel, typed into the focused GUI text box of the Game Instance; in GUI state the triggering key press is not forwarded (it would paste the Game Instance's local clipboard on top). Character acceptance is vanilla's own filter, identical to local typing; without an active session native browser paste stays untouched, and a paste without textual content is silently ignored.
_Avoid_: Clipboard sync, Two-way clipboard, Paste injection

**Event Popup**:
The generic top-center popup presenting displayed downstream ClientEvents on the web client: a lightweight auto-fading toast variant for display-only events, and an interactive variant with a real `<a target="_blank">` open link or a selectable textarea plus copy button as the fallback when silent execution is blocked (popup blocker or insecure-context clipboard). Latest replaces previous; its clicks never forward to the game.
_Avoid_: Action card, Notification banner, Link popup

**Audio Loopback**:
The in-memory audio capture mechanism routing Minecraft's OpenAL Soft audio output directly into WebRTC Audio Track.
**Native half**: `streaming::audio_loopback::AudioLoopback` owns the Opus encoder and the sender of the current session's bounded 8-packet queue; each session attaches a fresh queue at establishment and spawns its own write loop — an `AudioWriter` (`streaming::writers::AudioWriter`), the same lifecycle as the video write loop: cancel + bounded join on close. The capture thread encodes synchronously and drops the frame when the queue is full — audio continuity yields to never blocking capture; the session's write loop drains it serially and writes to that session's track. The engine detaches the sender when the session closes; with no session, samples are dropped before encoding.
_Avoid_: Sound record, Mic streaming

**Playout Recording**:
The recording of the received media streams — rendered video and Audio Loopback audio — performed entirely within the Remote Player's browser, yielding a self-contained MP4 file downloaded to the player's machine. Contains no Game Instance UI overlays and is distinct from the game-side Capture Engine that produces the live stream.
_Avoid_: Client-side recording, Session recording, Screen recorder, Stream capture, Bitstream dump

**Replay Buffer**:
The always-on in-browser fMP4 fragment ring on a dedicated long-lived MediaRecorder, retaining the received media streams of roughly the last 30 seconds while a session is live: the first timeslice chunk (init segment) is kept for the whole session and later chunks are evicted on a 30-second wall-clock window. Discarded silently on session teardown. Zero coupling with Playout Recording — the two may run simultaneously on the same tracks.
_Avoid_: Sliding window recording, Rolling buffer, Replay cache, Instant replay

**Replay Clip**:
The MP4 clip assembled from the Replay Buffer on a single HUD-button trigger (回放): the retained init segment plus the windowed fragments downloaded as `vistrel-clip-*.mp4`, with no throttle between saves. The trigger is HUD-button-only, for the same reason Playout Recording has no hotkey.
_Avoid_: Highlight export, Clip dump, Back-capture

**Top-First Wire Format**:
The streaming wire convention that frame row 0 is the visually top row of the image, produced by the Capture-side Color Space Converter sampling with inverted V and consumed without any client-side orientation correction. It supersedes the legacy bottom-first (OpenGL-origin) order.
_Avoid_: Bottom-first wire, GL row order, Flipped stream

**Headless Cloud Mode**:
The off-screen cloud rendering execution mode running on Linux instances without physical displays (via Xvfb, KMS, or pure offscreen EGL/Vulkan).
_Avoid_: Dedicated server, Console mode

**Stonecutter Active Version**:
The specific Minecraft version branch actively unpacked and configured in the IDE workspace (e.g. 1.20, 1.21, or 26.3).
_Avoid_: Working version, Current branch

**VCS Baseline Version**:
The designated version branch checked into version control (permanently set to 26.3) to preserve clean and uniform Git history.
_Avoid_: Master version, Main branch

**Full Build And Collect**:
Root aggregate task (`buildAndCollect`) that builds the full release Jar matrix across all loaders and versions, runs the check gate, and collects them into `build/libs/`. Hierarchically shares its name with the per-version collect task (root = full matrix, node = single version), mirroring the `build`/`assemble` convention.
_Avoid_: Chiseled build (obsolete Stonecutter ≤0.7 API term), Build all, Full assemble

**In-Place Comment Reversal**:
Stonecutter's preprocessor mechanism reversing `//? if` conditional comments directly inside source files to toggle API views without multiple trees.
_Avoid_: Preprocessor, Macro rewrite

**Versioned Subproject**:
Fully-qualified subproject path (e.g. `:fabric:26.3`, `:forge:1.20`) isolating dependency resolution and client execution per version.
_Avoid_: Sub-module, Version folder

**Frame Pacing Throttler**:
The schedule-driven pacing gate in the capture pipeline that fires capture on a regular beat anchored to the Streaming Profile, realigning to the present after render stalls instead of accumulating catch-up debt.
_Avoid_: FPS limiter, Sleep timer

**Capture Engine**:
The graphics subsystem adapter responsible for reading rendered frames from OpenGL or Vulkan. It always offers a Frame Loan over RAM Readback; whether it can also offer a Frame Hand-off over VRAM Direct is settled once when it is constructed and never changes within its life.
_Avoid_: Screen recorder, Screenshot taker

**Capture State**:
The render-thread-confined state machine of the VideoCaptureScheduler (`IDLE ⇄ CAPTURING`, terminal `TORN_DOWN`): session broadcasts drive transitions inside the render-thread task, capture gates on `CAPTURING` only, and teardown is the one-way exit that drops late broadcasts. Deliberately distinct from SessionState.Phase — PAUSED and INACTIVE collapse to the same capture-side `IDLE` (gate shut, pipelines released), and the phase distinction that matters for audio lives in the AudioCaptureScheduler. Single-writer single-field: no volatile, no torn flag.
_Avoid_: isStreaming/contentPaused booleans, Streaming flags, Volatile gate flags

**Frame Loan**:
The settlement protocol of the RAM Readback path: the Capture Engine lends a mapped pixel buffer to the Encoder Channel for the span of one capture cycle and reclaims it through an explicit return call. The buffer is never surrendered, and the capture side is responsible for its lifetime throughout.
_Avoid_: Buffer borrow, Pool checkout, Mapped view

**Frame Hand-off**:
The settlement protocol of the VRAM Direct path: the Capture Engine surrenders a frame reference — a Texture Ring Buffer slot index plus its synchronization value — to the Encoder Channel and never reclaims it. Reuse of that slot is governed by the Texture Ring Buffer and by Native Core's own return of ownership, not by the capture side.
_Avoid_: Frame transfer, Ownership move, Fire-and-forget (which names the egress semantics, not the lifetime)

**Renderpearl Capture Engine**:
The unified capture implementation for Minecraft 26.3+ leveraging Mojang's Renderpearl graphics abstraction layer to capture frames across both OpenGL and Vulkan backends.
_Avoid_: Vulkan hook, RenderDoc capture

**Frame Source**:
The category of origin of a captured frame — a VRAM slot, a graphics texture, or system memory — which selects both the Native Bridge submission entry point and the encoder input path.
_Avoid_: Frame type, Payload kind, Input mode

**Canonical Frame Format**:
The single channel order every Capture Engine must produce for system-memory frames: RGBA, red in the lowest byte. Native Core holds exactly one assumption rather than one per adapter.
_Avoid_: Native order, Preferred format

**Host Frame Pool**:
The vendor-neutral pool of host frame buffers owned by the Capture Engine and merely lent to an Encoder Backend. The pool is a single object for the lifetime of an Encoder Channel; blocks carry their own memory domain (`Cuda` holds its `Arc<CudaContext>`, `Vulkan` holds its `VideoDevice`, `Heap` holds nothing), so a domain mismatch is structurally impossible rather than defended against. The backend allocates a batch on its own device/context at construction (or on rebuild) and hands it back; the encoder thread installs it with `attach`, which is also the only release point for the previous batch and for stale blocks. The render thread leases a block (`acquire`) and writes the whole frame directly into it; the consuming backend DMAs from the same memory (CUDA pinned pointer for NVENC, persistently mapped host-visible VkBuffer for the Vulkan Video Engine), eliminating the backend-side full-frame memcpy. Software encoding consumes heap blocks from the same pool, so there is exactly one host frame path; a frame with no free block is dropped rather than staged. Blocks are sized to the maximum geometry and released by RAII return.
_Avoid_: Pinned frame ring, CUDA pinned pool, Host staging ring, Refill ring, Publishing slot, Generation, Frame pool (as the old `BytesMut` staging pool)

**VRAM Direct Streaming**:
The end-to-end pixel pipeline where rendered frame buffers remain exclusively within GPU video memory and feed directly into hardware encoders without traversing CPU system memory or PCIe bus roundtrips (encompassing pure Vulkan zero-copy via dma-buf and two-stage intra-GPU CUDA-GL DMA).
_Avoid_: Direct streaming, Fast copy, Zero-Copy (as generic term across non-Vulkan backends)

**Texture Ring Buffer**:
A fixed multi-slot VRAM buffer pool that rotates frame textures between the render pipeline and the video encoder to prevent tearing while keeping the render path stall-free.
_Avoid_: Frame queue, Double buffer

**Timeline Semaphore**:
A monotonically increasing scalar synchronization primitive used to coordinate fine-grained GPU-to-GPU execution order between game rendering and video encoding with zero CPU polling.
_Avoid_: Mutex, Spinlock, Wait fence

**Hardware Color Space Converter**:
The dedicated GPU video processing silicon (e.g. NVENC hardware CSC, VA-API VPP, D3D11 Video Processor) that transforms RGB textures to NV12 within VRAM without shader compute overhead.
_Avoid_: Pixel converter, CPU YUV converter

**Vulkan Video Engine**:
The unified, cross-vendor GPU hardware video encoding pipeline leveraging official Khronos VK_KHR_video_encode extensions directly within the same Vulkan device instance; serves as the primary VRAM direct streaming engine for AMD Radeon, Intel Arc, and non-CUDA environments.
_Avoid_: Proprietary encoder, Driver wrapper

**Compute CSC Pipeline**:
The GPU compute-shader pipeline running within the Dedicated Encoder Actor that performs asynchronous RGBA-to-NV12 color space conversion and subsampling via embedded SPIR-V kernels, fulfilling hardware video encoder input requirements with zero CPU overhead.
_Avoid_: CPU converter, YUV blit

**Encoder Backend**:
The vendor-specific implementation that produces the video bitstream — hardware silicon (NVENC, VA-API, AMF) or software (OpenH264). Switching backends leaves the wire format unchanged.
_Avoid_: Codec, Encoder type, GPU support

**Codec**:
The video wire format a bitstream conforms to (H.264, H.265, AV1), determining how it is framed, negotiated, and decoded. Orthogonal to the Encoder Backend producing it: one Codec spans several backends, and one backend spans several Codecs.
_Avoid_: Format, Encoder, Video standard

**Hardware-Only Codec**:
A Codec whose pipeline implementation is strictly bound to dedicated GPU silicon (e.g. NVENC HEVC / AV1) with zero software CPU fallback; unsupported hardware configurations trigger automatic negotiation fallback to H.264.
_Avoid_: Unaccelerated codec, GPU-only format

**Video Codec Negotiation**:
The dynamic capability exchange during WebRTC handshake matching the Remote Player browser's offered codecs against the Game Instance GPU's hardware capabilities, resolving to the highest mutual tier (AV1 > H.265 > H.264) with optional client preference override.
_Avoid_: Codec handshake, Video format matching

**AV1 Tile Partitioning Ladder**:
The dynamic, resolution-and-framerate adaptive tiling strategy configuring AV1 NVENC encoding into 2x1 vertical tile columns for $\le$60 FPS and 4x1 tile columns for $>$60 FPS or 1440p, maximizing client-side parallel decoding throughput within 4.16ms budgets while preserving Minecraft high-frequency spatial compression efficiency.
**Unbuilt**: 该阶梯从未实装 —— NVENC AV1 按驱动默认分块编码。
_Avoid_: Static tiles, Grid slicing, Random tiling

**NVENC Ingest Engine**:
The hardware video encoding subsystem within Native Core that directly ingests Vulkan `VkImage` slot external memory via `cudarc` and the in-tree NVENC driver FFI (`encode/hardware/nvenc`) with zero CPU copy. OpenGL never enters this path — see **CUDA-GL Interop**.
_Avoid_: NVENC wrapper, Video encoder module

**PRIME File Descriptor**:
A Linux kernel POSIX file descriptor (`dma-buf`) exported from a Vulkan `VkDeviceMemory` allocation via `VK_KHR_external_memory_fd` for direct cross-API video memory sharing with CUDA.
_Avoid_: Linux fd, Memory pointer, Shared fd

**CUDA-GL Interop**:
The NVIDIA driver API mechanism (`cuGraphicsGLRegisterImage` and `cuGraphicsMapResources`) that bridges OpenGL texture IDs into CUDA arrays within the current thread's rendering context without PCIe roundtrips.
**Unused**: 项目已放弃此路径 —— OpenGL 后端一律走 RAM Readback,代码中无任何调用。词条保留仅因历史文档仍在引用。
_Avoid_: GL wrapper, Texture copy, Bridge hack

**Pipeline**:
The composition root of one video pipeline: the capture→encode seam (`FrameInbox`), the Doorbell, the Control Queue, the CPU frame recycle pool, the Dedicated Encoder Actor thread, and the `FrameOutbox` the actor posts into. One process runs one Pipeline; tests construct their own instance.
**It is a deep module**: callers see only `control`, `output`, `prepare_encoder`, `import_vram_slots`, `clear_vram_slots`, `slot_state` and `shutdown`; the Doorbell, the Control Queue's dispatch verbs and the outbox stay inside, so a caller cannot split an operation that must stay whole. `control()` is the one deliberate exception — it is handed to the `StreamingSession`, whose rate loop's sink and keyframe throttle each capture one clone. `output()` hands the outbox's `Arc` to that session's write loop, so the encoder's post and the wire write feed one outbox and one frame ledger. `spawn()` returns the finished object — there is no window in which a seam exists but is not yet owned.
_Avoid_: Global state, Engine context, Singleton

**Mailbox**:
The single-slot overwrite core both media seams are built on (`encode::frame::Mailbox<T>`): capacity one, `post` replaces any unconsumed value and rings the bell, `take` drains, `notified` waits for the next post. It carries the shared `FrameOutcomes` ledger. "Mailbox" is the Vulkan `VK_PRESENT_MODE_MAILBOX` sense — a single-entry queue where the newest replaces — never the Erlang accumulating sense.
_Avoid_: Queue, Ring buffer, Bounded channel

**FrameInbox**:
The capture→encode seam: a `Mailbox<VideoFrame>` plus the deferred-disposal queue (`discarded`) that parks displaced payloads for the actor thread to drop — pool locks and VRAM ownership bits must not land on the render thread. Its writer never blocks and its consumer takes linear RAII ownership. It is a register, not a queue — the producer owns the trigger (a render-thread callback that can neither wait nor be asked for a frame on demand), so a slow consumer drops stale frames upon overwrite rather than delaying new ones. (Replacing the transitional `triple_buffer` — the name Frame Inbox belonged to that dead seam and is recycled here.)
**Formerly FrameMailbox.**
_Avoid_: Frame queue, Ring buffer, Triple buffer

**Frame Outbox**:
The encode→wire seam: the single-slot overwrite handoff (`Mailbox<StampedVideoPacket>`, alias `FrameOutbox`) owned by the Pipeline — the actor posts each encoded video packet (one per frame; the track's packetizer fragments it into RTP packets downstream), the active session's write loop takes it. Named an outbox, not a slot: "slot" already means the VRAM encoding slots. It shares the FrameInbox's `FrameOutcomes` ledger, so the inbox, the actor and the write loop write one account. Unlike the inbox it parks no displaced payloads — an overwritten frame's `Bytes` drop in place. The Pipeline owns it across sessions; a frame left behind by a closing session is voided by the write loop's stale gate on the next take.
_Avoid_: Output slot, LatestFrame, Egress, Send queue, VideoWriter

**Doorbell**:
The zero-payload wakeup from producers to the Dedicated Encoder Actor: a `tokio::sync::Notify` shared as `Arc<Notify>` with `FrameInbox` and `ControlSender`, which the actor awaits in its `select!` alongside the Control Queue. Waking is idempotent — `notify_one` with no waiter stores a single permit, so repeated rings coalesce rather than queue. It is a bell, not a queue: the actor wakes once no matter how many rings accumulated while it was busy.
**Coalescing is carried by the type, not by a convention**: `Notify` stores at most one permit, so there is no capacity constant left to get wrong, and ringing has no failure path at all — `FrameInbox::push` runs on the render thread, and nothing there may propagate an error into capture.
_Avoid_: Wakeup queue, Notify pipe, Doorbell register, Signal bus

**Video Writer**:
Formerly **Track Binding**. One session's video writer (`streaming::writers::VideoWriter`): it holds the Pipeline's `FrameOutbox` and owns the tick pacing (`TickDiffer`), the waiting-for-keyframe gate and the Keyframe Throttle. Created on session establishment; the writer is constructed complete and consumed by value as its one write loop — nothing holds it afterwards, and that loop dies with the session.
**It exists so the throttle's lifetime equals the session's**: the writer dies with the session, so the 3000ms window cannot leak into the next session, while the encoder's pending-keyframe flag lives with the pipeline's actor (see Keyframe Throttle). A writer reads the outbox's shared `FrameOutcomes` ledger — the same one the inbox carries — so encoder and wire write into one account.
**Writing the track is the writer's own job, not the encoder's**: `post` is synchronous and unconditional (the Pipeline just drops the frame in its outbox, overwriting any unconsumed one). The write loop takes from it, applies the waiting-for-keyframe gate and the connection check, drops frames past the ~2-frame expiry threshold, paces the sample, enforces the 250ms write timeout, and records the outcome. The encoder thread therefore never waits on the network, never runs the gate, and `Admitted` is recorded on the write-loop side.
**It is pure data plane**: take → expiry gate → keyframe gate → pace → write. Reconfiguration and bitrate following are control-plane requests whose initiators — the signaling-driven profile update path on the `StreamingEngine` and the session's `RateControl` — live outside the writer; the writer never talks to the Pipeline's control plane.
_Avoid_: Session sink, Video port, Output target, Plane, Track binding, Binding

**Dedicated Encoder Actor**:
A dedicated native OS thread that owns every encoder instance for its whole lifetime — constructed, reconfigured and destroyed only on that thread, via the Control Queue — consuming frame tasks from the FrameInbox.
**The init-on-actor rule now holds for the encoder's own context**: `NvencBackend::new` creates its `CudaContext` on the actor thread (`encode/hardware/nvenc.rs`) and `import_vram_slots` binds through that same context, so no context or slot reference lives elsewhere. The former caveat — context and slot registry held by `CudaVulkanBridge` / `CudaGLBridge` on the JNI thread — no longer applies; those bridges are gone. The one remaining transient context is created inside `bootstrap_hardware_caps`, which only the engine-startup path calls.
_Avoid_: Encoder worker, Background task, spawn_blocking pool

**Latest-Frame-Wins**:
The overwrite policy at both seams that sit between a producer and a slower consumer — the FrameInbox (render→encode) and the Pipeline's `FrameOutbox` (encode→wire). A new publication replaces an unconsumed stale frame, permanently bounding pipeline backlog to exactly one frame at each seam under any rate burst.
_Avoid_: Drop-newest, FIFO pacing, Queue backlog

**Frame Acceptance**:
The outcome of publishing a frame to the encoder pipeline, meaning the frame entered the FrameInbox. Distinct from the frame being encoded, since Latest-Frame-Wins may overwrite an unconsumed frame. **The only rejection reason is a dead pipeline** — a busy actor whose wakeup did not fit in the channel is not a rejection.
**Not currently surfaced**: no interface reports this outcome. `FrameInbox::push`, `StreamingEngine::video().push_host_frame` and the Java-side `NativeBridge.pushSystemFrame` / `pushVramSlot` all return nothing, so the term names a distinction the code cannot make. On the Java side this is load-bearing: `VideoCaptureScheduler`'s documented policy — demote permanently when delivery is not accepted — is now carried by `CaptureOutcome` instead of a bare return value, so demotion happens only on a run of consecutive `Failed` frames (a thrown exception counts as `Failed`), never on a `Dropped` one; the RAM level has no demotion target and keeps capturing after its own breaker trips. The Rust side now keeps the count instead of the return value — see **Frame Outcomes**.
_Avoid_: Delivery, Enqueue success, Encode confirmation

**Frame Outcomes**:
The per-Pipeline tally of where every published frame ended up, carried as an `Arc` owned by the `FrameOutbox` (and handed to `FrameInbox` at construction) so that the render thread, the encoder thread and the session's write loop write into one account. Ten outcomes, each counted once and never decremented: _Admitted_ (encoded and written to the bound track; recorded by the write loop, not the encoder), _Superseded_ (let go before the write — pushed out of the inbox by the next frame, cleared on reconfiguration, or overwritten in the outbox), _PoolEmpty_ (no recycled block was free at publish time), _WriteFailed_ (the block rejected the bytes), _GeometryMismatch_ (the byte count disagreed with the encoder's current profile), _EncodeFailed_ (consumed but never reached the wire — no encoder yet, empty NALU, or backend error), _WriteStalled_ (the sample did not reach the track within the 250ms write timeout, or sat in the outbox past the ~2-frame expiry threshold; both are dropped, only the timeout also requests an IDR), _TrackWriteFailed_ (the write returned failure), _GateSkipped_ (the write loop dropped the frame: the waiting-for-keyframe gate was shut, or the session was not connected — not an error), _FenceDropped_ (rejected at the actor's frame step — a slot frame arrived while the actor was rejecting frames pending the slot-table re-import that follows a geometry or codec change, or a table clear; its busy bit is returned at the gate). It exists to make the arithmetic close: every published frame lands in exactly one of the ten, or is the one still in the inbox, or the one still in the outbox.
**Read-side is Rust-only**: `FrameOutcomes::snapshot` is test-only until a Java consumer exists; the logged outcomes each print their window count, and the wording / level of every such line lives in `FrameOutcomes`, with only the throttle window delegated to `RateLimitLog` (a caller states the outcome plus one detail sentence and nothing else). `PoolEmpty`, `WriteStalled` and `TrackWriteFailed` log at warn; `Admitted`, `Superseded`, `GateSkipped` and `FenceDropped` are silent by design.
_Avoid_: Frame statistics, Drop counter, Delivery metrics

**Control Queue**:
The actor-consumed command channel carrying operations to the Dedicated Encoder Actor. The vocabulary is closed and small: `Shutdown`, `BitrateChange`, `RequestKeyframe`, `ProfileChange`, `PrepareEncoder`, `ImportVramSlots`, `ClearVramSlots` (`encode/control.rs`). Delivery is non-blocking from the streaming stage, which runs on runtime threads where a blocking send would panic.
**The variant shape carries the acknowledgement discipline, so the type says which is which.** `Shutdown`, `BitrateChange` and `RequestKeyframe` are *tell* — they hold no reply channel, so a caller cannot wait for an answer that will never come. `BitrateChange` carries state that the next value supersedes; `RequestKeyframe` is an idempotent intent that collapses at the consume point. `ProfileChange`, `PrepareEncoder`, `ImportVramSlots` and `ClearVramSlots` are *ask* — the reply channel is a field of the variant, so an ask cannot be constructed without somewhere for the answer to go, and it cannot be dispatched and forgotten.
**`ControlSender` is the single door.** Seven named methods — `shutdown`, `set_bitrate`, `request_keyframe`, `change_profile`, `prepare_encoder`, `import_vram_slots`, `clear_vram_slots` — each documenting its own waiting rule; the generic `dispatch` / `dispatch_and_wait` verbs are gone. The control plane is owned by the `StreamingEngine`: reconfigures originate from its signaling-driven profile update path, encoder preparation from negotiation, slot imports from the JNI capture side through the `VideoPipeline`; bitrate follows originate from the session's `RateControl` — the single dispatch site — and keyframe requests from the session's `KeyframeThrottle`, each through the one `ControlSender` clone captured by its sink, so the session holds no control-plane state of its own. An unknown command no longer has a code path to be sent through.
**One reply carries data, not a status code.** `ProfileChange` answers with `Result<(StreamingProfile, Option<EncodeBackendKind>), ControlStatus>` — the profile the encoder actually settled on plus the backend identity actually built (`None` when the encoder is not yet live: the profile is adopted, nothing built), or the reason it was refused (`ReconfigureFailed`) — so the caller commits what the actor said instead of inferring it from success/failure or by comparing profiles, and no caller needs a second identity query. `PrepareEncoder` likewise answers with the built `EncodeBackendKind`, a silent fallback to software reported as software. The remaining asks answer with `ControlStatus`, converted to an `i32` code by `ControlStatus::code()` only at the JNI boundary (`jni/api.rs`).
**Backend preference is construction-time state, not a per-call argument.** `ControlSender::new` takes `prefer_hardware` once, from the caps bootstrap the pipeline already ran, and stamps it onto every `PrepareEncoder`; `prepare_encoder(profile)` cannot ask for a backend the machine does not have.
_Avoid_: Control channel, GlobalState lock, Signal bus

**Keyframe Gate**:
_Superseded_ — The overloaded gate coupling pipeline state with network pacing was split into the **Keyframe Throttle** and the control-plane keyframe request (`RequestKeyframe`).
_Avoid_: IDR flag, Global keyframe state, PLI counter

**Keyframe Throttle**:
The 3000ms anti-storm rate limiter owned by the `VideoWriter`, guarding against cascaded PLI/FIR packets and stalled or failed writes. `request_throttled()` dispatches at most one `RequestKeyframe` control command per window; every request path goes through it, so a burst of gate-skipped frames cannot stack up IDR requests at the worst moment with the largest bitrate.
**It is per-session (it lives and dies with the VideoWriter) while the encoder's pending-keyframe flag is per-pipeline, and that asymmetry is load-bearing**: a new session must not inherit the previous session's window, or a reconnecting client sits black until it expires — and because each writer is born with a fresh window, the session's first recovery request is never delayed by one. Do not hoist the throttle into the `Pipeline`.
_Avoid_: Pipeline throttle, Keyframe timer, Rate limiter

**Fill Sync**:
The per-encode GPU completion barrier ensuring the renderer's frame write is fully visible before NVENC reads the slot — timeline-semaphore wait plus context synchronize on the Vulkan path. There is no OpenGL path: that backend never reaches the encode seam.
_Avoid_: Fence spin, Polling wait, Implicit sync

**Fail-Closed Posture**:
The degraded-safe response to an actor fault or lifecycle acknowledgement timeout: mark the pipeline dead, log the cause, and route capture into the existing RAM Readback fallback chain instead of retrying or respawning.
**Partly unbuilt**: the RAM Readback fallback chain and the no-retry rule hold, and acknowledgement timeout is real (`ACK_TIMEOUT` = 5s). The "mark the pipeline dead" half has no mechanism — there is no liveness flag; `ControlStatus::ActorDead` is inferred from the control channel failing to send or disconnecting. See **Pipeline**.
_Avoid_: Actor respawn, Silent retry, Best-effort recovery

**VRAM Direct Downgrade**:
The permanent abandonment of VRAM Direct Streaming for the remainder of a Game Instance session, after slot registration or canary probing fails. All subsequent capture cascades to RAM Readback. It is triggered by **structural** unavailability — the hand-off path was never usable — and not by a single failed Frame Hand-off, which costs one frame of fallback and nothing more.
_Avoid_: Temporary fallback, Retry, Degradation mode, Zero-Copy Downgrade

**Two-Stage GPU DMA Pipeline**:
A split pipeline decoupling context-bound OpenGL texture mapping and asynchronous GPU-to-GPU DMA copying on the render thread from hardware video encoding on the dedicated actor thread, transferring frames via Scratch VRAM Slots while remaining completely off-CPU.
**Withdrawn**: 该设计已废弃 —— `ScratchVram` 槽与第三帧来源变体均不实装,OpenGL 后端固定走 RAM Readback。
_Avoid_: Double copy, GL hack, Split encoder

**Scratch VRAM Slot**:
A pre-allocated linear CUDA device memory buffer (`CUdeviceptr`) in Native Core serving as the destination for render-thread asynchronous 2D texture copies and the direct input resource for NVENC hardware encoding in the Two-Stage GPU DMA Pipeline.
_Avoid_: Temporary texture, intermediate buffer, staging RAM

**Sequence Parameter Set Delivery**:
The three-layer H.264 parameter-set guarantee (driver inlining via repeatSPSPPS as primary, FFI sequence-param retrieval as fallback, player-side duplication guard).
_Avoid_: SPS patcher, Header injection, Sequence Header Healing

**Scoped Export Allocation**:
The thread-bound RAII context within which Renderpearl textures are constructed with Vulkan external memory export flags and bound to the dedicated export pool, eliminating global static pool handles and string-label routing.
_Avoid_: Global export pool, Texture hook, Label sniff

**Normalized HID Scancode**:
The canonical keyboard input representation conforming to USB HID Usage Tables Page 0x07 transmitted in Input Frames, decoupled from Game Instance window subsystems (GLFW vs SDL3).
_Avoid_: Web keycode, SDL scancode, Platform key

**Streaming Profile**:
The server-owned video transmission configuration: Content Resolution + Streaming Framerate + BPP. The Remote Player controls only BPP and Display Fit Mode; geometry and pacing come from the Game Instance.
_Avoid_: Quality setting, Video mode, Dynamic profile

**Content Resolution**:
The Game Instance framebuffer's physical pixel dimensions — the capture source size and the stream's display resolution. Server-owned; never a Remote Player choice.
_Avoid_: Game resolution, Window size, Monitor resolution, Native resolution

**Streaming Framerate**:
The integer frames-per-second the encoder is configured for, following the Game Instance's configured maximum framerate, capped by the display refresh under VSync and by the codec level budget.
_Avoid_: FPS tier, Target FPS, Measured FPS

**Display Fit Mode**:
The Remote Player's client-side presentation policy mapping the encoded frame onto the browser viewport: contain (letterbox, default), cover (crop), or fill (stretch). It never changes the encoded frame.
_Avoid_: Aspect mode, Scaling mode, Crop setting

**Discrete Resolution Tier**:
_Superseded_ — Client-chosen 16:9 tokens ("720p"…"1440p") are gone; resolution is now Content Resolution, derived from the Game Instance and capped by the level budget.
_Avoid_: Arbitrary resolution, Screen size, 2K

**Discrete Framerate Tier**:
_Superseded_ — Client-chosen 30/60/120/144/180/240 FPS tokens are gone; streaming framerate now follows the Game Instance's configured maximum, capped by display refresh under VSync and by the level budget.
_Avoid_: Custom FPS, Variable refresh rate, Target speed

**Adaptive Bitrate Ladder**:
The sub-linear rate-distortion CBR bitrate allocation matrix dynamically mapping Content Resolution and Streaming Framerate to network budget, accounting for Minecraft's high spatial entropy and Wi-Fi bufferbloat constraints.
_Avoid_: Static bitrate, Linear bitrate, Hardcoded bandwidth

**Followed Bitrate**:
The one rate the encoder is currently instructed to run at, held by a single owner. Distinct from the Streaming Profile's own CBR target, which is its upper bound, and from any Remote Player's bandwidth report, which is an input to it.
_Avoid_: Effective bitrate, Target bitrate, Encoder bitrate, Estimate

**Rate Control**:
The single decision point that turns Remote Players' bandwidth reports into the Followed Bitrate, bounded by the Streaming Profile's CBR target and floor. It observes; the encoder follows.
_Avoid_: Congestion control, Bitrate policy, Adaptive bitrate

**Dynamic RTT BPP Adaptation**:
The transport-layer rate control mechanism measuring live WebRTC ICE CandidatePair round-trip-time (RTT via `pc.get_stats()`) and dynamically modulating Bits-Per-Pixel (BPP) across a continuous 20ms~80ms linear interpolation scale, stabilized with 0.02 quantization steps and a 0.019 deadband to prevent profile renegotiation jitter.
**Unbuilt**: 该机制从未实装 —— RTT 仅是浏览器 `getStats` 轮询后的 UI 展示读数，链路自适应由发送侧 GCC → Rate Control（Followed Bitrate）承担。
_Avoid_: Static LAN/WAN flag, Timestamp ping, Jittery profile switch

**Software Encoder Fallback Clamp**:
The fail-safe constraint clamping the content-driven Streaming Profile to min(Content Resolution, 1080p) at 60 FPS, with a warning log, when hardware encoding is unavailable and OpenH264 is in use.
**Unbuilt**: 该钳制从未实装 —— 硬编不可用时 Actor 直接以软编按同一档位兜底，无 1080p60 钳制。
_Avoid_: Software error, Resolution reject, Silent crash

**Media Clock**:
The media timeline carried by the video track's RTP timestamps, which must advance in lockstep with real time so that remote playout pace stays true and latency remains bounded.
_Avoid_: Fixed-frame-rate clock, Constant sample duration, Timer

**Frame Birth Timestamp**:
The monotonic timestamp stamped when a frame enters the encoding pipeline at its submission entry point, serving as the single source of truth for Media Clock increments.
_Avoid_: Encode time, Send time, Capture delay

**Controlling Player**:
_Superseded_ — With the removal of multi-viewer spectator mode, the distinction between a controlling player and spectator peers has ceased to exist. There is simply the single authenticated **Remote Player**.
_Avoid_: Master, Operator, Controller, Admin

**Spectator Peer**:
_Superseded_ — Multi-viewer spectator mode was eliminated in the single-session redesign; Vistrel permanently operates in 1-to-1 dedicated player streaming mode.
_Avoid_: Viewer, Observer, Guest, Secondary client

**StreamingSession**:
The unified active Remote Player's end-to-end streaming session, carried by same-named implementations on both endpoints: Native Core's session struct (WebRTC transport, dynamic Streaming Profile, Send-Side Rate Control, Input Frame ingestion, lifecycle managed by StreamingEngine) and the web client's session class (`web/src/streaming/session.js` — signaling negotiation over the signaling WebSocket, PeerConnection lifecycle, input DataChannel feeding, stats polling, and the auth/kick/reconnect state machine). The stream is transient inside the session — transports are built and torn down on reconnect while credentials and session state persist — so the session names the streaming activity, not the stream.
**Media adaptation is not session state any more**: tick pacing, the waiting-for-keyframe gate and the Keyframe Throttle live on the session's `VideoWriter`; the writers are consumed by value as their write loops (video and audio), so the session holds neither the `VideoPipeline` nor the `AudioLoopback` nor the writers — only their writer handles, cancelled and joined together on close. The reconfigure transaction lives on the `StreamingEngine` (ask → settled snapshot → the applied bitrate committed back into the session's rate loop); what stayed on the session is `RateControl` and the `KeyframeThrottle`, each capturing one clone of the engine's control plane (rate follows and keyframe requests) — the session holds no `ControlSender` of its own.
_Avoid_: ActiveSession, WebRtcSession, WebRTC session, RtcPeer, Peer connection, Connection

**Signaling Session**:
_Superseded_ — The dual-layer signaling/peer session concept was unified into **StreamingSession**.
_Avoid_: Peer connection, WebRTC session, Client

**Session Broadcast Pool**:
_Superseded_ — The 1-to-N fan-out distribution pool was replaced by the single-session `StreamingEngine` with Kick-Old preemption.
_Avoid_: Peer list, Client manager, Broadcast map

**Session Password**:
The secret token configured in the Game Instance required during initial handshake to authenticate the single Remote Player session before WebRTC streaming commences (formerly Session Passcode / Controller Passcode).
_Avoid_: Passcode, Admin password, Secret key, Auth token

**Session Generation**:
The strictly monotonic fencing token minted by Native Core's `StreamingEngine` (`session_epoch` counter) to sequence successive Remote Player connections, shielding session transitions against stale asynchronous transport events.
_Avoid_: Connection ID, Peer index, Session counter, Epoch

**Peer Event**:
_Superseded_ — The intermediate transport-level lifecycle channel and event enum was eliminated; WebRTC callbacks directly dispatch idempotent `SessionEvent` transitions (`Connected` / `Closed`) or spawn dedicated DataChannel coroutines.
_Avoid_: Session notification, WebRTC callback, Transport message

**Control Plane Push**:
_Superseded_ — The native→Java JNI status callback was removed: session lifecycle is driven by the Java signaling layer (offer acceptance / WS close), and profile updates are the return value of the blocking `apply_profile` ask.
_Avoid_: Reverse JNI, State query, Polling hook

**Session State Snapshot**:
The immutable record (`SessionState`) held in Java memory and updated by the signaling layer on offer acceptance and channel close, offering the render loop zero-overhead, tear-free reads of active session presence. It no longer carries negotiated geometry or framerate — those come from the blocking `apply_profile` / offer-answer result.
_Avoid_: Global flags, Cached status, Volatile booleans

**Encoder Channel**:
The dedicated bidirectional data-plane port (`EncoderChannel`): egress through which the capture engine submits rendered video frames and audio PCM to the streaming layer, ingress through which raw Remote Player input events are pulled per frame. Session lifecycle and Streaming Profile are owned by the signaling layer, not pushed through this port.
_Avoid_: StreamChannel, Stream Channel, Streaming Gateway, Frame Pipe, Push Bridge, Egress-only port

**External Handle**:
The cross-platform 64-bit opaque identifier representing an exported GPU VRAM allocation or synchronization timeline semaphore, mapping to POSIX file descriptors (`int fd`) on Linux and Win32 handles (`HANDLE`) on Windows across the unified JNI seam.
_Avoid_: Resource pointer, File descriptor (as generic cross-platform term), OS handle

**Win32 Opaque Handle**:
The Windows NT kernel handle representing an exported Vulkan memory allocation or timeline semaphore via `VK_KHR_external_memory_win32` / `VK_KHR_external_semaphore_win32`, imported directly into CUDA via `CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32` for zero-copy VRAM streaming on Windows.
_Avoid_: Win32 pointer, D3D resource, Windows fd

**Render Stack Generation**:
The three generations of Minecraft's client GPU abstraction the mod compiles against, expressed as Stonecutter constants: _Legacy GL_ (1.20 and 1.21.x, `LegacyOpenGlRamPipeline`, direct GL PBO readback), _blaze3d_ (26.1/26.2, `GpuDevice`/`CommandEncoder` under `com.mojang.blaze3d`, Vulkan backend only from 26.2), and _renderpearl_ (26.3+, the same abstraction re-homed to `com.mojang.renderpearl` with `FrontendGpuDevice`). Gates name the thing each version line introduces: `gpu_device` (≥26.1, the abstract device API), `vulkan_backend` (≥26.2, the game ships a Vulkan backend — driver extension support is probed at runtime, not here), `renderpearl_era` (≥26.3).
_Avoid_: "26.3 pipeline" (the stack is era-scoped, not version-scoped), New/old pipeline without naming the generation

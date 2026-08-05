# aimedia

[简体中文](README.md) | English

`aimedia` is an open-source, service-native live media runtime for developers and integrators.
Declare inputs, outputs, quality, latency, and failure policy; the runtime compiles an inspectable,
bounded execution plan and keeps the media job running.

It is not a full-format FFmpeg rewrite. The near-term goal is to replace common FFmpeg subprocess,
shell, and supervisor glue in live services while exposing a non-blocking path for AI analysis.

## Why it is different

- Intent is compiled into typed media nodes instead of being expanded into an opaque command.
- `aimedia explain` reports media type, memory domain, clock domain, queue policy, and GPU sessions.
- Reconnects, backpressure, fault boundaries, structured state, and shutdown are runtime concerns.
- Source timestamps are mapped onto an independent monotonic program clock.
- AI taps are sampled side paths and can never be required for media output to stay alive.

## Current status

The `0.2.0-alpha.1` Native Live Pipe release includes a typed graph compiler, streaming MPEG-TS demux/mux,
runtime-loaded libsrt and libxaac adapters, a bounded single-input scheduler, program clocks,
NVIDIA capability boundaries, deterministic switching policy, audio DSP, replay, benchmarks, and
fuzz targets.

An RTSP/RTP session boundary wraps `retina 0.4.19` behind aimedia-owned types. Interleaved TCP
H.264/AAC/G.711 now enters the bounded native single-input runtime directly; it remains experimental
until external-camera, GPU end-to-end, network impairment, and soak gates pass.

Frame-level NVDEC/NVENC and the production single-SRT GPU loop now run end to end. Input gaps keep
the last healthy frame with silent audio; SRT recovery resets the affected timeline, while output
recovery drops stale packets, emits fresh PAT/PMT, and requests an IDR. Runtime state now reports
every execution-plan edge, codec counters, NVDEC surface leases, and live input/output SRT stats.
The native single-SRT GPU data plane has passed FFmpeg/OBS/VLC interoperability, network
impairment, disconnect recovery, and a 1080p30 two-hour soak. The fixed single-SRT scope is now
supported; see the Chinese performance report and support matrix for exact boundaries.

The public configuration is now `aimedia/v1alpha2` `MediaJob`. Legacy `DirectorPipeline` files are
never loaded silently; migrate them explicitly and review the generated YAML:

```bash
cargo run -p aimedia -- explain -f examples/single-srt.yaml
cargo run -p aimedia -- explain -f examples/single-srt.yaml --json
cargo run -p aimedia -- run -f examples/single-srt.yaml --dry-run
cargo run -p aimedia -- run -f examples/single-srt.yaml --mock
cargo run -p aimedia -- config convert -f examples/v1alpha1.yaml -o media-job.yaml
```

The two-camera director remains available as an optional policy example:

```bash
cargo run -p aimedia -- replay examples/replay.jsonl -f examples/director.yaml
```

Workspace directories use concise contextual names (`core`, `graph`, `runtime`, `srt`), while
published Cargo package names retain the collision-resistant `aimedia-*` prefix.

Read the [architecture RFC](docs/rfcs/0001-intent-media-runtime.md),
[RTSP input RFC](docs/rfcs/0002-rtsp-input.md),
[roadmap](docs/roadmap.md), [user stories](docs/user-stories.md), and
[support matrix](docs/support-matrix.md). The verified scope and raw soak evidence are listed in
the [v0.2 release notes](docs/releases/v0.2.md).

The core is Apache-2.0. External transport, codec, model, and NVIDIA components retain their own
licenses. Open-source licensing does not grant codec patent rights.

# aimedia

[简体中文](README.md) | English

[![CI](https://github.com/anvsk/aimedia/actions/workflows/ci.yml/badge.svg)](https://github.com/anvsk/aimedia/actions/workflows/ci.yml)
[![Fuzz](https://github.com/anvsk/aimedia/actions/workflows/fuzz.yml/badge.svg)](https://github.com/anvsk/aimedia/actions/workflows/fuzz.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

`aimedia` is an open-source, AI-native director engine for live-streaming and AI application developers. Its long-term goal is to replace FFmpeg inside a deliberately narrow, tested support matrix: two synchronized SRT cameras in, one continuously encoded SRT program out.

> Development preview: the deterministic director core, configuration protocol, replay tools, and media parsers are runnable. The native SRT/NVDEC/NVENC data plane is not connected yet. `aimedia` is a working name pending naming and trademark review.

## What works today

- Strict `aimedia/v1alpha1` YAML validation with secret references.
- Monotonic program timestamps and fixed-capacity synchronization primitives.
- A two-camera director with minimum shot duration, hysteresis, candidate hold, cooldown, manual take/hold, and health failover.
- A VLM advisor contract with deadlines, expiry checks, strict JSON output, and a hard 25% score-weight ceiling.
- 48 kHz BS.1770-style rolling loudness estimation and an 80 ms equal-power audio crossfade.
- Clean-room MPEG-TS packet/PAT/PMT/PCR/continuity parsing plus H.264 Annex-B and AAC ADTS parsing.
- `probe`, `explain`, `run --dry-run`, `replay`, and `bench` commands.
- Linux fuzz targets for configuration, MPEG-TS, and elementary streams.

## What does not work yet

- Live SRT input or output through `libsrt`.
- NVDEC/NVENC or CUDA surfaces.
- H.264/AAC decoding and encoding.
- Silero VAD and visual ONNX analyzers.
- The native control API and loadable plugin function tables.

The project intentionally refuses to pretend these backends exist: `aimedia run` without `--dry-run` fails clearly until the native media path is linked.

## Try the director core

Rust 1.85 or newer is required.

```bash
cargo run -p aimedia -- explain -f examples/director.yaml
cargo run -p aimedia -- run -f examples/director.yaml --dry-run
cargo run -p aimedia -- replay examples/replay.jsonl -f examples/director.yaml
cargo run -p aimedia -- bench -f examples/director.yaml \
  --capture examples/replay.jsonl --iterations 1000
docker build -t aimedia:dev .
```

## Design rules

1. The media hot path never waits for a VLM.
2. Every queue has a fixed capacity and an explicit overflow policy.
3. Output uses an independent monotonic program clock.
4. AI produces bounded suggestions; a deterministic state machine makes the final switch.
5. Media protocols are implemented from public specifications without copying FFmpeg source.
6. Credentials must be environment-variable or mounted-file references.
7. A capability is not marked supported before interoperability testing.

See the [architecture](docs/architecture.md), [support matrix](docs/support-matrix.md), and [roadmap](docs/roadmap.md). Contributions are welcome, especially around MPEG-TS/PES reassembly, `libsrt`, NVIDIA Video Codec adapters, analyzer test corpora, and interoperability testing.

## License

The core is licensed under Apache-2.0. External transports, codecs, models, and NVIDIA runtimes retain their own licenses. Open-source source-code licenses do not grant H.264 or AAC patent rights; commercial distribution needs an independent legal review.

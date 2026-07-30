# pocket-character

A 3D digital-human desktop widget on the Pocket runtime family — the
[airi](https://github.com/moeru-ai/airi) VRM stage, reimplemented as **one
native process**: a transparent, always-on-top, frameless window rendering a
VRM character with idle animation, auto-blink, eye saccades and spring-bone
physics, driven by a QuickJS policy bundle.

Built to answer a measured question: *what does the same character widget
cost on the Pocket architecture instead of Electron?* See
[DESIGN.md](DESIGN.md) for the architecture and the parity contract, and the
measurement section below for the answer.

## Persona parity POC

This repository also owns the Pocket-native vertical slice of
[xikhar/persona](https://github.com/xikhar/persona). The two visual acceptance
commands prepare a pinned reference checkout, validate and stage the same local
VRM/VRMA inputs, build the selected target, launch it, and drive the same
idle/speaking/lip-sync/action sequence:

```sh
bun run accept:persona
bun run accept:pocket
```

Press Ctrl-C to terminate the complete target process tree. For the sequential
resource comparison:

```sh
bun run bench:persona
bun run bench:persona:controlled
```

See [docs/PERSONA.md](docs/PERSONA.md) for the parity boundary, benchmark
methodology, measurements, and asset-license constraints.

## What it does

- **AvatarSample_A** (VRoid official sample) with airi's `idle_loop.vrma`
  looped natively — retargeted at load, not baked.
- **Auto-blink** (sine 0.2 s envelope, 1–6 s uniform interval) and **idle eye
  saccades** (airi's interval distribution table), both from a deterministic
  seeded sim in `pocket-character-core`.
- **Spring bones** (hair / hood / bust) from the model's VRM data, solved by
  `pocket-vrm`'s verlet solver each tick.
- **Widget window**: 450×600 (airi's stage geometry), transparent,
  undecorated, always-on-top, drag anywhere to move, frame-paced at 60 fps
  (the loop sleeps; `--max-fps` to taste).
- **Guest policy bundle** (`app/main.ts` → QuickJS): the `character` surface
  delivers per-tick facts (`blink`, `hovered`, `fps`, events) and accepts
  intent ops (`setTracking`, `setExpression`, `playClip`, `quit`). The
  airi-parity personality is deliberately near-empty policy; a different
  character is a different bundle, no rebuild of the host.

## Layout

| Path | What |
|---|---|
| `crates/pocket-character` | macOS widget host (winit + wgpu via `pocket3d`) |
| `crates/pocket-character-core` | portable behavior sim (blink/saccade/look-at) |
| `app/` | guest bundle: `character` surface SDK + policy |
| `scripts/` | Bun TS: asset fetch, bundle build, run, measurement |
| `vendor/pocketjs` | the engine, pinned as a submodule |

The generic halves live in the PocketJS main repo:
`pocket3d` (morph targets, pose injection, widget windows) and `pocket-vrm`
(VRM 0.x parsing, spring bones, VRMA retargeting) — see
[pocket-stack/pocketjs#125](https://github.com/pocket-stack/pocketjs/pull/125).

## Manual verification, from scratch

Prerequisites: a Rust toolchain (stable) and [Bun](https://bun.sh). macOS
Apple Silicon is the measured platform.

```sh
# 1. Clone with the engine submodule
git clone --recurse-submodules https://github.com/pocket-stack/pocket-character
cd pocket-character

# 2. One-time setup: vendored bun install, node_modules symlinks,
#    and the model assets (downloaded, never committed)
bun run setup

# 3. Build guest bundle + release binary and launch the widget
bun run widget
```

`bun run widget` leaves the process attached to your terminal — quit with
Ctrl-C. Once built, launch directly:

```sh
target/release/pocket-character                # 60 fps (parity default)
target/release/pocket-character --max-fps 30   # low-power variant
```

What you should see: a transparent, undecorated, always-on-top 450×600
window with the character idling — looping motion, blinks every 1–6 s, eye
saccades, hair/hood physics. Drag anywhere on the character to move it.

Headless verification (no window; renders the same `Game` object offscreen —
the PNG's alpha channel is the actual window transparency):

```sh
target/release/pocket-character --headless-shot shot.png --ticks 90
```

Reproduce the measurements (launches its own instance, settles 15 s, then
samples ≥60 s and prints a `RESULT` JSON line + markdown row):

```sh
bun scripts/measure.ts                 # 60 fps
bun scripts/measure.ts --max-fps 30    # 30 fps variant
```

### Reproducing the airi side of the comparison

airi's out-of-the-box stage is **Live2D (a different character on a
different renderer)** — for an apples-to-apples screenshot or measurement it
must be switched to its VRM stage with the same model pocket-character
renders (AvatarSample_A). Either pick **AvatarSample_A** in airi's settings
→ Models, or script it:

```sh
/Applications/AIRI.app/Contents/MacOS/AIRI --remote-debugging-port=9222 &
bun scripts/airi-vrm.ts                    # switch to preset-vrm-1 (AvatarSample_A)
bun scripts/airi-vrm.ts preset-live2d-1    # revert
```

Give the VRM scene ~60 s to settle, hands off, then measure. Sum **all**
AIRI processes (search "AIRI" in Activity Monitor — besides the two obvious
helpers there are GPU/network/audio services and a hidden beat-sync
renderer; close the onboarding window first or count its renderer too).

### Reading CPU numbers

All CPU percentages here (and in Activity Monitor's per-process column,
`ps`, `top`) are **percent of one core** — 100 % = one core saturated, so a
16-core machine totals 1600 %. Activity Monitor's bottom System/User/Idle
summary is normalized to the whole machine instead, and covers *all*
processes, not this one. Two things inflate a casual glance right after
launch: the first ~2 CPU-seconds are model decode (init), and clicking or
dragging the widget adds work — judge idle cost only after ~1 min of
hands-off settling, which is what `scripts/measure.ts` automates.

## Model & animation assets

Fetched at setup, never committed: the VRoid sample-model terms and the
animation's provenance are not MIT. Same posture as airi, which downloads
them at build time.

## Measurements

See [REPORT.md](REPORT.md) for the full comparison against airi on the same
machine, same methodology.

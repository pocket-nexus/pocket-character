# Pocket Persona — a controlled native VRM vertical slice

*A feature-parity proof of concept for
[xikhar/persona](https://github.com/xikhar/persona), implemented as one
Pocket-native process so the architecture can be measured without carrying
Electron, React, React Three Fiber, or Three.js into the result.*

Pocket Persona is deliberately a clean vertical slice, not a source fork. It
keeps Persona's observable character contract — the library schema, VRM/VRMA
content, voice-state transitions, facial behavior, animation actions, local
event bridge, and MCP tools — while replacing the implementation beneath that
contract with `pocket3d`, `pocket-vrm`, `pocket-widget`, and a `pocket-mod`
QuickJS guest.

This is a performance and architecture POC, not yet a drop-in replacement for
the complete Persona desktop product. The exact boundary is recorded below.

## One-line visual acceptance

From this repository, each command prepares its own pinned inputs, builds the
target, launches it attached to the terminal, and drives the same visible
idle → speaking/lip-sync → listening → greeting → idle sequence:

```sh
bun run accept:persona
bun run accept:pocket
```

The first command clones Persona commit `4efec3ac…` into the ignored
`out/persona-reference/` directory; it never modifies a user checkout. Both
commands validate the same VRM/VRMA hashes and stage the checked-in
`fixtures/persona/library.json`. Press Ctrl-C to terminate the complete target
process tree.

Run the sequential production or controlled resource comparison with:

```sh
bun run bench:persona
bun run bench:persona:controlled
```

The benchmark never runs both renderers concurrently.

## 1. Reference and decision

The reference was inspected and measured at Persona commit
[`4efec3ac729944d0b36137dd8847cc1b488e0bcb`](https://github.com/xikhar/persona/tree/4efec3ac729944d0b36137dd8847cc1b488e0bcb),
version `0.1.0-beta.0`.

Primary upstream references:

- [README and product contract](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/README.md)
- [Architecture and development](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/docs/DEVELOPMENT.md)
- [Local bridge and MCP integration](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/docs/INTEGRATIONS.md)
- [Electron lifecycle and control plane](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/electron/main.cjs)
- [React/Three scene](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/src/components/Scene.tsx)
- [VRM animation component](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/src/components/Avatar.tsx)
- [Source license](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/LICENSE)
- [Asset-license boundary](https://github.com/xikhar/persona/blob/4efec3ac729944d0b36137dd8847cc1b488e0bcb/ASSET_LICENSES.md)

The local `~/code/pocket-character` checkout used for the implementation
comparison was at
[`b0932770ba0b4f4a490f607652b0cf09c76ab513`](https://github.com/pocket-stack/pocket-character/tree/b0932770ba0b4f4a490f607652b0cf09c76ab513).
It already proved the relevant Pocket primitives: one native wgpu process,
VRM 0.x parsing, VRMA retargeting, spring bones, morph expressions, a
demand-rendered widget shell, and a small QuickJS policy surface.

### Why a clean vertical slice

A fork would preserve the largest variables under test: Electron's process
tree, Chromium's renderer and GPU service, React reconciliation, React Three
Fiber, and Three.js. It could demonstrate UI changes, but it could not answer
how much of Persona's steady-state cost comes from that architecture.

The clean slice instead treats Persona as a behavioral and data contract:

1. Consume the same `library.json`, `.vrm`, and `.vrma` inputs.
2. Preserve the same idle/speaking/custom-action state machine and transition
   timings.
3. Preserve the same loopback event shapes and four MCP tools.
4. Keep continuous simulation, facial motion, physics, and rendering native.
5. Keep product-specific policy hot-swappable as a bounded QuickJS bundle.
6. Measure the whole launched process tree sequentially on the same machine.

This makes the controlled benchmark a cap-controlled POC comparison with the
same data contract and source texture ceiling. It exposes the runtime-stack
effect without pretending to isolate architecture completely: the renderer
feature and texture-role differences in section 4 still apply. A second,
production-oriented lane measures Pocket's intentional quality/power choices
separately.

## 2. Architecture and data flow

Persona's reference stack has four process and trust layers:

```text
Core Audio / WASAPI / PipeWire output activity
  -> native listener process or PipeWire adapter
  -> Electron main process
       settings store + window/tray/protocol lifecycle
       loopback HTTP events + Streamable HTTP MCP
  -> sandboxed preload bridge / IPC
  -> React 19 + React Three Fiber + Three.js
       VRM/VRMA loading, animation mixer, blink/lip hooks, WebGL rendering
```

The Pocket slice collapses the continuous path into one native process:

```text
Persona library.json
  -> immutable catalog validation
  -> pocket3d GLB upload + pocket-vrm VRM parse
  -> pocket-vrm VRMA retargeting

loopback HTTP /events or /mcp
  -> bridge worker thread
  -> bounded BridgeCommand channel
  -> fixed-step PersonaWidget core
       action selection and crossfade
       skeleton pose + spring bones
       blink + amplitude visemes
       morph upload + pocket3d render
  -> bounded state/events into pocket-mod QuickJS
  <- guest intents: playAnimation, setExpression, quit
```

The native core owns time and all per-frame work. The guest receives only:

```text
{ t, activity, audioLevel, animation, blink, renderFps }
{ type: "animationChanged" | "voiceChanged", value }
```

The guest cannot access the filesystem, network, renderer, or raw model. It
can request `playAnimation(name)`, `setExpression(name, weight)`, or `quit()`.
That is the Pocket runtime-family split: native core, narrow surface,
hot-swappable guest.

The implementation lives in:

| Path | Responsibility |
| --- | --- |
| `crates/pocket-persona/` | Product-level native core, catalog, bridge, MCP server, deterministic face simulation, and guest bundle |
| `vendor/pocketjs/engine/crates/pocket-vrm/` | VRM 0.x semantics, expressions, spring bones, VRMA parsing and humanoid retargeting |
| `vendor/pocketjs/engine/pocket3d/crates/pocket3d/` | glTF upload, skinning, morph targets, camera, wgpu renderer, and offscreen output |
| `vendor/pocketjs/engine/crates/pocket-widget/` | Transparent native window, fixed ticks, frame pacing, occlusion handling, demand rendering, and show/hide requests |
| `scripts/accept-persona.ts` | Pinned reference setup, asset staging, target build/run, visible event sequence, and cleanup |
| `scripts/bench-persona.ts` | Sequential, full-process-tree A/B resource harness |
| `fixtures/persona/library.json` | Canonical media-free acceptance catalog shared by both targets |

The PocketJS submodule currently pins
[pocket-stack/pocketjs#204](https://github.com/pocket-stack/pocketjs/pull/204).
That upstream change contains only generic runtime window Show/Hide support;
the Persona catalog, renderer product, bridge, MCP server, acceptance commands,
benchmark, and report remain owned by this repository. Repin the submodule to
the eventual PocketJS merge commit before merging this product change.

## 3. Implemented parity

“Parity” here means the visible/runtime slice listed in this table. It does
not mean that the deferred desktop-management features in the next section
are silently present.

| Contract | Persona reference | Pocket Persona | Status |
| --- | --- | --- | --- |
| Character input | Packaged or imported VRM | Default model from Persona schema-v1 `library.json` | Implemented for the controlled VRM 0.x asset |
| Animation input | One or more VRMA clips per action | Same relative `.vrma` paths, parsed and retargeted at load | Implemented for the controlled clip; loader limits are below |
| Idle role | Permanent `IDLE` action | Looped native idle action | Implemented |
| Speaking role | Permanent `TALK` action follows voice output | Looped native speaking action follows validated voice state | Implemented; state is externally supplied |
| Custom actions | Named one-shot actions | Named one-shots, then resume the current voice role | Implemented |
| Clip choice | Random clip, avoid immediate repeat | Deterministic seeded choice, avoid immediate repeat | Behavior parity with reproducible tests |
| Crossfades | General `0.7 s`; into speaking `0.85 s`; speaking to idle `1.15 s` | Same durations with smoothstep TRS blending | Implemented |
| Talk-to-idle hold | Listener activity gate plus app-side settling | `0.65 s` app-side delay after listening state | Implemented at the app boundary; native gate deferred |
| Lip sync | Amplitude-only five-viseme driver | Same five VRM expression families, audible threshold, attack/release smoothing, and `0.62` cap | Implemented |
| Blink | Random `2–6 s`, `0.24 s` envelope | Deterministic seeded `2–6 s`, `0.24 s` sine envelope | Implemented |
| Secondary motion | VRM spring bones | Native `pocket-vrm` spring solver every fixed tick | Implemented |
| Voice/action interaction | One-shot body action can override voice body motion while lip sync continues | Same; voice role resumes after the one-shot | Implemented |
| Camera framing | Full-body framing at default character size | Bounds-derived 20-degree perspective framing calibrated to Persona's default | Implemented, visually approximate |
| Camera input | Scroll zoom, left orbit, right pan | Same gestures; pitch and distance are bounded | Implemented |
| Window | `430×680`, transparent, frameless, topmost | Same logical default, resizable with `320×480` minimum | Implemented |
| Render scheduling | Continuous browser animation loop | Fixed tick plus dirty-frame governor; clips, crossfades, spring motion, and face changes re-arm presentation | Implemented |
| Local status | `GET /health` | Loopback-only `GET /health` with model, window, voice, audio, animation, and render state | Implemented |
| Event bridge | `POST /events` | Same `state`, `audio-level`, and `animation` event shapes; bounded and validated | Implemented |
| MCP animations | `play_animation`, `list_animations` | Same names and catalog metadata | Implemented |
| MCP window/status | `control_window`, `get_status` | Same names and show/hide/toggle/status behavior | Implemented |
| Agent policy seam | Electron/MCP control plane | `pocket-mod` QuickJS guest receives facts/events and emits bounded intents | Implemented, intentionally Pocket-shaped |
| Headless acceptance | Renderer tests, no product-level offscreen command | Fixed-step offscreen render to a transparent PNG | Implemented |

The bridge binds only to `127.0.0.1`, limits requests to 64 KiB, validates the
`Host` and optional `Origin`, clamps audio levels to `[0, 1]`, validates voice
enums and action names, and transfers commands to the render thread through an
MPSC queue. The MCP endpoint implements the JSON-RPC methods needed by the
four tools; it is a deliberately small Streamable HTTP subset with one static
local session, not a vendored copy of the upstream JavaScript MCP SDK. It has
been exercised with the official SDK client, but dynamic session allocation,
strict protocol-version negotiation, session expiry, and tools-changed
notifications remain desktop control-plane work.

## 4. Deliberately deferred gaps

The following are out of the POC's parity claim:

### Settings, catalog mutation, and imports

Pocket Persona reads an immutable, validated subset of Persona's schema-v1
`library.json`. It selects `default_model_id` or the first model, and reads
action names, descriptions, trigger scenarios, roles, and relative clip
paths. The catalog rejects absolute paths and lexical `..` traversal, but the
library directory remains a trusted local input: symlink targets are allowed
so local, uncommitted development assets can stay outside either checkout.
This is not a filesystem sandbox for an untrusted catalog.

The catalog also enforces Persona's unique model/animation IDs and names,
allowed animation types, and reserved `system-idle`/`system-speaking` slots.
Missing system slots are materialized as empty permanent actions, matching the
reference catalog behavior.

It does not implement Persona's Settings window, previews, model switching,
character-size preference, user-level `settings.json`, copy-on-write packaged
overrides, tombstones, reset flow, or the model/action CRUD UI. It also does
not implement `.vrm`/`.vrma` import, upload limits, or library migration.
Those are desktop product and persistence features, not part of the renderer
performance slice.

### Native audio detection

Pocket Persona does not ship Persona's Core Audio, WASAPI, or PipeWire output
listener. It never records, stores, transcribes, or sends audio. The local
bridge accepts normalized voice state and amplitude, so the complete character
behavior can be exercised deterministically by a test client or a future
platform adapter.

This means automatic detection of a supported voice application's output,
macOS audio-capture permission UX, the listener's activity gate, native helper
lifecycle, and listener health reporting are deferred. Do not describe the
current POC as automatic voice detection.

### Desktop lifecycle and protocol

The native window supports close, resize, topmost presentation, occlusion
suspension, and MCP show/hide. It does not yet provide Persona's tray menu,
global shortcut, background-start behavior, login/startup integration,
single-instance handoff, all-Spaces macOS policy, or `persona://` protocol
registration and deep-link actions.

### Rendering differences

The visual output is intentionally not a pixel-parity port:

- Persona uses `@pixiv/three-vrm`/Three.js, its VRM material path, a
  `dawn.exr` environment, directional and ambient lights, sRGB output,
  `NoToneMapping`, and a device-pixel-ratio cap of `1.5`. Its Settings preview
  also adds contact shadows.
- Pocket currently uses glTF base-color materials with pocket3d's simple
  hemisphere-plus-sun shader, `lit = 0.25`, alpha cutout `0.5`, and a
  transparent clear. `pocket-vrm` parses MToon facts, but this POC does not
  render the complete MToon shading model.
- Pocket's production default halves oversized textures until their longest
  side is at most `2048`; the reference keeps the model's `4096` authoring
  textures. Pocket also uploads only images sampled as base color by a used
  material. These are intentional memory levers, not free visual parity.
- Persona's live renderer follows the display refresh rate. Pocket's
  production tick/render cap is `60 Hz`; its active idle clip therefore
  presents at most 60 frames per second.
- Orbit inertia/damping, exact HDR lighting, exact alpha/material behavior,
  preview contact shadows, and pixel-identical framing remain deferred.

The accepted 3D formats are narrower than Persona's general Three.js path.
This POC supports VRM 0.x, not VRM 1.0. Its VRMA loader consumes the first
animation, retargets humanoid rotation plus hips translation, and supports the
accessor subset exercised by the controlled clip. Sparse accessors, exact
CUBICSPLINE interpolation, non-humanoid translation, and
`VRMC_vrm_animation` expression/look-at tracks are not parity claims.

For that reason, performance must be reported in two lanes: a controlled
`120 Hz / 4096` run that avoids crediting Pocket for a lower configured
quality target, and the actual `60 Hz / 2048` production default that measures
the user-facing optimization.

## 5. Asset and license boundary

The Persona source is MIT, but its own asset policy explicitly says that MIT
does not grant rights to local VRM or VRMA media. Persona's distributable
catalog is empty at the pinned commit, and there is no reference character
asset in the repository that this POC can legally copy by implication.

The controlled local inputs are:

| Input | Size | SHA-256 |
| --- | ---: | --- |
| `~/code/pocket-character/assets/AvatarSample_A.vrm` | 26,781,812 bytes | `2a0ccd84880b03d7b65503d8b6287f7a97f3bb4fab70a5fd0a47b433c97827f5` |
| `~/code/pocket-character/assets/idle_loop.vrma` | 157,664 bytes | `ace95ba6dcc0bdf2ed1081c002332b4184441117c8d543b6f642b3d2c5cf99be` |

They are local development inputs only. `pocket-character` also fetches them
instead of committing them because the VRoid sample terms and animation
provenance are not covered by its source license. Do not commit them here,
attach them to a release, or infer redistribution permission from either
project's source license.

The VRM contains 40,406 vertices, 29,221 triangles, 95 nodes, three skins,
seven primitives/materials, and thirteen PNG images. The source images include
four `4096²` and five `2048²` textures. Decoded RGBA is approximately
336.50 MiB before mipmaps and 448.67 MiB with a full mip chain, which is why
the source texture ceiling is held constant in the controlled lane and changed
only in the production lane.

No Persona art, icon, HDR environment, VRM, or VRMA file is added to this
repository by the POC.

## 6. Reproducible local library

The acceptance wrapper owns this setup in normal use. The expanded commands
below document what it stages inside its ignored Persona checkout:

```sh
export POCKET_CHARACTER_ROOT="$PWD"
export PERSONA_ROOT="$POCKET_CHARACTER_ROOT/out/persona-reference"
export PERSONA_ASSETS="$PERSONA_ROOT/public/assets"
export PERSONA_LIBRARY="$PERSONA_ASSETS/library.json"

cd "$PERSONA_ROOT"
git switch --detach 4efec3ac729944d0b36137dd8847cc1b488e0bcb

mkdir -p "$PERSONA_ASSETS/models" "$PERSONA_ASSETS/animations"
ln -sfn "$POCKET_CHARACTER_ROOT/assets/AvatarSample_A.vrm" \
  "$PERSONA_ASSETS/models/model.vrm"
for name in idle talk1 talk2 greeting happy finger-gun dance; do
  ln -sfn "$POCKET_CHARACTER_ROOT/assets/idle_loop.vrma" \
    "$PERSONA_ASSETS/animations/$name.vrma"
done

cat >"$PERSONA_LIBRARY" <<'JSON'
{
  "schema_version": 1,
  "default_model_id": "packaged-model",
  "models": [
    {
      "id": "packaged-model",
      "model_name": "Packaged model",
      "asset_path": "models/model.vrm"
    }
  ],
  "animations": [
    {
      "id": "system-idle",
      "animation_name": "idle",
      "animation_description": "A calm resting motion for the character.",
      "animation_trigger_scenario": "Used automatically while Persona is waiting and not speaking.",
      "animation_type": "IDLE",
      "asset_paths": ["animations/idle.vrma"]
    },
    {
      "id": "system-speaking",
      "animation_name": "speaking",
      "animation_description": "Natural conversational body movement while the character speaks.",
      "animation_trigger_scenario": "Used automatically while supported voice output is active.",
      "animation_type": "TALK",
      "asset_paths": [
        "animations/talk1.vrma",
        "animations/talk2.vrma"
      ]
    },
    {
      "id": "packaged-greeting",
      "animation_name": "greeting",
      "animation_description": "A friendly greeting motion.",
      "animation_trigger_scenario": "Use when beginning an interaction or welcoming the user.",
      "animation_type": "GREETING",
      "asset_paths": ["animations/greeting.vrma"]
    },
    {
      "id": "packaged-happy",
      "animation_name": "happy",
      "animation_description": "A warm, upbeat reaction.",
      "animation_trigger_scenario": "Use for good news, success, gratitude, or a positive response.",
      "animation_type": "HAPPY",
      "asset_paths": ["animations/happy.vrma"]
    },
    {
      "id": "packaged-finger-gun",
      "animation_name": "finger-gun",
      "animation_description": "A playful finger-gun gesture.",
      "animation_trigger_scenario": "Use for lighthearted confidence, a clever solution, or playful approval.",
      "animation_type": "FINGER_GUN",
      "asset_paths": ["animations/finger-gun.vrma"]
    },
    {
      "id": "packaged-dance",
      "animation_name": "dance",
      "animation_description": "A celebratory dance.",
      "animation_trigger_scenario": "Use for a major success, an exciting milestone, or an explicit request to dance.",
      "animation_type": "DANCE",
      "asset_paths": ["animations/dance.vrma"]
    }
  ]
}
JSON
```

All seven staged paths intentionally resolve to the one available local clip;
the speaking slot has two aliases so the no-immediate-repeat path is exercised.
This exact catalog was used by both final benchmark reports. It verifies role
switching, one-shot completion, clip choice, crossfades, and control-plane
behavior without inventing or distributing additional media; it is not a claim
that the motions are artistically distinct.

Build the pinned reference after staging the catalog so Vite copies the same
media into `dist`:

```sh
cd "$PERSONA_ROOT"
npm ci
npm run native:build
npm run native:test
npm run check
```

Return to the Pocket Character checkout before running the remaining commands:

```sh
cd "$POCKET_CHARACTER_ROOT"
export PERSONA_LIBRARY="$PERSONA_ROOT/public/assets/library.json"
```

## 7. Build, run, and headless verification

The normal one-line command builds the minified IIFE guest and release native
binary before launching:

```sh
bun run accept:pocket
```

Outputs:

```text
dist/pocket-persona/guest.js
dist/pocket-persona/guest.js.map
target/release/pocket-persona
```

Run the production-oriented defaults — `430×680`, fixed `60 Hz`, texture cap
`2048`, loopback bridge on port `47831`:

```sh
./target/release/pocket-persona \
  --library "$PERSONA_LIBRARY" \
  --bundle "$PWD/dist/pocket-persona/guest.js" \
  --fps 60 \
  --max-texture-dim 2048
```

Disable the control plane for a renderer-only manual run:

```sh
./target/release/pocket-persona \
  --library "$PERSONA_LIBRARY" \
  --bundle "$PWD/dist/pocket-persona/guest.js" \
  --fps 60 \
  --max-texture-dim 2048 \
  --no-bridge
```

Render a deterministic fixed-step acceptance frame without creating a window.
Headless mode disables the bridge automatically:

```sh
mkdir -p "$PWD/dist/pocket-persona"
./target/release/pocket-persona \
  --library "$PERSONA_LIBRARY" \
  --bundle "$PWD/dist/pocket-persona/guest.js" \
  --fps 60 \
  --max-texture-dim 2048 \
  --ticks 90 \
  --headless-shot "$PWD/dist/pocket-persona/headless.png"
```

Useful native tests:

```sh
cargo test -p pocket-persona
cargo test --manifest-path vendor/pocketjs/engine/Cargo.toml -p pocket-vrm
```

## 8. Bridge acceptance

Keep the normal windowed process running, then use a second terminal.

Read health and current state:

```sh
curl --fail-with-body --silent --show-error \
  http://127.0.0.1:47831/health | jq
```

Enter speaking state and drive the amplitude-only lip synchronizer:

```sh
curl --fail-with-body --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"type":"state","state":{"phase":"active","activity":"speaking","microphoneMuted":false,"outputMuted":false}}' \
  http://127.0.0.1:47831/events

curl --fail-with-body --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"type":"audio-level","level":0.28}' \
  http://127.0.0.1:47831/events
```

Return through listening to idle after the `0.65 s` app-side delay:

```sh
curl --fail-with-body --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"type":"state","state":{"phase":"active","activity":"listening","microphoneMuted":false,"outputMuted":false}}' \
  http://127.0.0.1:47831/events
```

Play the configured one-shot action:

```sh
curl --fail-with-body --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"type":"animation","animation_name":"greeting"}' \
  http://127.0.0.1:47831/events
```

Expected event responses are HTTP `202` with `{"accepted":true}`. Malformed
JSON returns `400`; a structurally invalid event returns `422`.

## 9. MCP acceptance

Initialize the minimal Streamable HTTP endpoint and inspect its session header:

```sh
curl --silent --show-error --include \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}' \
  http://127.0.0.1:47831/mcp
```

List tools:

```sh
curl --fail-with-body --silent --show-error \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'Mcp-Session-Id: pocket-persona-v1' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  http://127.0.0.1:47831/mcp | jq
```

Call an animation and read status:

```sh
curl --fail-with-body --silent --show-error \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'Mcp-Session-Id: pocket-persona-v1' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"play_animation","arguments":{"animation":"greeting"}}}' \
  http://127.0.0.1:47831/mcp | jq

curl --fail-with-body --silent --show-error \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'Mcp-Session-Id: pocket-persona-v1' \
  -d '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_status","arguments":{}}}' \
  http://127.0.0.1:47831/mcp | jq
```

To make the running POC available to new Codex sessions:

```sh
codex mcp add pocket-persona --url http://127.0.0.1:47831/mcp
```

## 10. Performance methodology

Do not launch Persona and Pocket Persona together. Chromium GPU work, shader
compilation, asset decoding, and workspace builds materially contaminate each
other. `scripts/bench-persona.ts` runs the reference first, terminates its full
process group, then runs Pocket. For each target it:

1. requires `GET /health` to return `200` with `ok: true`, then waits for the
   configured settle period;
2. snapshots every descendant process;
3. samples cumulative process CPU time over fixed intervals;
4. sums RSS and records the complete process inventory;
5. reports median and p95 values; and
6. captures a health receipt alongside every resource sample; and
7. writes machine facts, exact launch commands, raw samples, and comparison
   formulas to JSON.

The harness requires `--library` to resolve to the reference checkout's own
`public/assets/library.json`, so both targets cannot silently benchmark
different catalogs. Both always-on-top windows must remain visible and
uncovered. The harness fails closed if Pocket's health receipts stop reporting
a configured, visible model and delivered frames, so compositor suspension
cannot silently become a renderer optimization.

Persona starts its bridge before the avatar renderer, so its health response
is a control-plane readiness check rather than proof of visible model output.
The benchmark result must therefore be paired with a rendered-model
screenshot and a frame-rate receipt. Pocket health is stronger: the bridge
starts after model/animation/guest initialization and reports `renderFps`,
`modelConfigured`, and `windowVisible` in every sample.

CPU percentages are percent of one logical core: `100%` means one saturated
core. Summed RSS is useful for a same-machine process-tree comparison, but it
can double-count shared mappings and is not a substitute for platform physical
footprint tools.

### Cap-controlled POC lane: same asset, 120 Hz, 4096 textures

Use the same `library.json` and source hashes for both targets. Set the macOS
display to `120 Hz`, keep both windows untouched and visible, and ensure no
other build or benchmark overlaps the run. Persona is display-driven; Pocket
is explicitly fixed/capped at `120`.

```sh
bun run bench:persona:controlled
```

`4096` leaves the controlled model's source textures at authoring resolution.
This is the strongest like-for-like POC lane, but not an architecture-only
headline: Pocket still omits the reference MToon/HDR path and unused texture
roles. Record the actual frame rate and physical framebuffer of each window as
a companion receipt: the reference caps DPR at `1.5`, while the native
swapchain follows the OS backing scale. Equal logical window size does not by
itself prove equal fragment workload.

### Production lane: Pocket 60 Hz, 2048 textures

This keeps the reference unchanged and applies Pocket's intended shipping
defaults. It measures the total user-facing saving including the refresh and
texture-cap choices:

```sh
bun run bench:persona
```

Before reporting a result, verify the two JSON reports have `status: "ok"`,
both runs contain all requested samples, the commands contain the intended
caps, the asset hashes still match this document, and no unexpected helper or
build process joined either process tree.

## 11. Performance results

Both final sequential reports completed with `status: "ok"` on 2026-07-30.
Each target settled for 30 seconds and then produced nine five-second process
tree samples. No build or second benchmark overlapped either run.

Reference environment: MacBook Pro `Mac15,8`, Apple M3 Max, 128 GiB RAM,
macOS `26.5.2 (25F84)`, Electron `39.8.10`, Chromium `142.0.7444.265`,
logical viewport `430×680`, and a 120 Hz display. Persona rendered a
`645×1020` WebGL canvas at DPR `1.5`; Pocket's native 2× swapchain was
`860×1360`, or 1.78 times as many backing pixels. Both targets consumed the
same catalog, 26,781,812-byte VRM, and 157,664-byte VRMA recorded above.
Neither run included Persona's optional native audio helper.

RSS comes from the nine process-tree snapshots and remains a diagnostic
mapping total. The physical-footprint rows come from separate 30-second
sustained-render measurements with macOS `footprint`: all four Electron
process IDs were included for Persona, and Pocket had one process. The
reference footprint was measured once because the reference configuration is
identical in both lanes. These are steady-state snapshots, not startup peaks.
Pocket's auxiliary per-process peak fields were 1,400,440,104 bytes at 4096
and 1,030,063,256 bytes at 2048, so this POC makes no peak-memory reduction
claim.

### Controlled result

This lane used the same authoring-resolution textures and requested 120 Hz
from Pocket:

| Metric | Persona | Pocket | Pocket delta |
| --- | ---: | ---: | ---: |
| Observed frame rate | 120.017 fps | 102.135 fps | 14.9% fewer frames |
| Frame interval p95 / p99 | 9.700 / 10.200 ms | 10.951 / 11.116 ms | see sampling note |
| Process-tree CPU median / p95 | 10.800% / 11.517% | 15.400% / 16.396% | **42.6% / 42.4% more** |
| CPU percent per delivered fps | 0.0900 | 0.1508 | **67.6% more** |
| Summed process-tree RSS median / p95 | 1,181,392 / 1,182,906 KiB | 115,488 / 116,102 KiB | **90.2% less** |
| Process count | 4 | 1 | **75.0% fewer** |
| Settled macOS physical footprint (30 s) | 1,395,873,216 bytes | 679,920,456 bytes | **51.3% less; 2.05× smaller** |

The cap-controlled POC therefore has a large memory and process-count win, but
this is not an architecture-only attribution: Pocket omits renderer features
and texture roles listed in section 4. It also does **not** have a controlled
high-refresh CPU win. It delivered fewer frames while using more process CPU.
Pocket's larger native backing surface makes the fragment workload stricter
than exact pixel parity, but it does not turn the CPU-per-frame result into an
optimization claim.

Persona's frame receipt is one continuous 45-second CDP `requestAnimationFrame`
sample; the Pocket p95/p99 values above are the medians of nine native rolling
one-second receipts. They describe each renderer accurately but are not an
identical long-window percentile estimator. Persona had zero intervals over
20 ms across 5,401 delivered frames. Pocket's worst reported one-second maximum
was 12.592 ms.

### Production result

This lane retained the unchanged 120 Hz Persona reference and used Pocket's
shipping defaults, a 60 Hz cap and 2048 texture cap:

| Metric | Persona | Pocket | Pocket delta |
| --- | ---: | ---: | ---: |
| Observed frame rate | 120.008 fps | 54.372 fps | 54.7% fewer frames |
| Frame interval p95 / p99 | 9.600 / 10.200 ms | 19.448 / 19.639 ms | see sampling note |
| Process-tree CPU median / p95 | 10.600% / 11.638% | 8.800% / 9.000% | **17.0% / 22.7% less** |
| CPU percent per delivered fps | 0.0883 | 0.1619 | **83.3% more** |
| Summed process-tree RSS median / p95 | 1,190,720 / 1,192,163 KiB | 89,232 / 89,344 KiB | **92.5% less** |
| Process count | 4 | 1 | **75.0% fewer** |
| Settled macOS physical footprint (30 s) | 1,395,873,216 bytes | 517,260,128 bytes | **62.9% less; 2.70× smaller** |

The production configuration saves 17.0% median process CPU in absolute
terms, but only by delivering about half as many frames and downscaling source
textures. Normalized per delivered frame, it is 83.3% more CPU-expensive than
Persona in this run. The defensible performance headline is therefore:

- 51.3% less settled controlled physical footprint, or 62.9% less settled
  footprint at production texture settings;
- 90.2–92.5% less summed RSS and one process instead of four;
- 17.0% less total CPU at the default 60 Hz / 2048 configuration; and
- no CPU-throughput optimization yet—the controlled lane regresses 42.6% in
  raw CPU and 67.6% per delivered frame.

The unsigned upstream arm64 `.app` occupied 302,128 KiB and its distributable
zip was 129,730,393 bytes. The final POC release binary, guest bundle, model,
and seven declared local clip paths total 38,876,395 bytes before application
packaging; the catalog adds 2,363 bytes. All seven clip files are byte-identical
test aliases. This suggests substantial distribution-size headroom, but it is
not reported as a parity optimization because this POC intentionally omits
settings, import, tray/update plumbing, and native audio.

Startup is also left out of the percentage claim. Persona exposes bridge
health before avatar readiness, while Pocket starts its bridge after model,
animation, and guest initialization, so those timestamps do not mark the same
event.

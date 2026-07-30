import { onPersonaTick, persona } from "./sdk";

console.log(
  `pocket-persona: model=${persona.boot.model}`,
  `actions=[${persona.boot.actions.join(", ")}]`,
);

// The native core owns continuous animation, facial motion, and physics.
// This deliberately small Pocket guest is the hot-swappable personality
// seam: product-specific reactions can be added without rebuilding Rust.
let lastHeartbeat = 0;
onPersonaTick((state, events) => {
  for (const event of events) {
    console.log(`pocket-persona: ${event.type}=${event.value}`);
  }
  if (state.t - lastHeartbeat >= 60) {
    lastHeartbeat = state.t;
    console.log(
      `pocket-persona: t=${state.t.toFixed(0)}s animation=${state.animation}`,
      `level=${state.audioLevel.toFixed(2)} fps=${state.renderFps.toFixed(1)}`,
    );
  }
});

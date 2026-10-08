export interface PersonaTick {
  t: number;
  activity: "idle" | "listening" | "speaking";
  audioLevel: number;
  animation: string;
  blink: number;
  renderFps: number;
}

export interface PersonaEvent {
  type: "animationChanged" | "voiceChanged";
  value: string;
}

interface PersonaSurface {
  readonly boot: {
    readonly model: string;
    readonly actions: readonly string[];
  };
  playAnimation(name: string): void;
  setExpression(name: string, weight: number): void;
  quit(): void;
  __dispatch?: (state: PersonaTick, events: PersonaEvent[]) => void;
}

declare global {
  // Mounted by the native `pocket-mod` host before this bundle is evaluated.
  // eslint-disable-next-line no-var
  var persona: PersonaSurface;
}

export const persona = globalThis.persona;

export function onPersonaTick(
  callback: (state: PersonaTick, events: PersonaEvent[]) => void,
): void {
  globalThis.persona.__dispatch = callback;
}

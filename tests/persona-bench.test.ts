import { describe, expect, test } from "bun:test";

import {
  parseCpuTimeSeconds,
  parseReferenceCanvasReceipt,
  parseReferenceFrameSample,
  validateBenchmarkActivityHealth,
  validatePocketHealthBody,
} from "../scripts/bench-persona";

describe("Persona benchmark receipts", () => {
  test("accepts Pocket's nested health status", () => {
    expect(
      validatePocketHealthBody({
        ok: true,
        status: {
          modelConfigured: true,
          windowVisible: true,
          renderFps: 59.8,
          renderFrameCount: 123,
        },
      }),
    ).toBe(59.8);
    expect(() =>
      validatePocketHealthBody({
        modelConfigured: true,
        windowVisible: true,
        renderFps: 59.8,
        renderFrameCount: 123,
      }),
    ).toThrow("status must be an object");
    expect(() =>
      validatePocketHealthBody({
        status: {
          modelConfigured: true,
          windowVisible: true,
          renderFps: 59.8,
        },
      }),
    ).toThrow("renderFrameCount must be a positive integer");
  });

  test("requires the requested activity to reach both targets", () => {
    const speakingState = {
      phase: "active",
      activity: "speaking",
      microphoneMuted: false,
      outputMuted: false,
    };
    expect(() =>
      validateBenchmarkActivityHealth(
        "reference",
        { ok: true, lastState: speakingState },
        "speaking",
      ),
    ).not.toThrow();
    expect(() =>
      validateBenchmarkActivityHealth(
        "pocket",
        {
          ok: true,
          status: {
            voiceState: speakingState,
            activeAnimation: "speaking",
            audioLevel: 0.349999994,
          },
        },
        "speaking",
      ),
    ).not.toThrow();
    expect(() =>
      validateBenchmarkActivityHealth(
        "pocket",
        {
          status: {
            voiceState: speakingState,
            activeAnimation: "idle",
            audioLevel: 0,
          },
        },
        "speaking",
      ),
    ).toThrow("activeAnimation must be speaking");
  });

  test("requires the reference viewport, backing size, and WebGL context", () => {
    expect(
      parseReferenceCanvasReceipt({
        ready_state: "complete",
        viewport: [430, 680, 1.5],
        canvas: {
          client: [430, 680],
          backing: [645, 1020],
        },
        webgl: {
          version: "WebGL 2.0",
          renderer: "ANGLE Metal",
          vendor: "Apple",
        },
      }),
    ).toEqual({
      ready_state: "complete",
      viewport: [430, 680, 1.5],
      canvas: {
        client: [430, 680],
        backing: [645, 1020],
      },
      webgl: {
        version: "WebGL 2.0",
        renderer: "ANGLE Metal",
        vendor: "Apple",
      },
    });
    expect(() =>
      parseReferenceCanvasReceipt({
        ready_state: "complete",
        viewport: [430, 680, 1.5],
        canvas: {
          client: [430, 680],
          backing: [860, 1360],
        },
        webgl: {
          version: "WebGL 2.0",
          renderer: "ANGLE Metal",
          vendor: "Apple",
        },
      }),
    ).toThrow("unexpected canvas backing");
  });

  test("validates a full-window reference frame receipt", () => {
    const receipt = {
      requested_duration_ms: 1_000,
      elapsed_ms: 1_001,
      frames: 120,
      fps: 119.88,
      mean_interval_ms: 8.34,
      p50_interval_ms: 8.3,
      p95_interval_ms: 9.6,
      p99_interval_ms: 10.2,
      max_interval_ms: 10.4,
      over_20ms: 0,
    };
    expect(parseReferenceFrameSample(receipt, 1_000)).toEqual(receipt);
    expect(() =>
      parseReferenceFrameSample({ ...receipt, elapsed_ms: 100 }, 1_000),
    ).toThrow("failed validation");
  });

  test("parses POSIX cumulative CPU time", () => {
    expect(parseCpuTimeSeconds("02:03")).toBe(123);
    expect(parseCpuTimeSeconds("1-02:03:04")).toBe(93_784);
    expect(() => parseCpuTimeSeconds("not-a-time")).toThrow(
      "invalid ps CPU time",
    );
  });
});

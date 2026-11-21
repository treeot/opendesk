import { describe, expect, it } from "vitest";
import { attachGamepads, gamepadUnavailableText, MIN_INTERVAL_MS, neutralGamepad, readGamepad, type GamepadState } from "./gamepad";
import { encodeInputEvent } from "./input";

const pad = (pressed: number[], axes: number[] = [0, 0, 0, 0], triggers: [number, number] = [0, 0]) => ({
  connected: true,
  axes,
  buttons: Array.from({ length: 17 }, (_, i) => ({ pressed: pressed.includes(i), touched: false, value: i === 6 ? triggers[0] : i === 7 ? triggers[1] : pressed.includes(i) ? 1 : 0 })),
}) as unknown as Gamepad;

describe("readGamepad", () => {
  it("packs standard buttons, scales sticks and reads analog triggers", () => {
    expect(readGamepad(pad([0, 12, 16], [1, -1, 0.5, 2], [0.5, 1]), 2)).toEqual({
      kind: "gamepad", index: 2, connected: true, buttons: 1 | 1 << 12 | 1 << 16, axes: [32767, -32767, 16384, 32767], triggers: [128, 255],
    });
  });

  it("encodes the protocol's 25-byte gamepad event", () => {
    const state: GamepadState = { kind: "gamepad", index: 1, connected: true, buttons: 1 | 1 << 16, axes: [-32768, 32767, 0, -1], triggers: [0, 255] };
    expect(Array.from(encodeInputEvent(state, 3))).toEqual([18, 0, 0, 0, 0, 0, 0, 0, 3, 1, 1, 0, 1, 0, 1, 0x80, 0, 0x7f, 0xff, 0, 0, 0xff, 0xff, 0, 255]);
  });

  it("explains each reason the host gives for having no gamepad", () => {
    expect(gamepadUnavailableText("vigembus_missing")).toContain("ViGEmBus");
    expect(gamepadUnavailableText("unsupported_os")).toContain("Mac");
    expect(gamepadUnavailableText("later_code")).toContain("later_code");
  });
});

describe("attachGamepads", () => {
  const setup = () => {
    let pads: (Gamepad | null)[] = [];
    let active = true;
    let clock = 0;
    let frame: (() => void) | undefined;
    const sent: GamepadState[] = [];
    const detach = attachGamepads((state) => sent.push(state), {
      active: () => active,
      getGamepads: () => pads,
      now: () => clock,
      schedule: (callback) => { frame = callback; return 1; },
      cancel: () => { frame = undefined; },
    });
    const tick = (ms = MIN_INTERVAL_MS) => { clock += ms; frame?.(); };
    return { sent, detach, tick, setPads: (next: (Gamepad | null)[]) => { pads = next; }, setActive: (on: boolean) => { active = on; } };
  };

  it("sends changes only, at a bounded rate, and unplugs on detach", () => {
    const { sent, detach, tick, setPads } = setup();
    setPads([pad([0])]);
    tick();
    setPads([pad([1])]);
    tick(1);
    expect(sent.map((s) => s.buttons)).toEqual([1]);
    tick(MIN_INTERVAL_MS);
    tick();
    expect(sent.map((s) => s.buttons)).toEqual([1, 2]);
    detach();
    expect(sent[2]).toEqual(neutralGamepad(0, false));
  });

  it("centres pads while inactive and unplugs pads that go away", () => {
    const { sent, tick, setPads, setActive } = setup();
    setPads([null, pad([3], [0.5, 0, 0, 0])]);
    tick();
    setActive(false);
    tick();
    expect(sent[1]).toEqual(neutralGamepad(1));
    tick();
    expect(sent).toHaveLength(2);
    setActive(true);
    setPads([]);
    tick();
    expect(sent[2]).toEqual(neutralGamepad(1, false));
  });
});

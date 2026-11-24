import { describe, expect, it } from "vitest";
import { formatPrice, formatTps } from "@/lib/format";

describe("formatPrice", () => {
  it("shows tokens per million, and a price by the second per minute", () => {
    expect(formatPrice({ input: 0.25, output: 2 })).toBe("$0.25 / $2");
    expect(formatPrice({ input: 0, output: 0, per_second: 0.0001 })).toBe("$0.006 / min");
    expect(formatPrice({ input: 0.25, output: 2 }, " per M")).toBe("$0.25 / $2 per M");
    expect(formatPrice({ input: 0, output: 0, per_second: 0.0001 }, " per M")).toBe("$0.006 / min");
    expect(formatPrice(null)).toBe("—");
  });
});

describe("formatTps", () => {
  it("rounds a hundred to whole tokens, keeps one decimal below, and dashes without a sample", () => {
    expect(formatTps(123.4)).toBe("123 tok/s");
    expect(formatTps(84.6)).toBe("84.6 tok/s");
    expect(formatTps(0)).toBe("0 tok/s");
    expect(formatTps(null)).toBe("—");
    expect(formatTps(undefined)).toBe("—");
  });
});

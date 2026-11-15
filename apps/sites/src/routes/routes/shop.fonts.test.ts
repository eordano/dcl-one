import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { describe, expect, it, vi } from "vitest";

vi.mock("../shop/host", () => ({ default: () => null }));

const UI3_PUBLIC = fileURLToPath(new URL("../../../../ui3/public", import.meta.url));
const INTER_CSS = fileURLToPath(new URL("../../../../ui3/src/atoms/inter.css", import.meta.url));

describe("the Shop's Inter", () => {
  it("preloads a font file that ships, and the one the Shop's Latin text is set in", async () => {
    const { links } = await import("./shop");
    const hrefs = links().map((l) => l.href);
    expect(hrefs).toEqual(["/fonts/Inter-v20-latin.woff2"]);
    for (const href of hrefs) expect(existsSync(`${UI3_PUBLIC}${href}`)).toBe(true);
  });

  it("declares every face it names from a file that ships, once per weight as Google Fonts does", () => {
    const css = readFileSync(INTER_CSS, "utf8");
    const faces = [...css.matchAll(/@font-face \{ font-family: "Inter"; src: url\("([^"]+)"\)[^}]*font-weight: ([^;]+);/g)];
    expect(faces.length).toBe(21);
    for (const [, src] of faces) expect(existsSync(`${UI3_PUBLIC}${src}`)).toBe(true);
    expect(new Set(faces.map(([, , weight]) => weight))).toEqual(new Set(["400", "600", "700"]));
  });
});

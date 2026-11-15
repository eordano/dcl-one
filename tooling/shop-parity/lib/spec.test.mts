import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { PAGES } from "../pages.mts";
import { interOnReference } from "../pages/sites/frame.mts";
import { interFontFaces } from "./fonts.mts";
import { pagePath, pathLabel, referenceOf, signedIn, type PageSpec } from "./spec.mts";

const base: PageSpec = { id: "p", title: "p", path: "/x", ready: { reference: "h1", "dcl-one": "h1" }, keys: [{ name: "site-nav.signin", selector: "a" }, { name: "footer", selector: "footer" }] };
const byId = (id: string) => PAGES.find((p) => p.id === id)!;

describe("pagePath", () => {
  it("opens the same path on both sides unless the page names one per side", () => {
    assert.equal(pagePath({ path: "/shop/cart" }, {}, "reference"), "/shop/cart");
    assert.equal(pagePath({ path: "/shop/cart" }, {}, "dcl-one"), "/shop/cart");
    const events = { path: { reference: "/events", "dcl-one": "/whats-on" } };
    assert.equal(pagePath(events, {}, "reference"), "/events");
    assert.equal(pagePath(events, {}, "dcl-one"), "/whats-on");
    assert.equal(pagePath(events, {}), "/events");
  });
  it("lets a function read the recording's targets and answer one path or one per side", () => {
    assert.equal(pagePath({ path: (t) => t.item ?? "/shop/items" }, { item: "/shop/item/0x1/2" }, "dcl-one"), "/shop/item/0x1/2");
    const place = { path: (t: Record<string, string>) => ({ reference: `/places/place/${t.pos}`, "dcl-one": `/places/${t.id}` }) };
    assert.equal(pagePath(place, { pos: "1,2", id: "abc" }, "reference"), "/places/place/1,2");
    assert.equal(pagePath(place, { pos: "1,2", id: "abc" }, "dcl-one"), "/places/abc");
    assert.equal(pathLabel(place, { pos: "1,2", id: "abc" }), "/places/place/1,2 → /places/abc");
    assert.equal(pathLabel({ path: "/" }, {}), "/");
  });
});

describe("signedIn", () => {
  it("keeps the page but its sign-in key and its discovery", () => {
    const s = signedIn({ ...base, discover: async () => ({}) }, "signed");
    assert.equal(s.id, "p-signed-in");
    assert.equal(s.signedIn, true);
    assert.equal(s.discover, undefined);
    assert.deepEqual(s.keys.map((k) => k.name), ["footer"]);
    assert.equal(s.path, base.path);
    assert.equal(signedIn({ ...base, init: interOnReference }, "signed").init, interOnReference);
  });
});

describe("interOnReference", () => {
  it("installs inter.css's faces, under the mount, in the reference's context only", async () => {
    const installed: unknown[] = [];
    const context = { addInitScript: async (_fn: unknown, arg: unknown) => void installed.push(arg) } as unknown as Parameters<typeof interOnReference>[0];
    await interOnReference(context, "dcl-one");
    assert.deepEqual(installed, []);
    await interOnReference(context, "reference");
    assert.equal(installed.length, 1);
    const css = installed[0] as string;
    assert.equal(css, interFontFaces(), "the faces of inter.css, as lib/harness.test.mts checks them");
  });
});

describe("PAGES", () => {
  it("holds every page once, the shop pages on the shop reference and the sites pages on the sites one", () => {
    const ids = PAGES.map((p) => p.id);
    assert.equal(new Set(ids).size, ids.length);
    for (const id of ["landing", "events", "create", "places", "place-genesis", "place-live"]) {
      const out = byId(id);
      const on = byId(`${id}-signed-in`);
      assert.ok(out && on, id);
      assert.equal(referenceOf(out), "sites");
      assert.equal(referenceOf(on), "sites");
      assert.equal(on.signedIn, true);
      assert.ok(out.keys.some((k) => k.name === "site-nav.signin") && !on.keys.some((k) => k.name === "site-nav.signin"), id);
    }
    for (const p of PAGES) assert.equal(p.init, referenceOf(p) === "sites" ? interOnReference : undefined, `${p.id}: the frame's font rule on every sites page, none on a shop page`);
    assert.equal(referenceOf(byId("overview")), "shop");
    assert.equal(pagePath(byId("events"), {}, "dcl-one"), "/whats-on");
    assert.equal(pagePath(byId("events"), {}, "reference"), "/events");
    assert.equal(pagePath(byId("place-live"), { livePlace: "/places/place/5,7" }, "dcl-one"), "/places/place/5,7");
    assert.throws(() => pagePath(byId("place-live"), {}), /livePlace/);
  });

  it("opens place-world at the world the /places grid yields, discovered before the page opens", () => {
    const out = byId("place-world");
    const on = byId("place-world-signed-in");
    assert.equal(referenceOf(out), "sites");
    assert.equal(out.discoverAt, "/places");
    assert.ok(out.discover && !on.discover && !on.discoverAt && on.signedIn);
    assert.equal(pagePath(out, { worldPlace: "/places/world/a.dcl.eth" }, "dcl-one"), "/places/world/a.dcl.eth");
    assert.throws(() => pagePath(on, {}), /worldPlace/);
  });
});

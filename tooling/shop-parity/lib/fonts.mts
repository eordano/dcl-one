// Inter on the sites reference. decentraland.org's stylesheet names `Inter, Helvetica, Arial, sans-serif`
// but ships no @font-face for Inter (its index.html only preconnects to Google Fonts), so it renders the
// fallback face; dcl-one declares Inter v20 on every page (the shared foundations' typography entry,
// apps/ui3/src/atoms/inter.css). The decision
// is to keep Inter, so the reference gets the same faces from the same files: the rules of inter.css,
// their urls moved under the reference static server's mount, installed before navigation. Both sides
// then await document.fonts.ready before anything is measured (capture.mts `settle`).
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { BrowserContext } from "playwright-core";
import { UI3, type ReferenceId } from "./paths.mts";

export const FONTS_DIR = join(UI3, "public", "fonts");
export const INTER_CSS = join(UI3, "src", "atoms", "inter.css");
/** The URL prefix of inter.css's files, and where the reference static server serves the same files. */
export const FONTS_URL = "/fonts/";
export const FONTS_MOUNT = "/__parity/fonts/";

/** The directories a reference's static server serves beside its build, by URL prefix. */
export const REFERENCE_MOUNTS: Partial<Record<ReferenceId, Record<string, string>>> = { sites: { [FONTS_MOUNT]: FONTS_DIR } };

/** inter.css's @font-face rules (families, weights, unicode ranges, display as declared), each url under the mount. */
export function interFontFaces(css = readFileSync(INTER_CSS, "utf8")): string {
  const rules = css.match(/@font-face\s*\{[^}]*\}/g) ?? [];
  if (!rules.length) throw new Error(`${INTER_CSS} declares no @font-face rule`);
  return rules.map((rule) => rule.replaceAll(`url("${FONTS_URL}`, `url("${FONTS_MOUNT}`)).join("\n");
}

/**
 * Installs the faces in `context` before any of its pages loads, as `signIn` installs a session: a
 * <style> first in the document's head, added the moment the document has an element.
 */
export async function installInter(context: BrowserContext, css = interFontFaces()): Promise<void> {
  await context.addInitScript((css: string) => {
    const add = () => {
      const style = document.createElement("style");
      style.setAttribute("data-parity-fonts", "");
      style.textContent = css;
      (document.head ?? document.documentElement).prepend(style);
    };
    if (document.documentElement) add();
    else new MutationObserver((_, o) => { if (document.documentElement) { o.disconnect(); add(); } }).observe(document, { childList: true });
  }, css);
}

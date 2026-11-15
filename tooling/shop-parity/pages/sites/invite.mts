// The invite landing at /invite/<referrer>, for the test wallet as the referrer: two InviteHero sections
// (the inviter's name, the envelope and the avatar preview, then "Hang out in Decentraland" over its clip),
// the FAQ accordion and the LandingFooter. The route is Layout-less on the reference (sites:
// pages/invite/InvitePage.tsx, App.tsx), so it has no site nav on either side; dcl-one renders StInvite
// (apps/ui3/src/web/pages/StInvite.tsx) in SitesChrome without its nav.
import type { Page } from "playwright-core";
import { both, signedIn, type KeyElement, type PageSpec } from "../../lib/spec.mts";
import { interOnReference, NEWSLETTER_EXPECTED, NEWSLETTER_REGION, SITES_UNRECORDED } from "./frame.mts";
import { TEST_WALLET } from "./profile.mts";

const keys: KeyElement[] = [
  { name: "hero.title", selector: "h1" },
  { name: "hero.subtitle", selector: { reference: "xpath=(//h1/following-sibling::*[1])[1]", "dcl-one": "h1 + p" } },
  { name: "second.title", selector: "xpath=//section//h2[contains(normalize-space(),'Hang out in')]" },
  { name: "faqs.title", selector: "xpath=//section[@aria-label='Frequently Asked Questions']//h2" },
  { name: "faqs.item", selector: "xpath=//section[@aria-label='Frequently Asked Questions']//*[@role='button']", max: 7 },
  { name: "footer", selector: "footer" },
];

/**
 * Visible once the inviter's profile has resolved and the lazy FAQ section is mounted. Until the profile
 * resolves the h1 holds the title alone ("invited you to join Decentraland"); then the inviter's name leads it.
 */
const READY = "xpath=//h1[contains(normalize-space(),' invited you to join')][//section[@aria-label='Frequently Asked Questions']//h2]";

/**
 * Both heroes draw a WebGL <canvas> animated over a static picture (sites: decentraland-ui2 AnimatedBackground;
 * dcl-one: shop/discovery/components/animated-background): both sides hide the canvas, so the static fallback
 * and the text over it are compared. The second hero plays the same looping clip on both sides (sites:
 * data/inviteContent.ts through cms-videos.decentraland.org; dcl-one: assets/invite/referral.webm, the same
 * bytes): both sides hold it paused at 0 s once it can show its first frame. A clip that shows no first frame or
 * ends no seek within 10 s is left as it is, and the run says so.
 */
async function holdMotion(page: Page): Promise<void> {
  await page.addStyleTag({ content: "section canvas { visibility: hidden !important; }" });
  const held = await page.evaluate(
    () =>
      new Promise<boolean>((resolve) => {
        const v = document.querySelector<HTMLVideoElement>("section video");
        if (!v) return resolve(true);
        const hold = () => {
          v.pause();
          if (v.currentTime === 0) return resolve(true);
          v.addEventListener("seeked", () => resolve(true), { once: true });
          v.currentTime = 0;
        };
        if (v.readyState >= 2) hold();
        else v.addEventListener("loadeddata", hold, { once: true });
        setTimeout(() => resolve(false), 10_000);
      }),
  );
  if (!held) console.warn(`invite: the second hero's clip is not held at 0 s (${page.url()})`);
}

const PREVIEW = "the inviter's 3D avatar: decentraland.org's wearable-preview iframe, which the harness answers with a blank page, and dcl-one's own renderer (wearable-preview/WearablePreview.tsx)";

const invite: PageSpec = {
  id: "invite",
  title: "Invite landing",
  reference: "sites",
  path: `/invite/${TEST_WALLET}`,
  ready: both(READY),
  init: interOnReference,
  setup: holdMotion,
  keys,
  masks: [{ selector: { reference: ["section div:has(> iframe)"], "dcl-one": [".stinvite__avatar-wrap:has(> *)"] }, why: PREVIEW }],
  regions: [NEWSLETTER_REGION],
  expected: NEWSLETTER_EXPECTED,
  unrecorded: SITES_UNRECORDED,
};

export default [invite, signedIn(invite, "Invite landing, signed in as the test wallet")] satisfies PageSpec[];

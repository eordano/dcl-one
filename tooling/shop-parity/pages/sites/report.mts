// The player report: the reference's /report (ReportForm) and /report/success (ReportSuccess), the same paths on
// dcl-one (features/social/report). Both cards sit on the AnimatedBackground inside the site's Layout.
import type { Page } from "playwright-core";

import { signedIn, type KeyElement, type PageSpec } from "../../lib/spec.mts";
import { hideCanvas, mainText, SITES_UNRECORDED, sitesFrame, sitesFrameRules, UP } from "./frame.mts";

const field = (tag: "input" | "textarea", placeholder: string) => `xpath=//main//${tag}[@placeholder='${placeholder}']`;
const button = (label: string) => `xpath=//main//button[${UP}='${label}']`;
/** The card: the parent of the box that holds the logo and the title (sites: ReportForm.styled.ts FormCard, LogoWrapper). */
const card = (title: string) => `xpath=//main//h3[normalize-space()='${title}']/../..`;
const logo = (title: string) => `xpath=//main//h3[normalize-space()='${title}']/preceding-sibling::*[1]`;

const formKeys: KeyElement[] = [
  ...sitesFrame("dtb"),
  { name: "card", selector: card("Report User") },
  { name: "logo", selector: logo("Report User") },
  { name: "title", selector: mainText("Report User") },
  { name: "wallet", selector: field("input", "Write or paste your address here...") },
  { name: "signin-alert", selector: "xpath=//main//div[starts-with(normalize-space(),'Please sign in with your wallet') and not(.//div[starts-with(normalize-space(),'Please sign in with your wallet')])]" },
  { name: "signin-alert.cta", selector: button("SIGN IN") },
  { name: "reported", selector: field("input", "Write or paste an address here...") },
  { name: "reason", selector: "xpath=//main//*[@role='combobox']" },
  { name: "description", selector: field("textarea", "Write your description here...") },
  { name: "evidence.add", selector: button("ADD FILE") },
  { name: "comments", selector: field("textarea", "Write your comments here...") },
  { name: "confirm", selector: mainText("I confirm this report is accurate.") },
  { name: "submit", selector: button("SUBMIT REPORT") },
  { name: "help", selector: "xpath=//main//p[starts-with(normalize-space(),'Need help instead?')]" },
  { name: "help.link", selector: "xpath=//main//a[normalize-space()='contact the Support team']" },
];

/**
 * The AnimatedBackground behind both cards is a WebGL <canvas> animated over a static picture (sites: decentraland-ui2
 * AnimatedBackground; dcl-one: shop/discovery/components/animated-background), as on the landing page: both sides hide
 * the canvas, so the static picture and the card over it are compared.
 */
async function holdBackground(page: Page): Promise<void> {
  await hideCanvas(page);
}

const frame = sitesFrameRules();

const report: PageSpec = {
  id: "report",
  title: "Report a player",
  reference: "sites",
  path: "/report",
  ready: mainText("Report User"),
  setup: holdBackground,
  keys: formKeys,
  ...frame,
  unrecorded: SITES_UNRECORDED,
};

/**
 * /report/success shows its card only to the page a submission navigated to: both sides read `{ submitted: true }` from
 * the history entry's state and send any other visit to /report (sites: ReportSuccess.tsx; dcl-one:
 * features/social/report/ReportSuccess.tsx). The test wallet never submits (it declines every signature), so the page
 * opens the form and makes the submission's client navigation itself: the entry react-router pushes (state in `usr`)
 * and the popstate its router follows, on both sides.
 */
async function submitted(page: Page): Promise<void> {
  await holdBackground(page);
  await page.locator(mainText("Report User").reference).first().waitFor();
  await page.evaluate(() => {
    history.pushState({ usr: { submitted: true }, key: "parity", idx: (history.state?.idx ?? 0) + 1 }, "", "/report/success");
    dispatchEvent(new PopStateEvent("popstate", { state: history.state }));
  });
}

const successKeys: KeyElement[] = [
  ...sitesFrame("dtb"),
  { name: "card", selector: card("Report Submitted") },
  { name: "logo", selector: logo("Report Submitted") },
  { name: "title", selector: mainText("Report Submitted") },
  { name: "body", selector: mainText("Your report has been received and will be reviewed shortly.") },
  { name: "dismiss", selector: mainText("You can close this window now.") },
];

const success: PageSpec = {
  id: "report-success",
  title: "Report submitted",
  reference: "sites",
  path: "/report",
  ready: mainText("Report Submitted"),
  setup: submitted,
  keys: successKeys,
  ...frame,
  unrecorded: SITES_UNRECORDED,
};

const dropKeys = (spec: PageSpec, name: RegExp): PageSpec => ({ ...spec, keys: spec.keys.filter((k) => !name.test(k.name)) });

export default [
  report,
  // A signed-in visitor gets no sign-in alert (sites: ReportForm.tsx renders SignInAlert only without an identity).
  dropKeys(signedIn(report, "Report a player, signed in as the test wallet"), /^signin-alert/),
  success,
  signedIn(success, "Report submitted, signed in as the test wallet"),
] satisfies PageSpec[];

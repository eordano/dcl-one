// /profile/:address: the test wallet's public profile, signed out (a member's view) and signed in (the
// owner's view), and /profile/me, which sends a signed-in visitor to their own profile.
import { both, signedIn, type PageSpec } from "../../lib/spec.mts";
import { SITES_UNRECORDED, sitesFrame, sitesFrameRules } from "./frame.mts";

/** The test wallet: a signed-in run holds its key in SHOP_PARITY_WALLET_KEY. */
export const TEST_WALLET = "0x4e9C4a2502FdF71e93Ed8ed6ca9Ddbd891d6f295";
/** Lowercased, as both sides route a profile. */
export const PROFILE_ADDRESS = TEST_WALLET.toLowerCase();
/** The short address both headers show (sites: utils/address.ts truncateAddress). */
const SHORT = `${PROFILE_ADDRESS.slice(0, 6)}…${PROFILE_ADDRESS.slice(-4)}`;
const SHORT_TEXT = `xpath=//main//*[normalize-space()='${SHORT}' and not(.//*[normalize-space()='${SHORT}'])]`;
/** The owner's view has rendered: its header (desktop) or its section list (phone) offers sharing. */
const OWNER_TEXT = "xpath=//main//button[normalize-space()='Invite friends' or normalize-space()='Share Profile']";

const AVATAR_3D =
  "the reference's aside is decentraland.org's third-party wearable preview (an iframe on wearable-preview.decentraland.org, with a spinner until it loads); dcl-one shows the profile's full-body snapshot from the node's image cache in the same 500x706 box";

/** dcl-one's bar is a pseudo-element, so the selected tab (the same box on both sides) is what is masked. */
const TAB_INDICATOR =
  "the reference's MuiTabs indicator keeps the width MUI measured before the Inter webfont loaded (85.95px for the 85.25px tab), one column wider than the tab; dcl-one draws the bar at the tab's width";

const frame = sitesFrameRules();

const profile: PageSpec = {
  id: "profile",
  title: "Profile: the test wallet, a member's view",
  reference: "sites",
  path: `/profile/${PROFILE_ADDRESS}`,
  ready: both(SHORT_TEXT),
  keys: [...sitesFrame("dtb"), { name: "address", selector: SHORT_TEXT }],
  ...frame,
  masks: [{ selector: { reference: ['main [role="tab"][aria-selected="true"]'], "dcl-one": ['main nav[aria-label="Profile sections"] a[aria-current="page"]'] }, why: TAB_INDICATOR }],
  regions: [...frame.regions, { selector: { reference: ["xpath=//main//iframe/.."], "dcl-one": ['main [class*="_render_"]'] }, why: AVATAR_3D }],
  unrecorded: [...SITES_UNRECORDED, { match: /^GET wearable-preview\.decentraland\.org\//, why: AVATAR_3D }],
};

const owner = (spec: PageSpec, title: string): PageSpec => ({
  ...signedIn(spec, title),
  ready: both(OWNER_TEXT),
});

const profileMe: PageSpec = {
  ...owner(profile, "Profile: /profile/me, signed in as the test wallet"),
  id: "profile-me-signed-in",
  path: "/profile/me",
};

export default [profile, owner(profile, "Profile: the test wallet, the owner's view"), profileMe] satisfies PageSpec[];

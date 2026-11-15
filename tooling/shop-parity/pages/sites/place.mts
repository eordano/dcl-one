// What the two place pages share: the scene header's selectors on each side, and the name of the
// target the places page discovers for place-live.
import type { KeyElement, PageSpec, PerSide } from "../../lib/spec.mts";
import { SITES_UNRECORDED, sitesFrame } from "./frame.mts";

/** The recording's target holding the live scene's path (/places/place/<x,y>). */
export const LIVE_PLACE = "livePlace";

/**
 * The reference's scene header: the title is the paragraph followed by the creator line ("By <name>"),
 * and the location tag (the pin with the parcel) sits in that line's box. On desktop the tag is a link
 * labelled "Jump into <parcel>", on mobile a plain box, so both are found by their place in the header.
 * dcl-one's PlaceDetail ports that header with the title as an h1, so the same paths find both.
 */
const TITLE = "//main//p[following-sibling::*[1][.//p[starts-with(normalize-space(),'By ')]]]";
const DCL_ONE_TITLE = "//main//h1[following-sibling::*[1][.//p[starts-with(normalize-space(),'By ')]]]";
export const locationTag = (title: string) =>
  `xpath=(${title})[1]/following-sibling::*[1]//*[./*[local-name()='svg'] and translate(normalize-space(),'-0123456789','')=',']`;

export const REFERENCE_PLACE_TITLE = `xpath=${TITLE}`;
export const REFERENCE_PLACE_LOCATION = locationTag(TITLE);

export const PLACE_READY: PerSide<string> = { reference: REFERENCE_PLACE_TITLE, "dcl-one": `xpath=${DCL_ONE_TITLE}` };

export const PLACE_KEYS: KeyElement[] = [
  ...sitesFrame("dtb"),
  { name: "title", selector: PLACE_READY },
  { name: "location", selector: { reference: REFERENCE_PLACE_LOCATION, "dcl-one": locationTag(DCL_ONE_TITLE) } },
];

export const PLACE_UNRECORDED: NonNullable<PageSpec["unrecorded"]> = [
  ...SITES_UNRECORDED,
  {
    match: /^POST gatekeeper \/get-scene-adapter /,
    why: "the reference's scene chat asks comms-gatekeeper for a scene room with a signed POST, a write the recording never sends (README, Signed-in pages)",
  },
  {
    match: /^POST gatekeeper \/cast\/watcher-token /,
    why: "without a scene room the chat falls back to a cast watcher token whose body carries a name drawn at random, so its hash never meets the recorded key; the recorded answer is 400 and the node's 501, and either side shows the unavailable state",
  },
];

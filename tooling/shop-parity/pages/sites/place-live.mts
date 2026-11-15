// A live scene's page: the first Live Now card of the reference /places that is not Genesis Plaza, found while
// recording (places.mts), at /places/place/<x,y> on both sides (dcl-one redirects it to /places/<id>).
import { signedIn, type PageSpec } from "../../lib/spec.mts";
import { mainText, sitesFrameRules, UP } from "./frame.mts";
import { LIVE_PLACE, PLACE_KEYS, PLACE_READY, PLACE_UNRECORDED } from "./place.mts";

const CREATOR = "xpath=//main//p[starts-with(normalize-space(),'By ')]";
const DESCRIPTION = `xpath=//main//p[preceding-sibling::p[${UP}='WHAT TO EXPECT']]`;

const placeLive: PageSpec = {
  id: "place-live",
  title: "Place detail: the first Live Now scene that is not Genesis Plaza",
  reference: "sites",
  path: (t) => {
    if (!t[LIVE_PLACE]) throw new Error(`the recording has no ${LIVE_PLACE} target (record a new set: the places page's discovery reads it)`);
    return t[LIVE_PLACE];
  },
  ready: PLACE_READY,
  keys: [
    ...PLACE_KEYS,
    // The "By <creator>" line. JUMP IN is a button on the reference and a link to the explorer on dcl-one.
    { name: "creator", selector: CREATOR },
    { name: "jump-in", selector: { reference: `xpath=//main//button[${UP}='JUMP IN']`, "dcl-one": `xpath=//main//*[self::a or self::button][${UP}='JUMP IN']` } },
    // Only the desktop live view has "Explore the scene" and the chat panel: on a phone both sides show the Jump in card
    // instead (reference DiscoverScenePage isMobile, dcl-one PlaceDetail `mobile`), so neither renders them there.
    { name: "explore-scene", selector: mainText("Explore the scene"), absentOn: "mobile" },
    { name: "chat", selector: mainText("In-World Chat"), absentOn: "mobile" },
    { name: "what-to-expect", selector: mainText("What to expect") },
    { name: "description", selector: DESCRIPTION },
  ],
  ...sitesFrameRules(),
  unrecorded: PLACE_UNRECORDED,
};

export default [placeLive, signedIn(placeLive, "A live scene, signed in as the test wallet")] satisfies PageSpec[];

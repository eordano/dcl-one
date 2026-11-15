// Genesis Plaza's page: /places/place/0,0 on both sides (dcl-one redirects it to /places/<id>).
// On a desktop browser with people in the scene both sides show the live view (title bar, Explore the
// scene / Jump in, In-World Chat, What to expect); on a phone both show the Jump in card. dcl-one
// renders the page in SitesChrome (header.dtb) with PlaceDetail, the title as an h1.
import { signedIn, type KeyElement, type PageSpec } from "../../lib/spec.mts";
import { control, sitesFrame, sitesFrameRules, UP } from "./frame.mts";
import { locationTag, PLACE_UNRECORDED, REFERENCE_PLACE_LOCATION, REFERENCE_PLACE_TITLE } from "./place.mts";

const TITLE = { reference: REFERENCE_PLACE_TITLE, "dcl-one": "xpath=//main//h1" };

const keys: KeyElement[] = [
  ...sitesFrame("dtb"),
  { name: "title", selector: TITLE },
  { name: "location", selector: { reference: REFERENCE_PLACE_LOCATION, "dcl-one": locationTag("//main//h1") } },
  { name: "creator", selector: "xpath=//main//p[starts-with(normalize-space(),'By ')]" },
  // Explore the scene and the chat panel belong to the desktop live view: on a phone both sides show the Jump in card
  // instead (reference DiscoverScenePage isMobile, dcl-one PlaceDetail `mobile`), so neither renders them there.
  { name: "cta.explore", selector: control("EXPLORE THE SCENE"), absentOn: "mobile" },
  { name: "cta.jump-in", selector: control("JUMP IN") },
  { name: "chat", selector: `xpath=//main//*[self::h2 or self::h6][${UP}='IN-WORLD CHAT']`, absentOn: "mobile" },
  { name: "description", selector: "xpath=//main//*[normalize-space()='What to expect']/following-sibling::*[1]" },
];

const placeGenesis: PageSpec = {
  id: "place-genesis",
  title: "Place detail: Genesis Plaza (0,0)",
  reference: "sites",
  path: "/places/place/0,0",
  ready: TITLE,
  keys,
  ...sitesFrameRules(),
  unrecorded: PLACE_UNRECORDED,
};

export default [placeGenesis, signedIn(placeGenesis, "Genesis Plaza, signed in as the test wallet")] satisfies PageSpec[];

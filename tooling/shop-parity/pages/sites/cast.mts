// /cast: decentraland.org's Cast index is its not-found page (sites: App.tsx, CastNotFoundPage, components/cast/NotFound),
// inside the site Layout. A streamer page needs a token the gatekeeper mints for a scene admin (a signed write the
// recording never sends) and a watcher page a live LiveKit room, so the recording allows the not-found state only.
import { both, signedIn, type PageSpec } from "../../lib/spec.mts";
import { mainText, SITES_UNRECORDED, sitesFrame, sitesFrameRules, UP } from "./frame.mts";


const cast: PageSpec = {
  id: "cast",
  title: "Cast: the not-found page at /cast",
  reference: "sites",
  path: "/cast",
  ready: both("main h1"),
  keys: [
    ...sitesFrame("dtb"),
    { name: "title", selector: mainText("404 - Page Not Found") },
    { name: "description", selector: "xpath=//main//h1/following-sibling::p[1]" },
    // Go Home is a button that navigates on the reference and a link on dcl-one.
    { name: "go-home", selector: { reference: `xpath=//main//button[${UP}='GO HOME']`, "dcl-one": `xpath=//main//a[${UP}='GO HOME']` } },
    { name: "view-docs", selector: "xpath=//main//a[normalize-space()='View Documentation']" },
  ],
  ...sitesFrameRules(),
  unrecorded: SITES_UNRECORDED,
};

export default [cast, signedIn(cast, "Cast: the not-found page at /cast, signed in as the test wallet")] satisfies PageSpec[];

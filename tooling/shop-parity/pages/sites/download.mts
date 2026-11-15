// The download pages: the Explorer's /download (DownloadLayout) and /download_success (DownloadSuccess), the Creator
// Hub's /download/creator-hub and /download/creator-hub-success (sites: App.tsx, pages/download*, pages/DownloadSuccess).
import { both, signedIn, type KeyElement, type PageSpec } from "../../lib/spec.mts";
import { READY_TIMEOUT_MS } from "../../lib/spec.mts";
import { mainText, SITES_UNRECORDED, sitesFrameRules } from "./frame.mts";

// DownloadStepsLayout renders no <main> landmark, so its title is found by its words alone: the reference's h3, dcl-one's
// page heading (library/ui apps/sites/download/DownloadSuccess.tsx).
const STARTED = "xpath=//*[self::h1 or self::h3][normalize-space()='Your download has started!']";
// The title's apostrophe rules out mainText's single-quoted literal.
const ALMOST_DONE = `xpath=//main//h3[normalize-space()="You're almost done!"]`;
// The backdrop shows while the installer starts and closes once it has (DownloadSuccess's showBackdrop): the page is
// compared after that.
const BACKDROP = "text=Downloading Decentraland...";
const H = (tag: string, text: string) => `xpath=//main//${tag}[starts-with(normalize-space(),'${text}')]`;
const TITLE = H("h2", "Download Decentraland");
const CREATOR_HUB = mainText("Decentraland Creator Hub");

const frame = sitesFrameRules();

/** A download page: its title and the footer, in the frame every sites page shares. */
function downloadPage(id: string, title: string, path: string, ready: PageSpec["ready"], heading: KeyElement["selector"], more: Partial<PageSpec> = {}): PageSpec {
  return {
    id,
    title,
    reference: "sites",
    path,
    ready,
    keys: [
      { name: "title", selector: heading },
      { name: "footer", selector: "footer" },
    ],
    ...frame,
    unrecorded: SITES_UNRECORDED,
    ...more,
  };
}

// dcl-one renders the desktop and the phone title and CSS shows one: the ready title is the visible one.
const download = downloadPage("download", "Download Decentraland (the Explorer)", "/download", { reference: TITLE, "dcl-one": `${TITLE} >> visible=true` }, TITLE);

const downloadSuccess = downloadPage("download-success", "Download Decentraland: the download has started", "/download_success", both(STARTED), STARTED, {
  setup: async (page) => {
    await page.locator(BACKDROP).first().waitFor({ state: "hidden", timeout: READY_TIMEOUT_MS });
  },
  unrecorded: [
    ...SITES_UNRECORDED,
    { match: /^POST auth \/identities /, why: "the reference posts a new ephemeral identity to auth-api each session (sites: DownloadSuccess.tsx useGetIdentityId), so its body never repeats and cannot be recorded; dcl-one makes no such call" },
  ],
});

const creatorHub = downloadPage("download-creator-hub", "Download the Creator Hub", "/download/creator-hub", CREATOR_HUB, CREATOR_HUB);

const creatorHubSuccess = downloadPage("download-creator-hub-success", "Download the Creator Hub: almost done", "/download/creator-hub-success", both(ALMOST_DONE), ALMOST_DONE);

export default [
  download, signedIn(download, "Download Decentraland, signed in as the test wallet"),
  downloadSuccess, signedIn(downloadSuccess, "Download Decentraland: started, signed in as the test wallet"),
  creatorHub, signedIn(creatorHub, "Download the Creator Hub, signed in as the test wallet"),
  creatorHubSuccess, signedIn(creatorHubSuccess, "Download the Creator Hub: almost done, signed in as the test wallet"),
] satisfies PageSpec[];

// decentraland.org's pages, one file per page id, each a signed-out and a signed-in spec.
import type { PageSpec } from "../../lib/spec.mts";
import cast from "./cast.mts";
import create from "./create.mts";
import discover from "./discover.mts";
import download from "./download.mts";
import eventDetail from "./event-detail.mts";
import events from "./events.mts";
import invite from "./invite.mts";
import jump from "./jump.mts";
import landing from "./landing.mts";
import notFound from "./not-found.mts";
import placeGenesis from "./place-genesis.mts";
import placeLive from "./place-live.mts";
import placeWorld from "./place-world.mts";
import places from "./places.mts";
import profile from "./profile.mts";
import report from "./report.mts";

export const SITES_PAGES: PageSpec[] = [
  ...landing,
  ...events,
  ...create,
  ...places,
  ...placeGenesis,
  ...placeLive,
  ...download,
  ...profile,
  ...discover,
  ...cast,
  ...report,
  ...invite,
  ...placeWorld,
  ...eventDetail,
  ...jump,
  ...notFound,
];

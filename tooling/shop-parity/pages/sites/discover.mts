// /discover: decentraland.org renamed Discover to Places, so /discover lands on /places with its subpath and
// query (sites: src/pages/RenamedSectionRedirect.tsx, a client <Navigate replace>); dcl-one answers it with a
// redirect (apps/sites routes/discover.$.tsx). Both then render the places page, compared with the places keys.
// The ready selector is the Explore toolbar's first tab, which only /places renders: a redirect elsewhere fails it.
import { signedIn, type PageSpec } from "../../lib/spec.mts";
import places from "./places.mts";

const { discover: _discover, ...placesPage } = places.find((p) => p.id === "places")!;

const discover: PageSpec = {
  ...placesPage,
  id: "discover",
  title: "Discover (redirects to Places)",
  path: "/discover",
};

export default [discover, signedIn(discover, "Discover, signed in as the test wallet")] satisfies PageSpec[];

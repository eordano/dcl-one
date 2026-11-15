// @vitest-environment jsdom
import {
  dehydrate,
  focusManager,
  QueryClient,
  QueryClientProvider,
  QueryObserver,
} from "@tanstack/react-query";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { createMemoryRouter, RouterProvider, useNavigate } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.hoisted(() => {
  Object.assign(window, {
    __vite_plugin_react_preamble_installed__: true,
    $RefreshSig$: () => (type: unknown) => type,
    $RefreshReg$: () => {},
  });
});
const net = vi.hoisted(() => ({ fetchProfile: vi.fn(), fetchMyCommunities: vi.fn(), track: vi.fn() }));
const auth = vi.hoisted(() => ({
  state: { isConnected: false, address: null as string | null, identity: null as unknown },
}));
vi.mock("@/data/lib/catalyst/social/profile", async (original) => ({
  ...(await original<typeof import("@/data/lib/catalyst/social/profile")>()),
  fetchProfile: net.fetchProfile,
}));
vi.mock("@/data/lib/catalyst/social/profile-communities", () => ({ fetchMyCommunities: net.fetchMyCommunities }));
vi.mock("@/core/lib/telemetry/track", () => ({ track: net.track }));
vi.mock("@/data/lib/auth/index", () => ({ useAuth: () => auth.state }));

// The tab components are today's ui3 pages; these stand-ins show what the route hands them.
const { stub } = vi.hoisted(() => ({
  stub: (tab: string) => ({
    default: (props: { profile: { name: string }; isOwnProfile?: boolean }) => (
      <section data-tab={tab}>
        <h2>{props.profile.name}</h2>
        <p>{props.isOwnProfile ? "own" : "member"}</p>
      </section>
    ),
  }),
}));
vi.mock("@ui/web/pages/StProfileOverviewTab", () => stub("overview"));
vi.mock("@ui/web/pages/StProfileCreationsTab", () => stub("creations"));
vi.mock("@ui/web/pages/StProfilePlacesTab", () => stub("places"));
vi.mock("@ui/web/pages/StProfilePhotosTab", () => stub("photos"));
vi.mock("@ui/web/pages/StProfileCommunitiesTab", () => stub("communities"));
vi.mock("@ui/web/pages/StProfileMyAssetsTab", () => stub("assets"));

import { CatalystError } from "@/data/lib/catalyst/client";
import { mapProfile } from "@/data/lib/catalyst/social/profile";
import { profileKeys, profileQuery } from "@/data/lib/profile/queries";

import type { ProfileLoaderData } from "./profile-loader.server";
import { ProfileErrorBoundary, ProfileRoute, profileClientLoader } from "./profile-route";

const A = "0x4e9c4a2502fdf71e93ed8ed6ca9ddbd891d6f295";
const avatar = (name: string) => ({ name, hasClaimedName: true, description: "", avatar: { snapshots: { face256: "" } } });
const identity = { signer: A, ephemeral: { address: "0xeph", privateKey: "0xdeadbeef" }, expiration: "2999-01-01T00:00:00.000Z", authChain: [] };

let client: QueryClient;

function Other() {
  const navigate = useNavigate();
  return <button onClick={() => navigate(`/profile/${A}`)}>back to profile</button>;
}

/** The profile routes as Sites declares them, with the loader the browser runs on a navigation. */
function mount(path: string, loader: () => unknown = profileClientLoader) {
  const route = {
    loader,
    Component: () => <ProfileRoute loaderData={null} />,
    ErrorBoundary: ProfileErrorBoundary,
  };
  // Under a root route with its own boundary, as Sites' root.tsx has.
  const router = createMemoryRouter(
    [
      {
        path: "/",
        ErrorBoundary: () => <h1>Something went wrong</h1>,
        children: [
          { path: "profile/:address", ...route },
          { path: "profile/:address/:tab", ...route },
          { path: "other", Component: Other },
        ],
      },
    ],
    { initialEntries: [path] },
  );
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

/** The first document: the server's loader data is handed to the page as its props. */
function mountDocument(path: string, data: ProfileLoaderData) {
  const router = createMemoryRouter(
    [
      {
        path: "/profile/:address/:tab?",
        loader: () => data,
        Component: function Doc() {
          return <ProfileRoute loaderData={data} />;
        },
        ErrorBoundary: ProfileErrorBoundary,
      },
    ],
    { initialEntries: [path] },
  );
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

function serverState(name: string): ProfileLoaderData {
  const server = new QueryClient();
  server.setQueryData(profileKeys.profile(A), mapProfile(avatar(name) as never, A));
  return { sid: "sid-1", address: A, own: false, dehydratedState: dehydrate(server) };
}

/**
 * The unavailable screen, once the failed page no longer observes the profile read. React can show
 * the screen one task before it unsubscribes the page. A step taken in between acts on a read that
 * the page still owns: Retry reads through the page, and the page's exit cancels a read in flight.
 */
async function unavailable() {
  const heading = await screen.findByRole("heading", { name: "Profile unavailable" });
  await waitFor(() =>
    expect(client.getQueryCache().find({ queryKey: profileKeys.profile(A) })?.getObserversCount()).toBe(0),
  );
  return heading;
}

beforeEach(() => {
  client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 30_000 } } });
  auth.state = { isConnected: false, address: null, identity: null };
});
afterEach(() => {
  cleanup();
  client.clear();
  vi.resetAllMocks();
});

describe("profile route, first hydration", () => {
  it("renders the server's profile without asking for it again", async () => {
    mountDocument(`/profile/${A}`, serverState("Alice"));

    expect(await screen.findByRole("heading", { name: "Alice" })).toBeTruthy();
    expect(net.fetchProfile).not.toHaveBeenCalled();
    expect(client.getQueryData(profileKeys.profile(A))).toMatchObject({ name: "Alice" });
  });

  it("keeps the profile on screen when a background re-read fails", async () => {
    mountDocument(`/profile/${A}`, serverState("Alice"));
    await screen.findByRole("heading", { name: "Alice" });
    net.fetchProfile.mockRejectedValue(new CatalystError("Catalyst unreachable", "/lambdas/profile", 0));

    await act(async () => {
      await client.refetchQueries({ queryKey: profileKeys.profile(A) });
    });

    expect(screen.getByRole("heading", { name: "Alice" })).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

describe("profile route, client navigation", () => {
  it("keeps a newer healthy public profile when the previous error boundary leaves", async () => {
    net.fetchProfile.mockRejectedValueOnce(
      new CatalystError("Catalyst unreachable", "/lambdas/profile", 0),
    );
    const router = mount(`/profile/${A}`);
    await unavailable();
    const fresh = mapProfile(avatar("Recovered") as never, A);
    act(() => client.setQueryData(profileKeys.profile(A), fresh));
    await act(async () => {
      await router.navigate("/other");
    });
    expect(client.getQueryData(profileKeys.profile(A))).toEqual(fresh);
    await act(async () => {
      await router.navigate(`/profile/${A}`);
    });
    expect(await screen.findByRole("heading", { name: "Recovered" })).toBeTruthy();
    expect(net.fetchProfile).toHaveBeenCalledTimes(1);
  });

  it("keeps usable cached data after a newer background read fails", async () => {
    net.fetchProfile.mockRejectedValue(
      new CatalystError("Catalyst unreachable", "/lambdas/profile", 0),
    );
    const router = mount(`/profile/${A}`);
    await unavailable();
    const fresh = mapProfile(avatar("Recovered") as never, A);
    act(() => client.setQueryData(profileKeys.profile(A), fresh));
    await expect(
      client.fetchQuery({ ...profileQuery(A), staleTime: 0 }),
    ).rejects.toBeInstanceOf(CatalystError);
    await act(async () => {
      await router.navigate("/other");
    });
    expect(client.getQueryData(profileKeys.profile(A))).toEqual(fresh);
  });

  it("does not cancel a newer in-flight public profile read when the old boundary leaves", async () => {
    let finish!: (value: ReturnType<typeof avatar>) => void;
    let signal: AbortSignal | undefined;
    const response = new Promise<ReturnType<typeof avatar>>((resolve) => {
      finish = resolve;
    });
    net.fetchProfile
      .mockRejectedValueOnce(
        new CatalystError("Catalyst unreachable", "/lambdas/profile", 0),
      )
      .mockImplementationOnce((_address, options: { signal: AbortSignal }) => {
        signal = options.signal;
        return response;
      });
    const router = mount(`/profile/${A}`);
    await unavailable();
    const read = client.fetchQuery(profileQuery(A)).then(
      (value) => ({ ok: true, value }),
      (error: unknown) => ({ ok: false, error }),
    );
    try {
      await act(async () => {
        await router.navigate("/other");
      });
      expect(signal?.aborted).toBe(false);
    } finally {
      finish(avatar("Recovered"));
    }
    expect(await read).toMatchObject({
      ok: true,
      value: { name: "Recovered" },
    });
    expect(net.fetchProfile).toHaveBeenCalledTimes(2);
  });

  it("leaves a failed public profile owned by another active observer intact", async () => {
    const failure = new CatalystError(
      "Catalyst unreachable",
      "/lambdas/profile",
      0,
    );
    net.fetchProfile.mockRejectedValue(failure);
    const router = mount(`/profile/${A}`);
    await unavailable();
    const observer = new QueryObserver(client, {
      ...profileQuery(A),
      retryOnMount: false,
    });
    const unsubscribe = observer.subscribe(() => {});
    try {
      await act(async () => {
        await router.navigate("/other");
      });
      expect(observer.getCurrentResult().isError).toBe(true);
      expect(client.getQueryState(profileKeys.profile(A))?.error).toBe(failure);
      expect(net.fetchProfile).toHaveBeenCalledTimes(1);
    } finally {
      unsubscribe();
    }
  });

  it("uses a newer healthy profile when Retry leaves the old error boundary", async () => {
    net.fetchProfile.mockRejectedValueOnce(new CatalystError("Catalyst unreachable", "/lambdas/profile", 0));
    mount(`/profile/${A}`);
    await unavailable();
    const fresh = mapProfile(avatar("Recovered") as never, A);
    act(() => client.setQueryData(profileKeys.profile(A), fresh));

    await act(async () => {
      screen.getByRole("button", { name: "Try again" }).click();
    });

    expect(await screen.findByRole("heading", { name: "Recovered" })).toBeTruthy();
    expect(net.fetchProfile).toHaveBeenCalledTimes(1);
  });

  it("keeps repeated failed retries to one new read per attempt", async () => {
    net.fetchProfile
      .mockRejectedValueOnce(new CatalystError("Catalyst unreachable", "/lambdas/profile", 0))
      .mockRejectedValueOnce(new CatalystError("Catalyst unreachable", "/lambdas/profile", 0))
      .mockResolvedValueOnce(avatar("Recovered"));
    const router = mount(`/profile/${A}`);
    await unavailable();

    await act(async () => {
      screen.getByRole("button", { name: "Try again" }).click();
    });
    await waitFor(() => expect(client.getQueryState(profileKeys.profile(A))?.status).toBe("error"));
    expect(router.state.navigation.state).toBe("idle");
    expect(net.fetchProfile).toHaveBeenCalledTimes(2);
    expect(await unavailable()).toBeTruthy();

    await act(async () => {
      screen.getByRole("button", { name: "Try again" }).click();
    });
    expect(await screen.findByRole("heading", { name: "Recovered" })).toBeTruthy();
    expect(net.fetchProfile).toHaveBeenCalledTimes(3);
  });

  it("receives no server data and reads the profile through the browser client", async () => {
    net.fetchProfile.mockResolvedValue(avatar("Alice"));

    mount(`/profile/${A}`);

    expect(await screen.findByRole("heading", { name: "Alice" })).toBeTruthy();
    expect(net.fetchProfile).toHaveBeenCalledTimes(1);
  });

  it("shows the unavailable screen when the profile cannot be read, and Retry reads it again", async () => {
    net.fetchProfile.mockRejectedValueOnce(new CatalystError("Catalyst unreachable", "/lambdas/profile", 0)).mockResolvedValueOnce(avatar("Alice"));

    mount(`/profile/${A}`);

    expect(await unavailable()).toBeTruthy();
    screen.getByRole("button", { name: "Try again" }).click();
    expect(await screen.findByRole("heading", { name: "Alice" })).toBeTruthy();
    expect(net.fetchProfile).toHaveBeenCalledTimes(2);
  });

  it("reads the profile afresh after leaving a failed profile and coming back", async () => {
    net.fetchProfile.mockRejectedValueOnce(new CatalystError("Catalyst unreachable", "/lambdas/profile", 0)).mockResolvedValueOnce(avatar("Alice"));
    const router = mount(`/profile/${A}`);
    await unavailable();

    await act(async () => {
      await router.navigate("/other");
    });
    expect(client.getQueryState(profileKeys.profile(A))).toMatchObject({
      status: "pending",
      error: null,
      fetchStatus: "idle",
    });
    screen.getByRole("button", { name: "back to profile" }).click();

    expect(await screen.findByRole("heading", { name: "Alice" })).toBeTruthy();
    expect(net.fetchProfile).toHaveBeenCalledTimes(2);
  });

  it("hands a bug in the profile read to the root boundary, not the profile screen", async () => {
    net.fetchProfile.mockRejectedValue(new TypeError("Cannot read properties of undefined (reading 'name')"));

    mount(`/profile/${A}`);

    expect(await screen.findByRole("heading", { name: "Something went wrong" })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "Profile unavailable" })).toBeNull();
  });

  it("shows unavailable, not \"invalid address\", when the node answers 404 for a wallet", async () => {
    net.fetchProfile.mockRejectedValue(new CatalystError("Catalyst returned 404", "/lambdas/profile", 404));

    mount(`/profile/${A}`);

    expect(await unavailable()).toBeTruthy();
  });

  it("shows not found for an address that cannot be a wallet, without reading anything", async () => {
    mount("/profile/not-a-wallet");

    expect(await screen.findByRole("heading", { name: "Profile not found" })).toBeTruthy();
    expect(net.fetchProfile).not.toHaveBeenCalled();
  });

  it("sends a member's hidden tab back to the profile root", async () => {
    net.fetchProfile.mockResolvedValue(avatar("Alice"));

    const router = mount(`/profile/${A}/assets`);

    await waitFor(() => expect(router.state.location.pathname).toBe(`/profile/${A}`));
  });
});

describe("profile route, communities", () => {
  it("reports a failed read of the member's own communities once", async () => {
    auth.state = { isConnected: true, address: A, identity };
    net.fetchProfile.mockResolvedValue(avatar("Alice"));
    net.fetchMyCommunities.mockRejectedValue(new Error("500"));

    mount(`/profile/${A}/communities`);

    await waitFor(() =>
      expect(net.track).toHaveBeenCalledWith("profile_communities_load_failed", { address: A }, expect.anything()),
    );
    act(() => {
      focusManager.setFocused(false);
      focusManager.setFocused(true);
    });
    await new Promise((resolve) => setTimeout(resolve, 20));
    focusManager.setFocused(undefined);
    expect(net.track.mock.calls.filter(([event]) => event === "profile_communities_load_failed")).toHaveLength(1);
    expect(net.fetchMyCommunities).toHaveBeenCalledTimes(1);
  });

  it("neither reads nor reports communities on another member's profile", async () => {
    auth.state = { isConnected: true, address: "0x1111111111111111111111111111111111111111", identity };
    net.fetchProfile.mockResolvedValue(avatar("Alice"));

    mount(`/profile/${A}/communities`);

    expect(await screen.findByRole("heading", { name: "Alice" })).toBeTruthy();
    expect(net.fetchMyCommunities).not.toHaveBeenCalled();
    expect(net.track.mock.calls.some(([event]) => event === "profile_communities_load_failed")).toBe(false);
  });
});

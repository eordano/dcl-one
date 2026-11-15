// Which entry-url params stop the boot on the UntrustedLaunchGate — THIS front-end's policy.
// The engine's web param table (lib/webParams.ts) says what a param does; whether a link may set
// it is decided here, because trust is a property of the host: the editor app trusts its editor
// scene, this app trusts its bundled bridge scene and Decentraland's own deployments.

import { SERVICES, type ServiceDef } from '@explorer-core/engine/generated'
import {
  BASE_DOMAIN,
  derivedServiceUrl,
  hostBaseDomain,
  isTrustedBaseDomain,
  normaliseServiceUrl
} from './baseDomain'
import { bootMode } from './bootMode'
import { isTrustedSystemScene } from './systemScene'
import { webParam } from '@explorer-core/lib/webParams'
import type { UntrustedParam } from '@explorer-core/hostContract'

export type { UntrustedParam }


interface Gate {
  warning: string
  /** The effective value the engine will see, or null when the param is not in play. Read from
   *  the ENTRY url on first use — a later history.replaceState can't retire a warning. */
  read: (native: boolean) => string | null
  isTrusted: (value: string) => boolean
}

// Keyed by the table's param name (checked at module load).
const GATES: Record<string, Gate> = {
  // Substitutes the super-user scene — see lib/systemScene.ts for why that is a session takeover.
  systemScene: {
    warning:
      "Replaces the Explorer's interface with a scene loaded from this address. It can move your avatar, change your profile, and answer permission prompts on your behalf.",
    read: () => bootMode().systemScene,
    isTrusted: isTrustedSystemScene
  },
  // The HUD consumes this one itself (its own backend urls), so the value is the resolved
  // BASE_DOMAIN — normalised by the engine's rule, or derived from the origin when the param is
  // absent/invalid (both derive to a trusted deployment). Native is exempt: there the shell
  // injects it from the user's own --base-domain flag, not from a link.
  baseDomain: {
    warning:
      'Points every backend service — sign-in, content, comms — at servers under this domain. Whoever runs them would see your session and control what you play.',
    read: (native) => (native ? null : BASE_DOMAIN),
    isTrusted: isTrustedBaseDomain
  },
  // Repoints every scene and asset fetch at one server while everything else looks normal.
  contentServer: {
    warning:
      'Fetches every scene and asset from this server instead of the realm’s own. Whoever runs it decides what you see and what runs.',
    read: () => new URLSearchParams(location.search).get('contentServer'),
    isTrusted: isTrustedContentServer
  },
  // Swaps the textures and models of scenes, wearables and emotes for the ones this server
  // lists. They are trusted by their path on this server and rejected only when they fail to
  // decode: nothing checks them against the original files, so the server decides what scenes
  // and avatars look like. Same rule as the content server.
  bevyVariants: {
    warning:
      'Replaces the textures and models of the scenes you visit and the avatars you see with versions from this server. Whoever runs it decides what they look like.',
    read: () => new URLSearchParams(location.search).get('bevyVariants'),
    // `off` only turns the default (the asset-bundle generator's CDN) off
    isTrusted: (url) => url.trim().toLowerCase() === 'off' || isTrustedContentServer(url)
  }
}

/** A content server under one of Decentraland's own deployments. */
function isTrustedContentServer(url: string): boolean {
  try {
    return hostBaseDomain(new URL(url).hostname) != null
  } catch {
    return false
  }
}
/** A service value — url, or `host[:port]` for an authority service — under one of Decentraland's own deployments. */
function isTrustedServiceUrl(s: ServiceDef, value: string): boolean {
  try {
    const u = new URL(s.value === 'authority' ? `dummy://${value}` : value)
    return hostBaseDomain(u.hostname) != null
  } catch {
    return false
  }
}
// Every per-service override: worse than ?baseDomain= in that it can poison ONE service —
// sign-in, profiles, comms — while everything else looks normal. Same rule as the content server,
// and native is exempt the same way as the base domain (the shell injects the user's own flags).
// Read normalised, like the base domain: a value the HUD discards is never put to the user. A value
// equal to what this deployment derives by itself is no override at all: the engine writes its
// effective options into the address bar, so every reload of a dcl-one deployment outside
// Decentraland's own domains would otherwise stop here on its own urls.
for (const s of SERVICES) {
  GATES[s.name] = {
    warning: `Points the Explorer's "${s.name}" backend service at this server, with everything else looking normal. Whoever runs it would see the requests and choose the answers.`,
    read: (native) => (native ? null : normaliseServiceUrl(s, new URLSearchParams(location.search).get(s.name))),
    isTrusted: (value) => value === derivedServiceUrl(s.name) || isTrustedServiceUrl(s, value)
  }
}
for (const name of Object.keys(GATES)) webParam(name)
// The benchmark's switches (docs/guidelines/benchmark.md: `autopilot`, `perfStream`, `perfCsv`) have
// no entry here, and that is deliberate: there is nothing to approve. A link could never be
// allowed to run an unattended session or stream the log from a visitor's browser, and /play has no
// benchmark run at all, so it ignores the switches on every host: `autopilot` is kept out of the
// engine's launch options (features/engine/EngineHost.tsx) and `perfStream` and `perfCsv` are
// never read. The web explorer's own HUD (source/react-web) honours them on loopback hosts and in
// native only.
// The Pulse certificate pin is a PAGE param (lib/entryParams.ts), not a table one: it makes the
// browser accept a non-CA certificate for the comms server, so whoever holds that certificate and
// the network path could stand in for it. No value is trusted; a developer pinning a local server
// clicks through once.
GATES.pulseCertificateHash = {
  warning:
    'Makes the Explorer accept a self-signed certificate for its comms server. Whoever holds that certificate could stand in for the server and see where players are.',
  read: (native) => (native ? null : new URLSearchParams(location.search).get('pulseCertificateHash')),
  isTrusted: () => false
}

/** The gated params whose entry-url value this front-end doesn't recognise. Empty = boot. */
export function untrustedLaunchParams({ native }: { native: boolean }): UntrustedParam[] {
  const out: UntrustedParam[] = []
  for (const [name, gate] of Object.entries(GATES)) {
    const value = gate.read(native)
    if (value == null || value === '' || gate.isTrusted(value)) continue
    out.push({ name, value, warning: gate.warning })
  }
  return out
}

// A signed-in session for both sides, as decentraland.org's dapps keep one: the shared
// `single-sign-on-<address>` identity (@dcl/single-sign-on-client's format, which both the
// reference and apps/sites read) and, for the reference's decentraland-connect, its stored
// injected connection and a `window.ethereum` that answers with the account and forwards every
// read to the network (so the recording holds it, as any other request).
//
// The wallet is a test wallet whose key comes from the environment (SHOP_PARITY_WALLET_KEY),
// never from the repository. The ephemeral key is derived from it, so the same wallet and clock
// give the same session on every run, and the captures do not change with it.
import type { BrowserContext } from "playwright-core";
import { keccak256, toHex, type Hex } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { runnable, walletKey, WALLET_KEY_ENV } from "./wallet-key.mts";

export { runnable, walletKey, WALLET_KEY_ENV };

/** The test wallet's signer for a recording's signed reads, when its key is set (record.mts, nav-walk.mts --fill). */
export function testWalletSigner(): ((method: string, path: string, metadata: string) => Promise<Record<string, string>>) | undefined {
  if (!process.env[WALLET_KEY_ENV]) return undefined;
  const key = walletKey();
  return (method, path, metadata) => signedHeaders(key, method, path, Date.now(), metadata);
}

export type SharedIdentity = {
  ephemeralIdentity: { address: string; publicKey: string; privateKey: string };
  expiration: string;
  authChain: { type: string; payload: string; signature: string }[];
};

/** The session `key` signs at `now` (epoch ms), valid for a day. */
export async function sessionFor(key: Hex, now: number): Promise<{ address: string; identity: SharedIdentity }> {
  const wallet = privateKeyToAccount(key);
  const ephemeralKey = keccak256(toHex(`shop-parity ephemeral ${key}`)) as Hex;
  const ephemeral = privateKeyToAccount(ephemeralKey);
  const expiration = new Date(now + 24 * 60 * 60 * 1000).toISOString();
  const login = ["Decentraland Login", `Ephemeral address: ${ephemeral.address}`, `Expiration: ${expiration}`].join("\n");
  const address = wallet.address.toLowerCase();
  return {
    address,
    identity: {
      ephemeralIdentity: { address: ephemeral.address, publicKey: ephemeral.publicKey, privateKey: ephemeralKey },
      expiration,
      authChain: [
        { type: "SIGNER", payload: address, signature: "" },
        { type: "ECDSA_EPHEMERAL", payload: login, signature: await wallet.signMessage({ message: login }) },
      ],
    },
  };
}

/** Signs `context` in before any page loads, on either side. */
export async function signIn(context: BrowserContext, key: Hex, now: number, rpcUrl = "https://rpc.decentraland.org/mainnet"): Promise<string> {
  const { address, identity } = await sessionFor(key, now);
  await context.addInitScript(
    ({ address, identity, rpcUrl }) => {
      try {
        localStorage.setItem(`single-sign-on-${address}`, JSON.stringify(identity));
        localStorage.setItem("decentraland-connect-storage-key", JSON.stringify({ providerType: "injected", chainId: 1 }));
      } catch {
        // A frame without storage stays signed out; the page's own frame has it.
      }
      let id = 0;
      const listeners: Record<string, ((...args: unknown[]) => void)[]> = {};
      const provider = {
        isMetaMask: true,
        selectedAddress: address,
        chainId: "0x1",
        networkVersion: "1",
        async request({ method, params }: { method: string; params?: unknown[] }) {
          switch (method) {
            case "eth_accounts":
            case "eth_requestAccounts":
              return [address];
            case "eth_chainId":
              return "0x1";
            case "net_version":
              return "1";
            case "wallet_switchEthereumChain":
            case "wallet_addEthereumChain":
              return null;
            case "personal_sign":
            case "eth_sign":
            case "eth_signTypedData":
            case "eth_signTypedData_v3":
            case "eth_signTypedData_v4":
            case "eth_sendTransaction":
            case "eth_sendRawTransaction":
              // A capture never signs or sends: the wallet declines, as a buyer would (EIP-1193 4001).
              throw Object.assign(new Error("User rejected the request."), { code: 4001 });
            default: {
              if (!/^(eth_(call|getBalance|blockNumber|chainId|getBlockBy(Number|Hash)|getCode|getLogs|getTransaction(Count|Receipt|ByHash)|estimateGas|gasPrice|feeHistory|maxPriorityFeePerGas|getStorageAt)|net_version)$/.test(method)) {
                throw Object.assign(new Error(`The test wallet does not support ${method}.`), { code: 4200 });
              }
              const res = await fetch(rpcUrl, {
                method: "POST",
                headers: { "content-type": "application/json" },
                body: JSON.stringify({ jsonrpc: "2.0", id: ++id, method, params: params ?? [] }),
              });
              const body = await res.json();
              if (body.error) throw Object.assign(new Error(body.error.message), body.error);
              return body.result;
            }
          }
        },
        on(event: string, fn: (...args: unknown[]) => void) {
          (listeners[event] ??= []).push(fn);
          return provider;
        },
        removeListener(event: string, fn: (...args: unknown[]) => void) {
          listeners[event] = (listeners[event] ?? []).filter((f) => f !== fn);
          return provider;
        },
        enable: async () => [address],
      };
      Object.defineProperty(window, "ethereum", { value: provider, configurable: true });
    },
    { address, identity, rpcUrl },
  );
  return address;
}

/** The `x-identity-*` headers for one request to `path`, signed by `key`'s session at `now` (the real time). */
export async function signedHeaders(key: Hex, method: string, path: string, now = Date.now(), metadata = "{}"): Promise<Record<string, string>> {
  const { identity } = await sessionFor(key, now);
  const ephemeral = privateKeyToAccount(identity.ephemeralIdentity.privateKey as Hex);
  const timestamp = String(now);
  const payload = `${`${method}:${path}`.toLowerCase()}:${timestamp}:${metadata}`;
  const chain = [...identity.authChain, { type: "ECDSA_SIGNED_ENTITY", payload, signature: await ephemeral.signMessage({ message: payload }) }];
  const headers: Record<string, string> = { "x-identity-timestamp": timestamp, "x-identity-metadata": metadata };
  chain.forEach((link, i) => (headers[`x-identity-auth-chain-${i}`] = JSON.stringify(link)));
  return headers;
}

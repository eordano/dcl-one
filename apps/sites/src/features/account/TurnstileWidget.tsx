import { useEffect, useRef } from "react";

const SCRIPT_SRC = "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";

type TurnstileOptions = {
  sitekey: string;
  theme: "dark";
  size: "normal";
  retry: "auto";
  callback: (token: string) => void;
  "expired-callback": () => void;
  "error-callback": () => void;
};

export type TurnstileApi = {
  render: (element: HTMLElement, options: TurnstileOptions) => string;
  remove: (widgetId: string) => void;
};

declare global {
  interface Window {
    turnstile?: TurnstileApi;
  }
}

let loading: Promise<TurnstileApi> | null = null;

function loadTurnstile(): Promise<TurnstileApi> {
  if (window.turnstile) return Promise.resolve(window.turnstile);
  loading ??= new Promise<TurnstileApi>((resolve, reject) => {
    const script = document.createElement("script");
    script.src = SCRIPT_SRC;
    script.async = true;
    // A failed load forgets the script, so the next mount tries again.
    const fail = () => {
      script.remove();
      loading = null;
      reject(new Error("Turnstile did not load"));
    };
    script.onload = () => (window.turnstile ? resolve(window.turnstile) : fail());
    script.onerror = fail;
    document.head.appendChild(script);
  });
  return loading;
}

type TurnstileWidgetProps = {
  siteKey: string;
  onToken: (token: string) => void;
  /** The token expired or the widget failed: no token until it is solved again. */
  onExpire: () => void;
  onLoadError: () => void;
};

/** Cloudflare Turnstile, rendered explicitly; remount it (a new `key`) to get a fresh challenge. */
export function TurnstileWidget({ siteKey, onToken, onExpire, onLoadError }: TurnstileWidgetProps) {
  const element = useRef<HTMLDivElement>(null);
  const callbacks = useRef({ onToken, onExpire, onLoadError });
  callbacks.current = { onToken, onExpire, onLoadError };

  useEffect(() => {
    let cancelled = false;
    let widgetId: string | null = null;
    loadTurnstile().then(
      (api) => {
        if (cancelled || !element.current) return;
        try {
          widgetId = api.render(element.current, {
            sitekey: siteKey,
            theme: "dark",
            size: "normal",
            retry: "auto",
            callback: (token) => callbacks.current.onToken(token),
            "expired-callback": () => callbacks.current.onExpire(),
            "error-callback": () => callbacks.current.onExpire(),
          });
        } catch {
          callbacks.current.onLoadError();
        }
      },
      () => {
        if (!cancelled) callbacks.current.onLoadError();
      },
    );
    return () => {
      cancelled = true;
      if (widgetId) window.turnstile?.remove(widgetId);
    };
  }, [siteKey]);

  return <div ref={element} />;
}

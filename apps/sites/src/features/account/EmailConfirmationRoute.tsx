import {
  EmailConfirmation,
  type EmailConfirmationState,
} from "@dcl/one-ui/apps/sites/account/EmailConfirmation";
import { useCallback, useEffect, useRef, useState } from "react";

import {
  emailConfirmationFailure,
  isEmailConfirmationSource,
  type EmailConfirmationSource,
} from "@/data/lib/account/notifications";

import { useConfirmEmail } from "./hooks";
import { TurnstileWidget } from "./TurnstileWidget";

const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
const CONTINUE: Record<EmailConfirmationSource, string> = { account: "/", credits: "/shop" };
const WIDGET_UNAVAILABLE = "The security check could not load. Reload the page to try again.";

export type EmailConfirmationRouteProps = {
  code: string;
  address: string | null;
  source: string | null;
  turnstileSiteKey: string | null;
  /** `challenge` waits for the member's click; `auto` confirms on load, as the links without a challenge did. */
  mode: "challenge" | "auto";
};

function invalidLink(code: string, address: string | null, source: string | null): EmailConfirmationState | null {
  if (!code) return { kind: "invalid", title: "Invalid Link", message: "This confirmation link is invalid or expired." };
  if (!isEmailConfirmationSource(source)) {
    return { kind: "invalid", title: "Invalid Source", message: "Invalid confirmation source. Please check your email link." };
  }
  if (!address || !ADDRESS.test(address)) {
    return { kind: "invalid", title: "Missing Address", message: "Address parameter is required in the URL. Please check your email link." };
  }
  return null;
}

export function EmailConfirmationRoute({ code, address, source, turnstileSiteKey, mode }: EmailConfirmationRouteProps) {
  const confirm = useConfirmEmail();
  const [token, setToken] = useState("");
  const [widget, setWidget] = useState(0);
  const [widgetFailed, setWidgetFailed] = useState(false);
  const invalid = invalidLink(code, address, source);
  const started = useRef(false);

  const submit = useCallback(() => {
    if (invalid || !isEmailConfirmationSource(source) || !address) return;
    confirm.mutate(
      { address, code, source, ...(token ? { turnstileToken: token } : {}) },
      {
        // A token is single-use: any refusal needs a fresh challenge.
        onError: () => {
          setToken("");
          setWidget((n) => n + 1);
        },
      },
    );
  }, [address, code, confirm, invalid, source, token]);

  useEffect(() => {
    if (mode !== "auto" || invalid || started.current) return;
    started.current = true;
    submit();
  }, [invalid, mode, submit]);

  if (invalid) return <EmailConfirmation source="account" state={invalid} />;
  const linkSource = source as EmailConfirmationSource;
  const failure = confirm.isError ? emailConfirmationFailure(confirm.error, { challengeShown: !!turnstileSiteKey }) : null;

  let state: EmailConfirmationState;
  if (confirm.isSuccess) state = { kind: "confirmed", continueHref: CONTINUE[linkSource] };
  else if (failure && !failure.retry) state = { kind: "failed", message: failure.message, homeHref: "/" };
  else if (mode === "auto" && !failure) state = { kind: "confirming" };
  else {
    state = {
      kind: "challenge",
      pending: confirm.isPending,
      canConfirm: !turnstileSiteKey || !!token,
      error: failure?.message ?? (widgetFailed ? WIDGET_UNAVAILABLE : undefined),
    };
  }

  const challenge = turnstileSiteKey ? (
    <TurnstileWidget
      key={widget}
      siteKey={turnstileSiteKey}
      onToken={(value) => {
        setWidgetFailed(false);
        setToken(value);
      }}
      onExpire={() => setToken("")}
      onLoadError={() => setWidgetFailed(true)}
    />
  ) : undefined;

  return <EmailConfirmation source={linkSource} state={state} challenge={challenge} onConfirm={submit} />;
}

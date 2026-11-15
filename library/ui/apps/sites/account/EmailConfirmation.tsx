import { useId, type ReactNode } from 'react'
import decentralandLogo from '../assets/dcl-logo.svg'
import { Button } from '../shared/Button/Button'
import styles from './EmailConfirmation.module.css'

type EmailConfirmationSource = 'account' | 'credits'

type EmailConfirmationState =
  | { kind: 'invalid'; title: string; message: string }
  | { kind: 'challenge'; pending: boolean; canConfirm: boolean; error?: string }
  | { kind: 'confirming' }
  | { kind: 'confirmed'; continueHref: string }
  | { kind: 'failed'; message: string; homeHref: string }

type EmailConfirmationProps = {
  source: EmailConfirmationSource
  state: EmailConfirmationState
  /** The security challenge widget; omitted when the site runs without one. */
  challenge?: ReactNode
  onConfirm?: () => void
}

const GOAL: Record<EmailConfirmationSource, ReactNode> = {
  account: 'to activate your account notifications.',
  credits: (
    <>
      to start earning <b>Marketplace Credits!</b>
    </>
  )
}

const READY: Record<EmailConfirmationSource, ReactNode> = {
  account: 'Jump back over to the Decentraland app and start using your account!',
  credits: (
    <>
      Jump back over to the Decentraland app and start earning <b>Marketplace Credits!</b>
    </>
  )
}

/** The page an emailed confirmation link opens: challenge, progress, success or refusal. */
function EmailConfirmation({ source, state, challenge, onConfirm }: EmailConfirmationProps) {
  const titleId = useId()
  return (
    <main className={styles.root}>
      <img className={styles.logo} src={decentralandLogo} alt="Decentraland" />
      <section className={styles.card} aria-labelledby={titleId}>
        {state.kind === 'invalid' ? (
          <>
            <h1 className={styles.title} id={titleId}>
              {state.title}
            </h1>
            <p className={styles.message}>{state.message}</p>
          </>
        ) : null}
        {state.kind === 'challenge' ? (
          <>
            <h1 className={styles.title} id={titleId}>
              Confirm Your Email
            </h1>
            <p className={styles.message}>
              {challenge
                ? 'Complete the security challenge below and click "Confirm Email" '
                : 'Click "Confirm Email" '}
              {GOAL[source]}
            </p>
            {challenge ? <div className={styles.challenge}>{challenge}</div> : null}
            {state.error ? (
              <p className={styles.error} role="alert">
                {state.error}
              </p>
            ) : null}
            <Button
              className={styles.action}
              disabled={state.pending || !state.canConfirm}
              aria-busy={state.pending || undefined}
              onClick={onConfirm}
            >
              {state.pending ? 'Confirming...' : 'Confirm Email'}
            </Button>
          </>
        ) : null}
        {state.kind === 'confirming' ? (
          <>
            <h1 className={styles.title} id={titleId}>
              Confirming Your Email
            </h1>
            <p className={styles.message} role="status">
              One moment while we confirm your email.
            </p>
          </>
        ) : null}
        {state.kind === 'confirmed' ? (
          <>
            <h1 className={styles.title} id={titleId}>
              Email Confirmed!
            </h1>
            <p className={styles.message} role="status">
              You&apos;re ready to go.
              <br />
              {READY[source]}
            </p>
            <Button className={styles.action} href={state.continueHref}>
              {source === 'credits' ? 'Go to Shop' : 'Go to Decentraland'}
            </Button>
          </>
        ) : null}
        {state.kind === 'failed' ? (
          <>
            <h1 className={styles.title} id={titleId}>
              We Couldn&apos;t Confirm Your Email
            </h1>
            <p className={styles.error} role="alert">
              {state.message}
            </p>
            <Button className={styles.action} href={state.homeHref} variant="secondary">
              Go to Decentraland
            </Button>
          </>
        ) : null}
      </section>
    </main>
  )
}

export { EmailConfirmation }
export type { EmailConfirmationProps, EmailConfirmationSource, EmailConfirmationState }

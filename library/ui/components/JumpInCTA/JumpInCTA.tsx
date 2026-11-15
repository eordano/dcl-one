import type { AnchorHTMLAttributes, ButtonHTMLAttributes, MouseEvent, ReactNode } from 'react'
import styles from './JumpInCTA.module.css'

type OwnProps = {
  size?: 'xl' | 'lg' | 'md' | 'sm'
  disabled?: boolean
  /** Shows the spinner in place of the icon and stops activation. */
  loading?: boolean
  label?: ReactNode
  className?: string
}

type NativeButton = Omit<ButtonHTMLAttributes<HTMLButtonElement>, keyof OwnProps | 'children'> & {
  href?: undefined
}
type NativeLink = Omit<AnchorHTMLAttributes<HTMLAnchorElement>, keyof OwnProps | 'children'> & {
  href: string
}

export type JumpInCTAProps = OwnProps & (NativeButton | NativeLink)

const block = (event: MouseEvent<HTMLElement>) => event.preventDefault()

/** decentraland-ui2 JumpInIcon, as decentraland.org draws it inside the CTA. */
function JumpInIcon() {
  return (
    <svg
      className={styles.icon}
      viewBox="0 0 24 24"
      fill="none"
      focusable="false"
      aria-hidden="true"
    >
      <rect className={styles.iconFrame} x="0.75" y="0.75" width="22.5" height="22.5" rx="5.25" strokeWidth="1.5" />
      <path
        className={styles.iconArrow}
        d="M18.7111 11.065L14.034 6.39027C13.2002 5.55695 11.7971 6.14637 11.7971 7.32523V8.86994C11.7564 8.86994 11.7361 8.86994 11.6954 8.86994H7.25895C6.50654 8.86994 5.89648 9.45936 5.89648 10.2114V13.7683C5.89648 14.5203 6.50654 15.1301 7.25895 15.1301H11.6751C11.7158 15.1301 11.7361 15.1301 11.7768 15.1301V16.6748C11.7768 17.8536 13.2002 18.4431 14.0137 17.6097L18.6908 12.935C19.2195 12.4065 19.2195 11.5732 18.7111 11.065Z"
      />
    </svg>
  )
}

export function JumpInCTA(props: JumpInCTAProps) {
  const { size = 'xl', disabled = false, loading = false, label = 'Jump in', className, ...rest } = props
  const inert = disabled || loading
  const shared = {
    className: [styles.root, styles[size], className].filter(Boolean).join(' '),
    'aria-busy': loading || undefined,
    'data-disabled': disabled || undefined,
    'data-loading': loading || undefined
  }
  const content = (
    <>
      <span>{label}</span>
      {loading ? <span className={styles.spinner} aria-hidden="true" /> : <JumpInIcon />}
    </>
  )

  if (rest.href !== undefined) {
    const { href, onClick, ...link } = rest as NativeLink
    return (
      <a
        {...link}
        {...shared}
        href={inert ? undefined : href}
        aria-disabled={inert || undefined}
        tabIndex={inert ? -1 : link.tabIndex}
        onClick={inert ? block : onClick}
      >
        {content}
      </a>
    )
  }

  const { type = 'button', ...button } = rest as NativeButton
  return (
    <button
      {...button}
      {...shared}
      type={type}
      disabled={disabled}
      aria-disabled={loading || undefined}
      onClick={loading ? block : button.onClick}
    >
      {content}
    </button>
  )
}

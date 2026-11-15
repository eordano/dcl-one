import type { ButtonHTMLAttributes } from 'react'
import styles from './NavNotificationsButton.module.css'

export type NavNotificationsButtonProps = {
  state?: 'open' | 'closed'
  label?: string
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'children' | 'type'>

// from: decentraland-ui2@3.23.8:dist/components/Navbar/icons.js (BellIcon): the filled bell of DCL's navbar.
function Bell() {
  return (
    <svg viewBox="0 0 24 24" width={24} height={24} fill="none" aria-hidden="true">
      <path
        d="M12 21.75C13.1 21.75 14 20.85 14 19.75H10C10 20.85 10.89 21.75 12 21.75ZM18 15.75V10.75C18 7.68 16.36 5.11 13.5 4.43V3.75C13.5 2.92 12.83 2.25 12 2.25C11.17 2.25 10.5 2.92 10.5 3.75V4.43C7.63 5.11 6 7.67 6 10.75V15.75L4 17.75V18.75H20V17.75L18 15.75Z"
        fill="currentColor"
      />
    </svg>
  )
}

/** The bell that opens the signed-in visitor's notifications panel; the panel belongs to the caller. */
export function NavNotificationsButton({
  state = 'closed',
  label = 'Notifications',
  className,
  ...rest
}: NavNotificationsButtonProps) {
  return (
    <button
      type="button"
      aria-label={label}
      aria-haspopup="dialog"
      aria-expanded={state === 'open'}
      {...rest}
      className={[styles.root, className].filter(Boolean).join(' ')}
    >
      <Bell />
    </button>
  )
}

import type { HTMLAttributes, KeyboardEvent, ReactNode } from 'react'
import styles from './TabsBar.module.css'

export type TabsBarProps = {
  label: string
  children: ReactNode
  /** `underline` is the Figma bar (a bottom rule); `pill` spaces rounded tabs as decentraland.org's events page does. */
  variant?: 'underline' | 'pill'
} & Omit<HTMLAttributes<HTMLDivElement>, 'children' | 'role' | 'aria-label'>

const KEYS = new Set(['ArrowLeft', 'ArrowRight', 'Home', 'End'])

/** A tab list: arrows, Home and End move focus between its tabs; a button tab is also activated. */
export function TabsBar({ label, children, variant = 'underline', className, onKeyDown, ...rest }: TabsBarProps) {
  const handleKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    onKeyDown?.(event)
    if (event.defaultPrevented || !KEYS.has(event.key)) return
    const tabs = Array.from(event.currentTarget.querySelectorAll<HTMLElement>('[role="tab"]'))
    const current = tabs.indexOf(event.target as HTMLElement)
    if (current < 0) return
    event.preventDefault()
    const last = tabs.length - 1
    const next =
      event.key === 'Home'
        ? 0
        : event.key === 'End'
          ? last
          : (current + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length
    const tab = tabs[next]!
    tab.focus()
    if (tab instanceof HTMLButtonElement) tab.click()
  }
  return (
    <div
      {...rest}
      role="tablist"
      aria-label={label}
      className={[styles.root, styles[variant], className].filter(Boolean).join(' ')}
      onKeyDown={handleKeyDown}
    >
      {children}
    </div>
  )
}

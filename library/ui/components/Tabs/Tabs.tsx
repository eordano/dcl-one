import type { AnchorHTMLAttributes, ButtonHTMLAttributes, ReactNode } from 'react'
import styles from './Tabs.module.css'

type TabsBaseProps = {
  label: ReactNode
  /** The current tab: `aria-selected`, and the only one in the tab order (TabsBar moves focus with the arrows). */
  selected?: boolean
  /** `underline` is the Figma tab; `pill` is the rounded tab decentraland.org shows on the events page. */
  variant?: 'underline' | 'pill'
  /** Fixes the Figma device size; without it the tab follows the viewport. */
  device?: 'mobile' | 'desktop'
}

type TabsLinkProps = TabsBaseProps & { href: string } & Omit<
    AnchorHTMLAttributes<HTMLAnchorElement>,
    'children' | 'href' | 'role'
  >

type TabsButtonProps = TabsBaseProps & { href?: undefined } & Omit<
    ButtonHTMLAttributes<HTMLButtonElement>,
    'children' | 'type' | 'role'
  >

export type TabsProps = TabsLinkProps | TabsButtonProps

/** One tab of a TabsBar: a link when it has `href` (works without script), otherwise a button. */
export function Tabs(props: TabsProps) {
  const { label, selected = false, variant = 'underline', device, className, tabIndex, ...rest } = props
  const classes = [styles.root, styles[variant], device && styles[device], className]
    .filter(Boolean)
    .join(' ')
  const shared = {
    role: 'tab',
    'aria-selected': selected,
    tabIndex: tabIndex ?? (selected ? 0 : -1),
    className: classes
  } as const
  if (rest.href !== undefined) {
    return (
      <a {...(rest as AnchorHTMLAttributes<HTMLAnchorElement>)} {...shared}>
        {label}
      </a>
    )
  }
  return (
    <button type="button" {...(rest as ButtonHTMLAttributes<HTMLButtonElement>)} {...shared}>
      {label}
    </button>
  )
}

import type { ElementType, ReactNode } from 'react'
import type { PolymorphicLinkProps } from '../../link.helpers'
import styles from './NavMenuTab.module.css'

export type NavMenuTabProps<C extends ElementType = 'a'> = PolymorphicLinkProps<
  C,
  {
    label: ReactNode
    /** The section the visitor is in: marked as the current page. */
    active?: boolean
    /**
     * The tab opens a menu: it shows the chevron and announces the menu. The menu belongs to the caller,
     * which passes the menu's id as `aria-controls` and opens and closes it from the pointer and the keys.
     */
    hasMenu?: boolean
    /** Whether the tab's menu is open; read only when `hasMenu` is set. */
    expanded?: boolean
  }
>

/**
 * A section tab of the site bar: a link to the section. With `hasMenu` it stays a link that also opens the
 * section's menu, so it carries `aria-haspopup` and `aria-expanded`, which the link role supports.
 */
export function NavMenuTab<C extends ElementType = 'a'>({
  as,
  label,
  active = false,
  hasMenu = false,
  expanded = false,
  className,
  ...rest
}: NavMenuTabProps<C>) {
  const Link: ElementType = as ?? 'a'
  return (
    <Link
      {...rest}
      className={[styles.root, active && styles.active, className].filter(Boolean).join(' ')}
      aria-current={active ? 'page' : undefined}
      aria-haspopup={hasMenu ? 'menu' : undefined}
      aria-expanded={hasMenu ? expanded : undefined}
    >
      {label}
    </Link>
  )
}

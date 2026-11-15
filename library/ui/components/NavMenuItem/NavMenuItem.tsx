import type { ElementType, ReactNode } from 'react'
import type { PolymorphicLinkProps } from '../../link.helpers'
import styles from './NavMenuItem.module.css'

export type NavMenuItemProps<C extends ElementType = 'a'> = PolymorphicLinkProps<
  C,
  { label: ReactNode }
>

/**
 * One link of a site bar menu. Its role is `menuitem`, so the caller renders it inside an element with
 * `role="menu"` and moves focus between the items with the arrow, Home and End keys and closes the menu
 * with Escape. A caller that shows the links as a plain list passes `role={undefined}`.
 */
export function NavMenuItem<C extends ElementType = 'a'>({
  as,
  label,
  className,
  ...rest
}: NavMenuItemProps<C>) {
  const Link: ElementType = as ?? 'a'
  return (
    <Link
      role="menuitem"
      {...rest}
      className={[styles.root, className].filter(Boolean).join(' ')}
    >
      {label}
    </Link>
  )
}

import type { ComponentPropsWithoutRef, ElementType } from 'react'

/**
 * The props of a part that renders an anchor, or the caller's link component passed as `as`: the part's
 * own props, then every prop of that component but its children.
 */
export type PolymorphicLinkProps<C extends ElementType, Own> = Own & {
  as?: C
  className?: string
} & Omit<ComponentPropsWithoutRef<C>, 'as' | 'children' | 'className'>

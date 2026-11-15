import type { HTMLAttributes } from 'react'
import styles from './Skeleton.module.css'

export type SkeletonProps = Omit<HTMLAttributes<HTMLSpanElement>, 'children'> & {
  /** `brand` is the Figma purple sheen; `neutral` is the grey pulse decentraland.org draws on dark surfaces. */
  tone?: 'brand' | 'neutral'
}

/**
 * A loading placeholder: a purple block with a sheen that sweeps across it. It is decoration, so
 * assistive technology skips it; the region that loads states its busy state (`aria-busy`).
 * The consumer sizes it through `className` or `style`; without either it takes the Figma 200 × 112.
 */
export function Skeleton({ className, tone = 'brand', ...rest }: SkeletonProps) {
  const classes = [styles.root, tone === 'neutral' && styles.neutral, className].filter(Boolean).join(' ')
  return <span aria-hidden="true" {...rest} className={classes} />
}

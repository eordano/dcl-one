import type { HTMLAttributes } from 'react'
import styles from './Spinner.module.css'

export type SpinnerProps = {
  /** `dark` draws the track for a light surface, `light` for a dark one (Figma "Property 1"). */
  variant?: 'dark' | 'light'
  label?: string
} & Omit<HTMLAttributes<HTMLSpanElement>, 'children' | 'role'>

export function Spinner({ variant = 'dark', label = 'Loading', className, ...rest }: SpinnerProps) {
  return (
    <span
      role="progressbar"
      aria-label={label}
      {...rest}
      className={[styles.root, variant === 'light' ? styles.light : styles.dark, className].filter(Boolean).join(' ')}
    />
  )
}

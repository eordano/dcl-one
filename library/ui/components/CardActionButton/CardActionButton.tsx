import type { ButtonHTMLAttributes, ReactNode } from 'react'
import { GeneralLike } from '../../foundations/icons/general'
import styles from './CardActionButton.module.css'

export type CardActionButtonProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'children'> & {
  /** The button has no visible text, so it needs an accessible name. */
  'aria-label': string
  /** The 24px glyph; the foundations thumbs-up (`GeneralLike`) when not given. */
  icon?: ReactNode
}

/**
 * An icon-only action of a card, named by `aria-label`. A caller that uses it as a toggle (a like, a save)
 * passes `aria-pressed`: the button keeps every native button attribute.
 */
export function CardActionButton({ icon, className, type = 'button', ...rest }: CardActionButtonProps) {
  return (
    <button {...rest} type={type} className={[styles.root, className].filter(Boolean).join(' ')}>
      {icon ?? <GeneralLike className={styles.icon} focusable="false" aria-hidden="true" />}
    </button>
  )
}

import type { CSSProperties, HTMLAttributes } from 'react'
import styles from './NavProfileButton.module.css'

export type NavProfileButtonProps = {
  /** The account's face picture; without one the disc shows `fallbackSrc`. */
  imageUrl?: string
  /** The disc colour, as the reference takes it from the display name. */
  color?: string
  fallbackSrc: string
  /** Called when the face picture fails to load, so the caller can drop it. */
  onImageError?: () => void
} & Omit<HTMLAttributes<HTMLSpanElement>, 'children' | 'color'>

/**
 * The avatar disc of the site bar's profile control. The button that opens the profile menu belongs to the
 * caller, which owns the menu, its keyboard handling and the account actions. The disc is decoration: it
 * has no role and its pictures have an empty alt, so the caller's button carries the accessible name.
 */
export function NavProfileButton({
  imageUrl,
  color,
  fallbackSrc,
  onImageError,
  className,
  style,
  ...rest
}: NavProfileButtonProps) {
  return (
    <span
      {...rest}
      className={[styles.root, className].filter(Boolean).join(' ')}
      style={color ? ({ ...style, '--nav-profile-color': color } as CSSProperties) : style}
    >
      {imageUrl ? (
        <img
          className={styles.image}
          src={imageUrl}
          alt=""
          width="48"
          height="48"
          loading="lazy"
          onError={onImageError}
        />
      ) : (
        <img className={styles.fallback} src={fallbackSrc} alt="" width="36" height="36" />
      )}
    </span>
  )
}

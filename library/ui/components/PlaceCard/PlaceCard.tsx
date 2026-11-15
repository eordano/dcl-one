import type { ComponentType, CSSProperties, ReactNode } from 'react'
import { Skeleton } from '../Skeleton/Skeleton'
import styles from './PlaceCard.module.css'

/** The router's link, passed in so the card renders under any router. */
export type PlaceCardLinkProps = {
  to: string
  className?: string
  onClick?: () => void
  children?: ReactNode
}
export type PlaceCardLink = ComponentType<PlaceCardLinkProps>

/** The credited creator. `profileHref` makes the name a link to their profile. */
export type PlaceCardCreator = {
  name?: string
  avatar?: string
  /** The avatar's background colour, behind a transparent face. */
  avatarBackground?: string
  profileHref?: string
}

export type PlaceCardProps = {
  /**
   * `grid` is the Explore grid's card; `live` is the Live Now rail's card (decentraland.org's
   * LiveEventCard): a taller cover, the creator without coordinates, and on a phone JUMP IN under it.
   */
  variant?: 'grid' | 'live'
  /** Figma "loading": a skeleton in place of the card, marked busy. */
  loading?: boolean
  /** The name assistive technology reads for the loading card. */
  loadingLabel?: string
  title?: string
  /** The place page the title opens. The whole card is the title link's target. Without it the title is text. */
  href?: string
  /** Where JUMP IN goes. Without it the card has no JUMP IN and keeps its details on hover. */
  jumpHref?: string
  image?: string
  players?: number
  /** Show the presence count; by default only when someone is in the place. */
  showPlayers?: boolean
  live?: boolean
  /** The LIVE badge's text. */
  liveLabel?: string
  /** The running event's name, shown as the LIVE badge's tooltip. */
  liveEventName?: string
  featured?: boolean
  featuredLabel?: string
  /** The parcel coordinates, as `x,y`. */
  coords?: string
  creator?: PlaceCardCreator
  /** The word before the creator's name. */
  byLabel?: string
  jumpLabel?: string
  Link?: PlaceCardLink
  onOpen?: () => void
}

function Anchor({ to, className, onClick, children }: PlaceCardLinkProps) {
  return (
    <a className={className} href={to} onClick={onClick}>
      {children}
    </a>
  )
}

/**
 * The cover as the `--cover` custom property the module paints. The quote, the backslash and the three
 * characters that end a CSS string (line feed, carriage return, form feed) are percent-encoded.
 */
function coverStyle(image: string): CSSProperties {
  return { '--cover': `url("${image.replace(/["\\\n\r\f]/g, encodeURIComponent)}")` } as CSSProperties
}

/**
 * A place in a grid: the cover with its LIVE, presence and Featured badges, then the title, the
 * creator and the coordinates. Hover or keyboard focus inside the card swaps the details for JUMP IN.
 * The title is the card's link (its hit area covers the card); the creator and JUMP IN stay separate
 * links above it.
 */
export function PlaceCard({
  variant = 'grid',
  loading = false,
  loadingLabel = 'Loading',
  title = '',
  href,
  jumpHref,
  image,
  players = 0,
  showPlayers = players > 0,
  live = false,
  liveLabel = 'LIVE',
  liveEventName,
  featured = false,
  featuredLabel = 'Featured',
  coords,
  creator,
  byLabel = 'By',
  jumpLabel = 'Jump in',
  Link = Anchor,
  onOpen
}: PlaceCardProps) {
  if (loading) {
    return (
      <div className={styles.container} role="progressbar" aria-label={loadingLabel} aria-busy="true">
        <div className={styles.card}>
          <Skeleton className={styles.skeleton} />
        </div>
      </div>
    )
  }
  const badges = { players, showPlayers, live, liveLabel, liveEventName, featured, featuredLabel }
  const cover = image ? coverStyle(image) : undefined
  const heading = { title, href, Link, onOpen }
  if (variant === 'live') {
    return (
      <div className={styles.container}>
        <div className={styles.liveCard}>
          <div className={styles.liveMedia} style={cover}>
            <Badges {...badges} medalClassName={styles.liveMedal} />
          </div>
          <div className={styles.liveBar}>
            <Title {...heading} className={styles.liveTitle} />
            <div className={styles.liveSwap}>
              <div className={styles.liveBy}>
                {creator?.name && (
                  <>
                    {creator.avatar && <Avatar creator={creator} className={styles.liveAvatar} />}
                    <By creator={creator} label={byLabel} className={styles.liveByText} />
                  </>
                )}
              </div>
              {jumpHref && (
                <Jump
                  href={jumpHref}
                  label={jumpLabel}
                  className={styles.liveJump}
                  glyphClassName={styles.liveJumpGlyph}
                />
              )}
            </div>
          </div>
        </div>
      </div>
    )
  }
  return (
    <div className={styles.container}>
      <div className={styles.card}>
        <div className={styles.media}>
          <div className={styles.cover} style={cover} />
          <Badges {...badges} medalClassName={styles.medal} />
        </div>
        <div className={styles.body}>
          <Title {...heading} className={styles.title} />
          <div className={styles.swap}>
            <div className={styles.meta}>
              <div className={styles.creator}>
                {creator?.avatar && <Avatar creator={creator} className={styles.avatar} />}
                {creator?.name && <By creator={creator} label={byLabel} className={styles.by} />}
              </div>
              {coords && (
                <div className={styles.location}>
                  <PinGlyph />
                  {coords}
                </div>
              )}
            </div>
            {jumpHref && (
              <Jump
                href={jumpHref}
                label={jumpLabel}
                className={styles.jump}
                glyphClassName={styles.jumpGlyph}
              />
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

/** The title of both variants: the card's link when the place page is known, text when it is not. */
function Title({
  title,
  href,
  Link,
  onOpen,
  className
}: {
  title: string
  href: string | undefined
  Link: PlaceCardLink
  onOpen: (() => void) | undefined
  className: string | undefined
}) {
  return (
    <p className={className}>
      {href ? (
        <Link className={styles.link} to={href} onClick={onOpen}>
          {title}
        </Link>
      ) : (
        title
      )}
    </p>
  )
}

/**
 * JUMP IN of both variants. It is the card's own link and not JumpInCTA: the reference sets it in
 * Arial, scales it with the card (cqw) and swaps it in over the details on hover.
 */
function Jump({
  href,
  label,
  className,
  glyphClassName
}: {
  href: string
  label: string
  className: string | undefined
  glyphClassName: string | undefined
}) {
  return (
    <a className={className} href={href}>
      {label}
      <JumpInGlyph className={glyphClassName} />
    </a>
  )
}

function Badges({
  players,
  showPlayers,
  live,
  liveLabel,
  liveEventName,
  featured,
  featuredLabel,
  medalClassName
}: Pick<
  Required<PlaceCardProps>,
  'players' | 'showPlayers' | 'live' | 'liveLabel' | 'featured' | 'featuredLabel'
> & {
  liveEventName?: string
  medalClassName: string | undefined
}) {
  return (
    <div className={styles.topRow}>
      <div className={styles.group}>
        {live && (
          <div className={styles.live} title={liveEventName}>
            <Icon d={SENSORS} name="SensorsRoundedIcon" />
            {liveLabel}
          </div>
        )}
        {showPlayers && (
          <div className={styles.count}>
            <div className={styles.dot} />
            <Icon d={PERSON} name="PermIdentityRoundedIcon" />
            {players}
          </div>
        )}
      </div>
      {featured && (
        <div className={styles.featured}>
          <MedalGlyph className={medalClassName} />
          {featuredLabel}
        </div>
      )}
    </div>
  )
}

type CreatorProps = { creator: PlaceCardCreator; className: string | undefined }

function Avatar({ creator, className }: CreatorProps) {
  return (
    <img
      className={className}
      src={creator.avatar}
      alt=""
      loading="lazy"
      style={{ '--avatar-bg': creator.avatarBackground } as CSSProperties}
    />
  )
}

function By({ creator, label, className }: CreatorProps & { label: string }) {
  return (
    <p className={className}>
      {label}{' '}
      {creator.profileHref ? (
        <a className={styles.nameLink} href={creator.profileHref}>
          {creator.name}
        </a>
      ) : (
        <span className={styles.name}>{creator.name}</span>
      )}
    </p>
  )
}

// from: @mui/icons-material PermIdentityRounded and SensorsRounded (the badges' icons on decentraland.org)
const PERSON =
  'M12 12c2.21 0 4-1.79 4-4s-1.79-4-4-4-4 1.79-4 4 1.79 4 4 4m0-6c1.1 0 2 .9 2 2s-.9 2-2 2-2-.9-2-2 .9-2 2-2m0 7c-2.67 0-8 1.34-8 4v2c0 .55.45 1 1 1h14c.55 0 1-.45 1-1v-2c0-2.66-5.33-4-8-4m6 5H6v-.99c.2-.72 3.3-2.01 6-2.01s5.8 1.29 6 2z'
const SENSORS =
  'M8.54 8.54c.35.35.37.88.1 1.29C8.24 10.45 8 11.2 8 12c0 .8.24 1.55.64 2.17.27.41.24.95-.11 1.29-.43.43-1.17.4-1.51-.11C6.38 14.4 6 13.24 6 12c0-1.21.36-2.33.97-3.28.36-.54 1.11-.64 1.57-.18m6.92 6.92c.43.43 1.17.4 1.51-.11C17.62 14.4 18 13.24 18 12c0-1.24-.38-2.4-1.03-3.36-.34-.5-1.08-.54-1.51-.11-.35.35-.37.88-.11 1.29.41.63.65 1.38.65 2.18 0 .8-.24 1.55-.64 2.17-.27.41-.24.95.1 1.29M12 10c-1.1 0-2 .9-2 2s.9 2 2 2 2-.9 2-2-.9-2-2-2m6.32 8.32c.42.42 1.12.39 1.5-.08C21.18 16.53 22 14.36 22 12s-.82-4.53-2.18-6.24c-.37-.47-1.07-.5-1.5-.08-.36.36-.4.92-.08 1.32 1.1 1.37 1.76 3.11 1.76 5s-.66 3.63-1.76 5c-.32.39-.28.96.08 1.32M5.68 5.68c-.42-.42-1.12-.39-1.5.08C2.82 7.47 2 9.64 2 12s.82 4.53 2.18 6.24c.37.47 1.07.5 1.5.08.36-.36.4-.92.08-1.32C4.66 15.63 4 13.89 4 12s.66-3.63 1.76-5c.32-.39.28-.96-.08-1.32'

/** The icon keeps its MUI test id so the card pairs element for element with decentraland.org's. */
function Icon({ d, name }: { d: string; name: string }) {
  return (
    <svg
      className={styles.icon}
      viewBox="0 0 24 24"
      aria-hidden="true"
      focusable="false"
      data-testid={name}
    >
      <path d={d} />
    </svg>
  )
}

// from: decentraland.org's Places card glyphs (sites@17b3535a src/components/places/_shared/CardIcons.tsx), paths verbatim.
// The jump glyph here and the icon of JumpInCTA are two drawings on the reference: this one has a filled square
// in a 24.87 px box, that one an open frame in a 24 px box.
function MedalGlyph({ className }: { className: string | undefined }) {
  return (
    <svg className={[styles.glyph, className].filter(Boolean).join(' ')} aria-hidden="true" focusable="false" viewBox="0 0 12 14" fill="none">
      <path d="M1.70573 8.60973C2.65715 9.71845 4.02892 10.4545 5.57266 10.5702C5.57069 10.5747 5.56812 10.5793 5.56585 10.5838L3.91221 13.8646C3.82445 14.0387 3.57919 14.0467 3.48104 13.8782L2.48145 12.1608C2.43368 12.0787 2.34327 12.0317 2.24883 12.0398L0.26719 12.2096C0.07458 12.2261 -0.0608893 12.0235 0.0277572 11.8514L1.70573 8.60973Z" />
      <path d="M11.9721 11.8514C12.0608 12.0234 11.9261 12.2257 11.7337 12.2096L9.75203 12.0398C9.6575 12.0316 9.56624 12.0786 9.51843 12.1608L8.51885 13.8782C8.42065 14.0465 8.17542 14.0387 8.08768 13.8646L6.43404 10.5838C6.4318 10.5794 6.43014 10.5747 6.4282 10.5702C7.972 10.4545 9.34374 9.71857 10.2951 8.60973L11.9721 11.8514Z" />
      <path fillRule="evenodd" clipRule="evenodd" d="M5.99994 0C8.70388 0 10.8964 2.19753 10.8966 4.90842C10.8966 7.61953 8.70404 9.81781 5.99994 9.81781C3.29598 9.81766 1.10423 7.61943 1.10423 4.90842C1.10449 2.19762 3.29614 0.000150887 5.99994 0ZM6.21699 2.22489C6.12476 2.05002 5.875 2.04987 5.7829 2.22489L5.11229 3.49932C5.07673 3.5667 5.01203 3.61416 4.9371 3.62715L3.52094 3.87209C3.32669 3.90604 3.24927 4.14492 3.38663 4.28681L4.38815 5.31924C4.44118 5.37394 4.46608 5.45061 4.45531 5.52612L4.25092 6.95278C4.22321 7.14842 4.42528 7.29554 4.60228 7.20844L5.89191 6.5722C5.9602 6.53861 6.04069 6.53855 6.10895 6.5722L7.39761 7.20844C7.57464 7.29572 7.77761 7.14846 7.74994 6.95278L7.54458 5.52612C7.53384 5.45064 7.55873 5.37392 7.61173 5.31924L8.61326 4.28681C8.75088 4.14485 8.6735 3.90584 8.47895 3.87209L7.06279 3.62715C6.98794 3.61404 6.92307 3.5667 6.8876 3.49932L6.21699 2.22489Z" />
    </svg>
  )
}

function PinGlyph() {
  return (
    <svg className={[styles.glyph, styles.pin].filter(Boolean).join(' ')} aria-hidden="true" focusable="false" viewBox="0 0 15 15" fill="none">
      <path fillRule="evenodd" clipRule="evenodd" d="M11.082 10.394C10.3068 11.565 9.41232 12.6461 8.90834 13.2307C8.21939 14.0298 6.99075 14.0298 6.3018 13.2307C5.79782 12.6461 4.90333 11.565 4.12816 10.394C3.74091 9.80902 3.36728 9.1775 3.08668 8.55678C2.81567 7.95727 2.58267 7.26354 2.58267 6.58317C2.58267 3.79361 4.81361 1.5 7.60507 1.5C10.3965 1.5 12.6275 3.79361 12.6275 6.58317C12.6275 7.26354 12.3945 7.95727 12.1235 8.55678C11.8429 9.1775 11.4692 9.80902 11.082 10.394ZM8.00374 12.4508C7.79124 12.6973 7.4189 12.6973 7.2064 12.4508C6.22605 11.3137 3.77702 8.30103 3.77702 6.58317C3.77702 4.43544 5.4909 2.69436 7.60507 2.69436C9.71924 2.69436 11.4331 4.43544 11.4331 6.58317C11.4331 8.30103 8.98409 11.3137 8.00374 12.4508Z" />
      <path fillRule="evenodd" clipRule="evenodd" d="M7.6043 7.95758C8.35176 7.95758 8.9577 7.34202 8.9577 6.58269C8.9577 5.82336 8.35176 5.20781 7.6043 5.20781C6.85683 5.20781 6.25089 5.82336 6.25089 6.58269C6.25089 7.34202 6.85683 7.95758 7.6043 7.95758Z" />
    </svg>
  )
}

function JumpInGlyph({ className }: { className: string | undefined }) {
  return (
    <svg
      className={[styles.glyph, className].filter(Boolean).join(' ')}
      aria-hidden="true"
      focusable="false"
      viewBox="0 0 24.8735 24.8735"
      fill="none"
    >
      <rect className={styles.jumpGlyphFill} width="24.8735" height="24.8735" rx="8.29118" />
      <rect
        className={styles.jumpGlyphEdge}
        x="0.777298"
        y="0.777298"
        width="23.3189"
        height="23.3189"
        rx="7.51388"
        strokeOpacity="0.3"
        strokeWidth="1.5546"
      />
      <path className={styles.jumpGlyphArrow} d="M18.9554 11.031L14.1081 6.1861C13.244 5.32244 11.7898 5.93332 11.7898 7.15508V8.75601C11.7476 8.75601 11.7265 8.75601 11.6844 8.75601H7.08641C6.30662 8.75601 5.67436 9.36689 5.67436 10.1463V13.8326C5.67436 14.612 6.30662 15.244 7.08641 15.244H11.6633C11.7055 15.244 11.7265 15.244 11.7687 15.244V16.8449C11.7687 18.0667 13.244 18.6776 14.087 17.8139L18.9343 12.969C19.4823 12.4213 19.4823 11.5576 18.9554 11.031Z" />
    </svg>
  )
}

// Rows have fixed heights, so only the ones near the viewport are mounted.

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { AnchoredMenu, Button, MaskIcon, type ContextMenuItem } from '../../design'
import { photoTime } from '../session/useEngineSession'
import { useOptionalSession } from '../session/SessionContext'
import type { GalleryPhoto } from '@explorer-core/engine/protocol'
import { copyReelLink, downloadReel, shareReelToX } from './reelActions'
import menuVerticalIcon from '../../assets/camera-reel/menu-vertical.png'
import shareIcon from '../../assets/camera-reel/share-x.png'
import linkIcon from '../../assets/camera-reel/copy-link.png'
import downloadIcon from '../../assets/camera-reel/download.png'
import deleteIcon from '../../assets/camera-reel/delete.png'
import styles from './ReelGrid.module.css'

export interface ReelGridLayout {
  columns: number
  /** Thumbnail size in canvas px. With `fitWidth` the width follows the grid's width and the height
   *  keeps this aspect. */
  tileWidth: number
  tileHeight: number
  fitWidth?: boolean
  groupByMonth: boolean
  paddingTop: number
  paddingBottom: number
  paddingLeft: number
  monthSpacing: number
  optionButtonHeight: number
}

export interface ReelGridMenu {
  showDelete: boolean
  /** Resolves whether the service took the change. */
  onSetPublic: (photo: GalleryPhoto, isPublic: boolean) => Promise<boolean>
  onDelete?: (photo: GalleryPhoto) => void
}

// The header is one 18 px line.
const GAP = 4
const HEADER_SPACING = 20
const HEADER_HEIGHT = 22
const OVERSCAN = 600
const OPTION_BUTTON_CENTRE_Y = 33

type Row =
  | { kind: 'padding'; top: number; height: number }
  | { kind: 'header'; top: number; height: number; label: string }
  | { kind: 'thumbs'; top: number; height: number; start: number; count: number }

function reelMonthLabel(dateTime: string): string {
  return new Date(photoTime(dateTime)).toLocaleDateString('en-US', { month: 'long', year: 'numeric' })
}

type RowInit = Row extends infer R ? (R extends Row ? Omit<R, 'top'> : never) : never

export function buildReelRows(photos: GalleryPhoto[], layout: ReelGridLayout, tileHeight: number): { rows: Row[]; height: number } {
  const rows: Row[] = []
  let top = 0
  const push = (row: RowInit): void => {
    rows.push({ ...row, top } as Row)
    top += row.height
  }
  push({ kind: 'padding', height: layout.paddingTop })
  let start = 0
  while (start < photos.length) {
    const label = reelMonthLabel(photos[start].dateTime)
    let end = photos.length
    if (layout.groupByMonth) {
      end = start + 1
      while (end < photos.length && reelMonthLabel(photos[end].dateTime) === label) end++
      push({ kind: 'header', height: HEADER_HEIGHT + HEADER_SPACING, label })
    }
    for (let at = start; at < end; at += layout.columns) {
      const count = Math.min(layout.columns, end - at)
      const last = at + count >= end
      const after = !last ? GAP : end >= photos.length ? 0 : layout.monthSpacing
      push({ kind: 'thumbs', height: tileHeight + after, start: at, count })
    }
    start = end
  }
  push({ kind: 'padding', height: layout.paddingBottom })
  return { rows, height: top }
}

export function ReelGrid({
  photos,
  layout,
  menu,
  onOpen,
  onVisibleIndex,
  onRetry,
  'aria-label': ariaLabel
}: {
  photos: GalleryPhoto[]
  layout: ReelGridLayout
  /** The photo options; null where the reel isn't yours. */
  menu: ReelGridMenu | null
  onOpen: (index: number) => void
  onVisibleIndex: (index: number) => void
  /** Set when the next page failed: the grid ends with a row that asks for it again. */
  onRetry?: () => void
  'aria-label'?: string
}): React.JSX.Element {
  const scrollRef = useRef<HTMLDivElement>(null)
  // `measured` is false until the viewport has been read once: the first render can't know what is
  // on screen, and reporting every row as visible then would read pages nobody scrolled to.
  const [view, setView] = useState({ top: 0, height: 0, width: 0, measured: false })

  useLayoutEffect(() => {
    const el = scrollRef.current
    if (el == null) return
    const measure = (): void => setView({ top: el.scrollTop, height: el.clientHeight, width: el.clientWidth, measured: true })
    measure()
    if (typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(measure)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  const tileWidth = layout.fitWidth && view.width > 0 ? (view.width - layout.paddingLeft - GAP * (layout.columns - 1)) / layout.columns : layout.tileWidth
  const tileHeight = layout.fitWidth && view.width > 0 ? (tileWidth * layout.tileHeight) / layout.tileWidth : layout.tileHeight
  const { rows, height } = useMemo(() => buildReelRows(photos, layout, tileHeight), [photos, layout, tileHeight])

  // A viewport with no height (no layout, as in a test DOM): mount every row rather than none.
  const all = view.height === 0
  const visible = rows.filter((r) => all || (r.top + r.height >= view.top - OVERSCAN && r.top <= view.top + view.height + OVERSCAN))

  let lastOnScreen = -1
  for (const r of rows) {
    if (r.kind === 'thumbs' && (all || r.top <= view.top + view.height)) lastOnScreen = r.start + r.count - 1
  }
  useEffect(() => {
    if (view.measured) onVisibleIndex(lastOnScreen)
  }, [view.measured, lastOnScreen, photos.length, onVisibleIndex])

  const [menuFor, setMenuFor] = useState<{ photo: GalleryPhoto; anchor: { x: number; y: number }; trigger: HTMLElement } | null>(null)
  const openPhoto = menuFor != null ? photos.find((p) => p.id === menuFor.photo.id) : undefined
  useEffect(() => {
    if (menuFor != null && openPhoto == null) setMenuFor(null)
  }, [menuFor, openPhoto])

  return (
    <div
      ref={scrollRef}
      className={styles.scroll}
      aria-label={ariaLabel}
      onScroll={(e) => {
        const el = e.currentTarget
        setView((v) => (v.top === el.scrollTop ? v : { ...v, top: el.scrollTop }))
        // The menu is placed once, at its ⋮: a scroll would leave it floating over another photo, or
        // over none once the row unmounts.
        if (menuFor != null) setMenuFor(null)
      }}
    >
      <div className={styles.canvas} style={{ height }}>
        {visible.map((r) => {
          if (r.kind === 'padding') return null
          if (r.kind === 'header') {
            return (
              <h2 key={`h${r.top}`} className={styles.month} style={{ top: r.top, left: layout.paddingLeft }}>
                {r.label}
              </h2>
            )
          }
          return (
            <div key={photos[r.start].id} className={styles.row} style={{ top: r.top, left: layout.paddingLeft, gap: GAP }}>
              {photos.slice(r.start, r.start + r.count).map((photo, i) => (
                <Thumbnail
                  key={photo.id}
                  photo={photo}
                  width={tileWidth}
                  height={tileHeight}
                  optionButtonHeight={layout.optionButtonHeight}
                  hasMenu={menu != null}
                  menuOpen={menuFor?.photo.id === photo.id}
                  onOpen={() => onOpen(r.start + i)}
                  onMenu={(trigger) => {
                    if (menuFor?.photo.id === photo.id) {
                      setMenuFor(null)
                      return
                    }
                    const box = trigger.getBoundingClientRect()
                    setMenuFor({ photo, trigger, anchor: { x: box.left + box.width / 2, y: box.top + box.height / 2 } })
                  }}
                />
              ))}
            </div>
          )
        })}
      </div>
      {onRetry != null && (
        <div className={styles.retry} role="status" style={{ paddingLeft: layout.paddingLeft }}>
          <span>Couldn't load more photos.</span>
          <Button variant="secondary" size="sm" onClick={onRetry}>
            RETRY
          </Button>
        </div>
      )}
      {menu != null && menuFor != null && openPhoto != null && (
        <ReelMenu key={openPhoto.id} photo={openPhoto} anchor={menuFor.anchor} trigger={menuFor.trigger} menu={menu} onClose={() => setMenuFor(null)} />
      )}
    </div>
  )
}

function Thumbnail({
  photo,
  width,
  height,
  optionButtonHeight,
  hasMenu,
  menuOpen,
  onOpen,
  onMenu
}: {
  photo: GalleryPhoto
  width: number
  height: number
  optionButtonHeight: number
  hasMenu: boolean
  menuOpen: boolean
  onOpen: () => void
  onMenu: (trigger: HTMLElement) => void
}): React.JSX.Element {
  const [loaded, setLoaded] = useState(false)
  const [failed, setFailed] = useState(false)
  const ready = loaded && !failed
  return (
    <div
      className={`${styles.tile} ${ready ? styles.ready : ''} ${menuOpen ? styles.active : ''}`.trim()}
      style={{ width, height }}
    >
      <button type="button" className={styles.open} aria-label="Open photo" disabled={!ready} onClick={onOpen}>
        {!loaded && !failed && <span className={styles.loading} aria-hidden="true" />}
        <img
          className={styles.image}
          src={photo.thumbnailUrl ?? photo.url}
          alt=""
          draggable={false}
          onLoad={() => setLoaded(true)}
          onError={() => setFailed(true)}
        />
      </button>
      <span className={styles.outline} aria-hidden="true" />
      {hasMenu && ready && (
        <button
          type="button"
          className={styles.options}
          style={{ height: optionButtonHeight, top: OPTION_BUTTON_CENTRE_Y - optionButtonHeight / 2 }}
          aria-label="Photo options"
          aria-haspopup="menu"
          aria-expanded={menuOpen}
          onClick={(e) => onMenu(e.currentTarget)}
        >
          <MaskIcon src={menuVerticalIcon} size={24} className={styles.optionsIcon} />
        </button>
      )}
    </div>
  )
}

function ReelMenu({
  photo,
  anchor,
  trigger,
  menu,
  onClose
}: {
  photo: GalleryPhoto
  anchor: { x: number; y: number }
  trigger: HTMLElement
  menu: ReelGridMenu
  onClose: () => void
}): React.JSX.Element {
  const [isPublic, setIsPublic] = useState(photo.isPublic === true)
  const openExternalUrl = useOptionalSession()?.openExternalUrl
  const icon = (src: string): React.JSX.Element => <MaskIcon src={src} size={18} />
  const items: ContextMenuItem[] = [
    {
      kind: 'toggle',
      label: 'Set as Public',
      checked: isPublic,
      onChange: (next) => {
        setIsPublic(next)
        void menu.onSetPublic(photo, next).then((ok) => {
          if (!ok) setIsPublic(!next)
        })
      }
    },
    { kind: 'separator' },
    { label: 'Share on X', icon: icon(shareIcon), onClick: () => shareReelToX(photo.id, openExternalUrl) },
    { label: 'Copy Link', icon: icon(linkIcon), onClick: () => copyReelLink(photo.id) },
    { label: 'Download', icon: icon(downloadIcon), onClick: () => void downloadReel(photo.url) }
  ]
  if (menu.showDelete && menu.onDelete != null) {
    const onDelete = menu.onDelete
    items.push({ label: 'Delete', icon: icon(deleteIcon), onClick: () => onDelete(photo) })
  }
  return <AnchoredMenu anchor={anchor} items={items} onClose={onClose} ignore={trigger} aria-label="Photo options" />
}

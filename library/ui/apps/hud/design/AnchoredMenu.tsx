// AnchoredMenu — a ContextMenu opened from a point on screen, kept on screen.

import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { getHudScale } from '@explorer-core/lib/hudScale'
import { registerCancelLayer } from '@explorer-core/lib/cancelLayers'
import { ContextMenu, type ContextMenuItem } from './ContextMenu'
import styles from './AnchoredMenu.module.css'

const OFFSET_X = 11
const OFFSET_Y = 18

/** All in canvas px. */
export function anchoredMenuPosition(
  anchor: { x: number; y: number },
  size: { width: number; height: number },
  bounds: { width: number; height: number }
): { left: number; top: number } {
  let left = anchor.x + OFFSET_X
  let top = anchor.y - OFFSET_Y
  if (left + size.width > bounds.width) left = anchor.x - OFFSET_X - size.width
  if (top + size.height > bounds.height) top = anchor.y + OFFSET_Y - size.height
  left = Math.min(Math.max(0, left), Math.max(0, bounds.width - size.width))
  top = Math.min(Math.max(0, top), Math.max(0, bounds.height - size.height))
  return { left, top }
}

function closesMenu(item: ContextMenuItem): item is Extract<ContextMenuItem, { onClick?: () => void }> {
  return item.kind == null || item.kind === 'item' || item.kind === 'submenu' || item.kind === 'option'
}

export function AnchoredMenu({
  anchor,
  items,
  onClose,
  ignore,
  'aria-label': ariaLabel
}: {
  /** The point the menu opens from, in viewport px. */
  anchor: { x: number; y: number }
  items: ContextMenuItem[]
  onClose: () => void
  /** An element whose presses don't count as "outside" (the button that toggles the menu). */
  ignore?: Element | null
  'aria-label'?: string
}): React.JSX.Element {
  const ref = useRef<HTMLDivElement>(null)
  const onCloseRef = useRef(onClose)
  onCloseRef.current = onClose
  const ignoreRef = useRef(ignore)
  ignoreRef.current = ignore
  // Layout px, before the HUD scale.
  const [size, setSize] = useState({ width: 0, height: 0 })

  useLayoutEffect(() => {
    const node = ref.current
    if (node == null) return
    const next = { width: node.offsetWidth, height: node.offsetHeight }
    setSize((s) => (s.width === next.width && s.height === next.height ? s : next))
  })

  useEffect(() => {
    const onDown = (e: MouseEvent): void => {
      const target = e.target as Node
      if (ref.current?.contains(target) === true || ignoreRef.current?.contains(target) === true) return
      onCloseRef.current()
    }
    document.addEventListener('mousedown', onDown)
    const off = registerCancelLayer(() => onCloseRef.current())
    return () => {
      document.removeEventListener('mousedown', onDown)
      off()
    }
  }, [])

  // Focus returns to the opener only if the press that closed the menu didn't put it somewhere else.
  useEffect(() => {
    const node = ref.current
    const opener = ignoreRef.current ?? document.activeElement
    focusables(node)[0]?.focus()
    return () => {
      const active = document.activeElement
      const lost = active == null || active === document.body || node?.contains(active) === true
      if (lost && opener instanceof HTMLElement && opener.isConnected) opener.focus({ preventScroll: true })
    }
  }, [])

  const scale = getHudScale() || 1
  const { left, top } = anchoredMenuPosition(
    { x: anchor.x / scale, y: anchor.y / scale },
    size,
    { width: window.innerWidth / scale, height: window.innerHeight / scale }
  )

  const rows = items.map((item): ContextMenuItem => {
    if (!closesMenu(item)) return item
    const run = item.onClick
    return {
      ...item,
      onClick: () => {
        onClose()
        run?.()
      }
    }
  })

  return createPortal(
    <div
      ref={ref}
      className={styles.anchored}
      style={{ left: left * scale, top: top * scale }}
      onKeyDown={(e) => {
        if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return
        const list = focusables(ref.current)
        const at = list.indexOf(document.activeElement as HTMLElement)
        list[(at + (e.key === 'ArrowDown' ? 1 : list.length - 1)) % list.length]?.focus()
        e.preventDefault()
      }}
    >
      <ContextMenu items={rows} aria-label={ariaLabel} />
    </div>,
    document.body
  )
}

function focusables(node: HTMLElement | null): HTMLElement[] {
  return Array.from(node?.querySelectorAll<HTMLElement>('button:not(:disabled)') ?? [])
}

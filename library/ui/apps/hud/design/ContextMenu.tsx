// ContextMenu — a stacked menu surface (header, titles, separators, items,
// toggles) ported from the dcl-react-ui shared primitives.

import { useState, type ReactNode } from 'react'
import { Toggle } from './Toggle'
import { Submenu } from './Submenu'
import styles from './ContextMenu.module.css'

export type ContextMenuItem =
  | { kind: 'separator' }
  | { kind: 'title'; label: ReactNode }
  | { kind: 'caption'; label: ReactNode; avatar?: ReactNode }
  | { kind: 'header'; name: ReactNode; tag?: ReactNode; address?: string; avatar?: ReactNode }
  | { kind: 'toggle'; label: ReactNode; icon?: ReactNode; checked?: boolean; onChange?: (checked: boolean) => void }
  /** One of a set of exclusive choices (a radio): the checked one carries the mark. */
  | { kind: 'option'; label: ReactNode; icon?: ReactNode; checked: boolean; onClick?: () => void }
  /** A 'submenu' with `items` opens them beside it (Submenu); without, it only shows its chevron. */
  | { kind?: 'item' | 'submenu'; label: ReactNode; icon?: ReactNode; danger?: boolean; onClick?: () => void; items?: ContextMenuItem[] }

interface ContextMenuProps {
  items: ContextMenuItem[]
  'aria-label'?: string
  /** Scroll past a long list's maximum height (a submenu's list of names). */
  scroll?: boolean
}

function shortAddress(a: string): string {
  if (a.length <= 13) return a
  return `${a.slice(0, 6)}…${a.slice(-4)}`
}

export function ContextMenu({ items, scroll = false, 'aria-label': ariaLabel }: ContextMenuProps): React.JSX.Element {
  return (
    <div className={`${styles.ctx} ${scroll ? styles.scroll : ''}`.trim()} role="menu" aria-label={ariaLabel}>
      {items.map((it, i) => {
        if (it.kind === 'separator') return <div className={styles.sep} role="separator" key={i} />

        if (it.kind === 'title') {
          return (
            <div className={styles.title} key={i}>
              {it.label}
            </div>
          )
        }

        if (it.kind === 'caption') {
          return (
            <div className={styles.caption} key={i}>
              {it.avatar != null && it.avatar}
              <span className={styles.captionLabel}>{it.label}</span>
            </div>
          )
        }

        if (it.kind === 'header') {
          return (
            <div className={styles.header} key={i}>
              {it.avatar != null && it.avatar}
              <div className={styles.hinfo}>
                <div className={styles.hname}>
                  {it.name}
                  {it.tag != null && <span className={styles.htag}>{it.tag}</span>}
                </div>
                {it.address != null && <span className={styles.wallet}>{shortAddress(it.address)}</span>}
              </div>
            </div>
          )
        }

        if (it.kind === 'toggle') return <ToggleItem item={it} key={i} />

        if (it.kind === 'option') {
          return (
            <button
              type="button"
              className={`${styles.item} ${it.checked ? styles.itemChecked : ''}`.trim()}
              role="menuitemradio"
              aria-checked={it.checked}
              key={i}
              onClick={it.onClick}
            >
              {it.icon != null && <span className={styles.icon}>{it.icon}</span>}
              <span className={styles.label}>{it.label}</span>
              <span className={styles.check} aria-hidden="true">{it.checked ? '✓' : ''}</span>
            </button>
          )
        }

        if (it.kind === 'submenu' && it.items != null) {
          return (
            <Submenu key={i} label={it.label} icon={it.icon} className={styles.item}>
              <ContextMenu items={it.items} scroll />
            </Submenu>
          )
        }

        return (
          <button
            type="button"
            className={`${styles.item} ${it.danger ? styles.itemDanger : ''}`.trim()}
            role="menuitem"
            key={i}
            onClick={it.onClick}
          >
            {it.icon != null && <span className={styles.icon}>{it.icon}</span>}
            <span className={styles.label}>{it.label}</span>
            {it.kind === 'submenu' && <span className={styles.chev}>›</span>}
          </button>
        )
      })}
    </div>
  )
}

function ToggleItem({ item }: { item: Extract<ContextMenuItem, { kind: 'toggle' }> }): React.JSX.Element {
  const [on, setOn] = useState(!!item.checked)
  // A new `checked` from the owner wins (a write that failed flips the switch back).
  const [given, setGiven] = useState(item.checked)
  if (item.checked !== given) {
    setGiven(item.checked)
    setOn(!!item.checked)
  }
  const change = (next: boolean): void => {
    setOn(next)
    item.onChange?.(next)
  }
  const name = typeof item.label === 'string' ? item.label : undefined
  return (
    <label className={`${styles.item} ${styles.itemToggle}`} role="menuitemcheckbox" aria-checked={on} aria-label={name}>
      {item.icon != null && <span className={styles.icon}>{item.icon}</span>}
      <span className={styles.label}>{item.label}</span>
      <Toggle checked={on} onChange={change} aria-label={name} />
    </label>
  )
}

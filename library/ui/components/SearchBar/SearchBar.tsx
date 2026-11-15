import type { InputHTMLAttributes } from 'react'
import styles from './SearchBar.module.css'

export type SearchBarProps = {
  label: string
  value: string
  onValueChange: (value: string) => void
  /** Shows a clear button while the field has a value. */
  onClear?: () => void
  clearLabel?: string
  /** Class of the field itself; `className` styles the box around it. */
  inputClassName?: string
} & Omit<InputHTMLAttributes<HTMLInputElement>, 'value' | 'onChange' | 'type' | 'aria-label'>

/** A search field with an optional clear button, as decentraland.org's blog search draws it. */
export function SearchBar({
  label,
  value,
  onValueChange,
  onClear,
  clearLabel = 'Clear search',
  className,
  inputClassName,
  ...input
}: SearchBarProps) {
  return (
    <div className={[styles.root, className].filter(Boolean).join(' ')}>
      <input
        autoComplete="off"
        {...input}
        type="text"
        className={[styles.input, inputClassName].filter(Boolean).join(' ')}
        aria-label={label}
        value={value}
        onChange={(event) => onValueChange(event.target.value)}
      />
      {onClear && value && (
        <button type="button" className={styles.clear} onClick={onClear} aria-label={clearLabel}>
          ×
        </button>
      )}
    </div>
  )
}

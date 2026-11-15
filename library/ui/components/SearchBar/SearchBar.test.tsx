import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { useState } from 'react'
import { describe, expect, it, vi } from 'vitest'
import { SearchBar } from './SearchBar'

function Controlled({ onKeyDown }: { onKeyDown?: () => void }) {
  const [value, setValue] = useState('')
  return (
    <SearchBar
      label="Search the blog"
      placeholder="Search..."
      value={value}
      onValueChange={setValue}
      onClear={() => setValue('')}
      onKeyDown={onKeyDown}
    />
  )
}

describe('SearchBar', () => {
  describe('when empty', () => {
    it('shows a named text field with its placeholder and no clear button', () => {
      render(<Controlled />)
      const field = screen.getByRole('textbox', { name: 'Search the blog' })
      expect(field).toHaveAttribute('placeholder', 'Search...')
      expect(field).toHaveAttribute('autocomplete', 'off')
      expect(screen.queryByRole('button', { name: 'Clear search' })).not.toBeInTheDocument()
    })
  })

  describe('when the user types and clears', () => {
    it('reports the value and empties the field from the clear button', async () => {
      const onKeyDown = vi.fn()
      render(<Controlled onKeyDown={onKeyDown} />)
      const field = screen.getByRole('textbox', { name: 'Search the blog' })
      await userEvent.type(field, 'genesis')
      expect(field).toHaveValue('genesis')
      expect(onKeyDown).toHaveBeenCalled()
      await userEvent.click(screen.getByRole('button', { name: 'Clear search' }))
      expect(field).toHaveValue('')
    })
  })

  describe('when no clear handler is given', () => {
    it('never shows the clear button', () => {
      render(<SearchBar label="Search" value="x" onValueChange={() => {}} />)
      expect(screen.queryByRole('button')).not.toBeInTheDocument()
    })
  })
})

import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { Tabs } from './Tabs'

describe('Tabs', () => {
  describe('when it has an href', () => {
    it('renders a link tab that keeps the selected one in the tab order', () => {
      render(
        <div role="tablist" aria-label="Events">
          <Tabs label="All Events" href="/whats-on" selected variant="pill" />
          <Tabs label="My Events" href="/whats-on?tab=my" variant="pill" />
        </div>
      )
      const all = screen.getByRole('tab', { name: 'All Events' })
      const mine = screen.getByRole('tab', { name: 'My Events' })
      expect(all.tagName).toBe('A')
      expect(all).toHaveAttribute('href', '/whats-on')
      expect(all).toHaveAttribute('aria-selected', 'true')
      expect(all).toHaveAttribute('tabindex', '0')
      expect(mine).toHaveAttribute('aria-selected', 'false')
      expect(mine).toHaveAttribute('tabindex', '-1')
    })
  })

  describe('when it has no href', () => {
    it('renders a button tab that reports clicks', async () => {
      const onClick = vi.fn()
      render(<Tabs label="Overview" onClick={onClick} aria-controls="panel" />)
      const tab = screen.getByRole('tab', { name: 'Overview' })
      expect(tab.tagName).toBe('BUTTON')
      expect(tab).toHaveAttribute('type', 'button')
      expect(tab).toHaveAttribute('aria-controls', 'panel')
      await userEvent.click(tab)
      expect(onClick).toHaveBeenCalledOnce()
    })
  })

  describe('when a tab index is given', () => {
    it('keeps it', () => {
      render(<Tabs label="Overview" tabIndex={0} />)
      expect(screen.getByRole('tab')).toHaveAttribute('tabindex', '0')
    })
  })
})

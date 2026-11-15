import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { useState } from 'react'
import { describe, expect, it } from 'vitest'
import { Tabs } from '../Tabs/Tabs'
import { TabsBar } from './TabsBar'

function ButtonTabs() {
  const [value, setValue] = useState('all')
  return (
    <TabsBar label="Events" variant="pill">
      {['all', 'my', 'past'].map((id) => (
        <Tabs key={id} label={id} variant="pill" selected={value === id} onClick={() => setValue(id)} />
      ))}
    </TabsBar>
  )
}

describe('TabsBar', () => {
  describe('when rendered', () => {
    it('is a named tab list', () => {
      render(
        <TabsBar label="Events">
          <Tabs label="All" href="/a" selected />
        </TabsBar>
      )
      expect(screen.getByRole('tablist', { name: 'Events' })).toContainElement(screen.getByRole('tab'))
    })
  })

  describe('when the arrows move between button tabs', () => {
    it('focuses and selects the next tab, wrapping at the ends', async () => {
      render(<ButtonTabs />)
      await userEvent.tab()
      expect(screen.getByRole('tab', { name: 'all' })).toHaveFocus()
      await userEvent.keyboard('{ArrowRight}')
      expect(screen.getByRole('tab', { name: 'my' })).toHaveFocus()
      expect(screen.getByRole('tab', { name: 'my' })).toHaveAttribute('aria-selected', 'true')
      await userEvent.keyboard('{ArrowLeft}{ArrowLeft}')
      expect(screen.getByRole('tab', { name: 'past' })).toHaveFocus()
      await userEvent.keyboard('{Home}')
      expect(screen.getByRole('tab', { name: 'all' })).toHaveAttribute('aria-selected', 'true')
      await userEvent.keyboard('{End}')
      expect(screen.getByRole('tab', { name: 'past' })).toHaveFocus()
    })
  })

  describe('when the arrows move between link tabs', () => {
    it('moves focus only and leaves the selection to the link', async () => {
      render(
        <TabsBar label="Events">
          <Tabs label="All" href="#all" selected />
          <Tabs label="Mine" href="#mine" />
        </TabsBar>
      )
      await userEvent.tab()
      await userEvent.keyboard('{ArrowRight}')
      expect(screen.getByRole('tab', { name: 'Mine' })).toHaveFocus()
      expect(screen.getByRole('tab', { name: 'All' })).toHaveAttribute('aria-selected', 'true')
    })
  })
})

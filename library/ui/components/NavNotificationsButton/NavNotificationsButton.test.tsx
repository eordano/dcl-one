import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { NavNotificationsButton } from './NavNotificationsButton'

describe('NavNotificationsButton', () => {
  describe('when closed', () => {
    it('is a named button that announces a collapsed dialog', () => {
      render(<NavNotificationsButton />)
      const bell = screen.getByRole('button', { name: 'Notifications' })
      expect(bell).toHaveAttribute('type', 'button')
      expect(bell).toHaveAttribute('aria-haspopup', 'dialog')
      expect(bell).toHaveAttribute('aria-expanded', 'false')
    })
  })

  describe('when open', () => {
    it('announces the expanded dialog', () => {
      render(<NavNotificationsButton state="open" />)
      expect(screen.getByRole('button', { name: 'Notifications' })).toHaveAttribute(
        'aria-expanded',
        'true'
      )
    })
  })

  describe('when clicked', () => {
    it('calls the handler', () => {
      const onClick = vi.fn()
      render(<NavNotificationsButton onClick={onClick} />)
      fireEvent.click(screen.getByRole('button', { name: 'Notifications' }))
      expect(onClick).toHaveBeenCalledTimes(1)
    })
  })
})

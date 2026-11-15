import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { JumpInCTA } from './JumpInCTA'

describe('JumpInCTA', () => {
  describe('when rendered without a label', () => {
    it('reads "Jump in" and shows the icon hidden from assistive technology', () => {
      const { container } = render(<JumpInCTA />)
      expect(screen.getByRole('button', { name: 'Jump in' })).toHaveAttribute('type', 'button')
      expect(container.querySelector('svg.icon')).toHaveAttribute('aria-hidden', 'true')
    })

    it('applies the xl size by default', () => {
      render(<JumpInCTA />)
      expect(screen.getByRole('button')).toHaveClass('root', 'xl')
    })
  })

  describe('when given an href', () => {
    it('renders a link with the label in its own span', () => {
      render(<JumpInCTA href="/play/?position=0,0" label="JUMP IN" size="md" className="extra" />)
      const link = screen.getByRole('link', { name: 'JUMP IN' })
      expect(link).toHaveAttribute('href', '/play/?position=0,0')
      expect(link).toHaveClass('md', 'extra')
      expect(screen.getByText('JUMP IN').tagName).toBe('SPAN')
    })

    it('drops the href, leaves the tab order and blocks clicks when disabled', () => {
      const onClick = vi.fn()
      render(<JumpInCTA href="/play/" disabled onClick={onClick} />)
      const link = screen.getByText('Jump in').parentElement!
      expect(link).not.toHaveAttribute('href')
      expect(link).toHaveAttribute('aria-disabled', 'true')
      expect(link).toHaveAttribute('tabindex', '-1')
      fireEvent.click(link)
      expect(onClick).not.toHaveBeenCalled()
    })
  })

  describe('when loading', () => {
    it('replaces the icon with a spinner, marks it busy and blocks activation', () => {
      const onClick = vi.fn()
      const { container } = render(<JumpInCTA loading onClick={onClick} />)
      const button = screen.getByRole('button', { name: 'Jump in' })
      expect(button).toHaveAttribute('aria-busy', 'true')
      expect(container.querySelector('svg.icon')).toBeNull()
      expect(container.querySelector('.spinner')).toHaveAttribute('aria-hidden', 'true')
      fireEvent.click(button)
      expect(onClick).not.toHaveBeenCalled()
    })
  })

  describe('when disabled', () => {
    it('disables the native button', () => {
      render(<JumpInCTA disabled />)
      expect(screen.getByRole('button')).toBeDisabled()
    })
  })
})

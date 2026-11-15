import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { CardActionButton } from './CardActionButton'

describe('CardActionButton', () => {
  describe('when rendered with an accessible name', () => {
    it('renders a button named by aria-label with the default glyph hidden', () => {
      const { container } = render(<CardActionButton aria-label="Like" />)
      expect(screen.getByRole('button', { name: 'Like' })).toHaveAttribute('type', 'button')
      expect(container.querySelector('svg')).toHaveAttribute('aria-hidden', 'true')
    })

    it('renders a given icon instead of the default glyph', () => {
      render(<CardActionButton aria-label="Share" icon={<span data-testid="share" />} />)
      expect(screen.getByTestId('share')).toBeInTheDocument()
    })

    it('reports a pressed state and calls onClick', () => {
      const onClick = vi.fn()
      render(<CardActionButton aria-label="Like" aria-pressed="true" onClick={onClick} />)
      const button = screen.getByRole('button', { name: 'Like', pressed: true })
      fireEvent.click(button)
      expect(onClick).toHaveBeenCalledTimes(1)
    })
  })

  describe('when disabled', () => {
    it('does not call onClick', () => {
      const onClick = vi.fn()
      render(<CardActionButton aria-label="Like" disabled onClick={onClick} />)
      fireEvent.click(screen.getByRole('button'))
      expect(onClick).not.toHaveBeenCalled()
    })
  })
})

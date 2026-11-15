import { render } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { Skeleton } from './Skeleton'

describe('Skeleton', () => {
  describe('when rendered', () => {
    it('is hidden from assistive technology', () => {
      const { container } = render(<Skeleton />)
      expect(container.firstElementChild).toHaveAttribute('aria-hidden', 'true')
    })

    it('keeps the class and the style the consumer sizes it with', () => {
      const { container } = render(<Skeleton className="cover" style={{ width: 320 }} />)
      const node = container.firstElementChild as HTMLElement
      expect(node.className).toMatch(/cover/)
      expect(node.style.width).toBe('320px')
    })
  })

  describe('when the tone is neutral', () => {
    it('adds the neutral class and keeps the consumer class', () => {
      const { container } = render(<Skeleton tone="neutral" className="line" />)
      const node = container.firstElementChild as HTMLElement
      expect(node.className).toMatch(/neutral/)
      expect(node.className).toMatch(/line/)
    })
  })

  describe('when no tone is given', () => {
    it('does not add the neutral class', () => {
      const { container } = render(<Skeleton />)
      expect(container.firstElementChild?.className).not.toMatch(/neutral/)
    })
  })
})

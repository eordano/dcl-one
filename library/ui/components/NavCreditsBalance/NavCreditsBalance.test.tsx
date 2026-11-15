import { fireEvent, render, screen } from '@testing-library/react'
import type { ComponentPropsWithoutRef } from 'react'
import { describe, expect, it, vi } from 'vitest'
import { NavCreditsBalance } from './NavCreditsBalance'

describe('NavCreditsBalance', () => {
  describe('when rendered with a balance', () => {
    it('links the balance to its page with the mark hidden from assistive technology', () => {
      render(<NavCreditsBalance label="12,345" href="/shop/credits" aria-label="12,345 credits" />)
      const link = screen.getByRole('link', { name: '12,345 credits' })
      expect(link).toHaveAttribute('href', '/shop/credits')
      expect(link).toHaveTextContent('12,345')
      expect(link.querySelector('svg')).toHaveAttribute('aria-hidden', 'true')
    })
  })

  describe('when given the app router link', () => {
    it('renders through it, keeps the caller class and forwards the click', () => {
      const onClick = vi.fn()
      const RouterLink = (props: ComponentPropsWithoutRef<'a'>) => <a data-router="" {...props} />
      render(
        <NavCreditsBalance
          as={RouterLink}
          label="0"
          href="/shop/credits"
          className="hook"
          onClick={onClick}
        />
      )
      const link = screen.getByRole('link', { name: '0' })
      expect(link).toHaveAttribute('data-router')
      expect(link).toHaveClass('root', 'hook')
      fireEvent.click(link)
      expect(onClick).toHaveBeenCalledTimes(1)
    })
  })
})

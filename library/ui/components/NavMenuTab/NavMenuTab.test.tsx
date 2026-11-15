import { fireEvent, render, screen } from '@testing-library/react'
import type { ComponentPropsWithoutRef } from 'react'
import { describe, expect, it, vi } from 'vitest'
import { NavMenuTab } from './NavMenuTab'

describe('NavMenuTab', () => {
  describe('when rendered as a plain tab', () => {
    it('is a link to its section without menu attributes', () => {
      render(<NavMenuTab label="Shop" href="/shop" />)
      const link = screen.getByRole('link', { name: 'Shop' })
      expect(link).toHaveAttribute('href', '/shop')
      expect(link).not.toHaveAttribute('aria-current')
      expect(link).not.toHaveAttribute('aria-haspopup')
      expect(link).not.toHaveAttribute('aria-expanded')
    })
  })

  describe('when it is the active section', () => {
    it('marks the current page and keeps the caller class', () => {
      render(<NavMenuTab label="Create" href="/create" active className="hook" />)
      const link = screen.getByRole('link', { name: 'Create' })
      expect(link).toHaveAttribute('aria-current', 'page')
      expect(link).toHaveClass('root', 'active', 'hook')
    })
  })

  describe('when it opens a menu', () => {
    it('announces the menu and its state', () => {
      const { rerender } = render(<NavMenuTab label="Learn" href="/blog" hasMenu />)
      const link = screen.getByRole('link', { name: 'Learn' })
      expect(link).toHaveAttribute('aria-haspopup', 'menu')
      expect(link).toHaveAttribute('aria-expanded', 'false')
      rerender(<NavMenuTab label="Learn" href="/blog" hasMenu expanded />)
      expect(link).toHaveAttribute('aria-expanded', 'true')
    })

    it('names the menu the caller passes as aria-controls', () => {
      render(<NavMenuTab label="Learn" href="/blog" hasMenu aria-controls="learn-menu" />)
      expect(screen.getByRole('link', { name: 'Learn' })).toHaveAttribute(
        'aria-controls',
        'learn-menu'
      )
    })
  })

  describe('when given the app router link', () => {
    it('renders through it and forwards the click', () => {
      const onClick = vi.fn()
      const RouterLink = (props: ComponentPropsWithoutRef<'a'>) => <a data-router="" {...props} />
      render(<NavMenuTab as={RouterLink} label="Play" href="/play/" onClick={onClick} />)
      const link = screen.getByRole('link', { name: 'Play' })
      expect(link).toHaveAttribute('data-router')
      fireEvent.click(link)
      expect(onClick).toHaveBeenCalledTimes(1)
    })
  })
})

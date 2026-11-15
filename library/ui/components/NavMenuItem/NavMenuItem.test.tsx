import { render, screen } from '@testing-library/react'
import type { ComponentPropsWithoutRef } from 'react'
import { describe, expect, it } from 'vitest'
import { NavMenuItem } from './NavMenuItem'

describe('NavMenuItem', () => {
  describe('when rendered with a destination', () => {
    it('is a menu item that links to it', () => {
      render(<NavMenuItem label="Docs" href="/docs/" />)
      const item = screen.getByRole('menuitem', { name: 'Docs' })
      expect(item).toHaveAttribute('href', '/docs/')
      expect(item).toHaveClass('root')
    })
  })

  describe('when the caller shows the links as a plain list', () => {
    it('is a link when the role is cleared', () => {
      render(<NavMenuItem label="Docs" href="/docs/" role={undefined} />)
      expect(screen.getByRole('link', { name: 'Docs' })).toBeInTheDocument()
      expect(screen.queryByRole('menuitem')).not.toBeInTheDocument()
    })
  })

  describe('when given the app router link and a class', () => {
    it('renders through the link and keeps both classes', () => {
      const RouterLink = (props: ComponentPropsWithoutRef<'a'>) => <a data-router="" {...props} />
      render(<NavMenuItem as={RouterLink} label="Blog" href="/blog" className="hook" />)
      const item = screen.getByRole('menuitem', { name: 'Blog' })
      expect(item).toHaveAttribute('data-router')
      expect(item).toHaveClass('root', 'hook')
    })
  })
})

import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { PlaceCard, type PlaceCardLinkProps } from './PlaceCard'

const place = {
  title: 'Genesis Plaza',
  href: '/places/genesis',
  jumpHref: 'decentraland://?position=0,0',
  image: 'https://example.org/cover "1".png',
  coords: '0,0',
  players: 12
}
const setup = (props: Parameters<typeof PlaceCard>[0] = {}) => render(<PlaceCard {...place} {...props} />)

describe('PlaceCard', () => {
  describe('when rendered with a place', () => {
    it('links the title to the place page', () => {
      setup()
      expect(screen.getByRole('link', { name: 'Genesis Plaza' })).toHaveAttribute(
        'href',
        '/places/genesis'
      )
    })

    it('links JUMP IN to the Explorer deep link', () => {
      setup()
      expect(screen.getByRole('link', { name: 'Jump in' })).toHaveAttribute(
        'href',
        'decentraland://?position=0,0'
      )
    })

    it('shows the presence count and the coordinates', () => {
      setup()
      expect(screen.getByText('12')).toBeInTheDocument()
      expect(screen.getByText('0,0')).toBeInTheDocument()
      expect(screen.getByTestId('PermIdentityRoundedIcon')).toBeInTheDocument()
    })

    it('paints the cover with the image URL escaped', () => {
      const { container } = setup()
      const cover = container.querySelector<HTMLElement>('[style*="--cover"]')
      expect(cover?.style.getPropertyValue('--cover')).toBe(
        'url("https://example.org/cover %221%22.png")'
      )
    })

    it('escapes every character that ends a CSS string', () => {
      const { container } = setup({ image: 'https://example.org/a\nb\rc\fd\\e.png' })
      const cover = container.querySelector<HTMLElement>('[style*="--cover"]')
      expect(cover?.style.getPropertyValue('--cover')).toBe(
        'url("https://example.org/a%0Ab%0Dc%0Cd%5Ce.png")'
      )
    })
  })

  describe('when nobody is in the place', () => {
    it('hides the presence count', () => {
      setup({ players: 0 })
      expect(screen.queryByTestId('PermIdentityRoundedIcon')).not.toBeInTheDocument()
    })
  })

  describe('when the place is live and featured', () => {
    it('shows LIVE with the event name and the Featured badge', () => {
      setup({ live: true, liveEventName: 'Party', featured: true })
      expect(screen.getByText('LIVE')).toHaveAttribute('title', 'Party')
      expect(screen.getByText('Featured')).toBeInTheDocument()
    })
  })

  describe('when the labels are given', () => {
    it('shows them in place of LIVE and By', () => {
      setup({ live: true, liveLabel: 'EN VIVO', byLabel: 'Por', creator: { name: 'Ana' } })
      expect(screen.getByText('EN VIVO')).toBeInTheDocument()
      expect(screen.queryByText('LIVE')).not.toBeInTheDocument()
      expect(screen.getByText(/^Por/)).toBeInTheDocument()
      expect(screen.queryByText(/^By/)).not.toBeInTheDocument()
    })
  })

  describe('when the creator has a profile', () => {
    it('links the creator name to the profile', () => {
      setup({ creator: { name: 'Ana', profileHref: '/profile/0xabc' } })
      expect(screen.getByRole('link', { name: 'Ana' })).toHaveAttribute('href', '/profile/0xabc')
      expect(screen.getByText(/By/)).toBeInTheDocument()
    })
  })

  describe('when the creator has no profile', () => {
    it('shows the name as text', () => {
      setup({ creator: { name: 'Ana' } })
      expect(screen.queryByRole('link', { name: 'Ana' })).not.toBeInTheDocument()
      expect(screen.getByText('Ana')).toBeInTheDocument()
    })
  })

  describe('when the place page and the deep link are not given', () => {
    it.each(['grid', 'live'] as const)('shows the %s title as text and no JUMP IN', (variant) => {
      render(<PlaceCard variant={variant} title="Genesis Plaza" />)
      expect(screen.getByText('Genesis Plaza')).toBeInTheDocument()
      expect(screen.queryByRole('link')).not.toBeInTheDocument()
      expect(screen.queryByText('Jump in')).not.toBeInTheDocument()
    })
  })

  describe('when a router link is passed', () => {
    it('renders the title with it and reports the open', () => {
      const onOpen = vi.fn()
      const Link = ({ to, className, onClick, children }: PlaceCardLinkProps) => (
        <a
          data-router="yes"
          className={className}
          href={to}
          onClick={(event) => {
            event.preventDefault()
            onClick?.()
          }}
        >
          {children}
        </a>
      )
      setup({ Link, onOpen })
      const title = screen.getByRole('link', { name: 'Genesis Plaza' })
      expect(title).toHaveAttribute('data-router', 'yes')
      fireEvent.click(title)
      expect(onOpen).toHaveBeenCalledTimes(1)
    })
  })

  describe('when the title link has keyboard focus', () => {
    it('keeps JUMP IN reachable after it in the tab order', () => {
      setup()
      const links = screen.getAllByRole('link').map((link) => link.textContent)
      expect(links).toEqual(['Genesis Plaza', 'Jump in'])
    })
  })

  describe('when rendered as the live rail card', () => {
    it('links the title and JUMP IN and leaves the coordinates out', () => {
      setup({ variant: 'live', creator: { name: 'Ana' } })
      expect(screen.getByRole('link', { name: 'Genesis Plaza' })).toHaveAttribute(
        'href',
        '/places/genesis'
      )
      expect(screen.getByRole('link', { name: 'Jump in' })).toBeInTheDocument()
      expect(screen.queryByText('0,0')).not.toBeInTheDocument()
      expect(screen.getByText('Ana')).toBeInTheDocument()
    })

    it('shows the presence count when asked even with nobody in', () => {
      setup({ variant: 'live', players: 0, showPlayers: true })
      expect(screen.getByTestId('PermIdentityRoundedIcon')).toBeInTheDocument()
    })

    it('paints the cover on the media box', () => {
      const { container } = setup({ variant: 'live' })
      expect(container.querySelectorAll('[style*="--cover"]')).toHaveLength(1)
    })
  })

  describe('when loading', () => {
    it('marks the card busy and renders no link', () => {
      const { container } = render(<PlaceCard loading />)
      expect(container.firstElementChild).toHaveAttribute('aria-busy', 'true')
      expect(screen.queryByRole('link')).not.toBeInTheDocument()
    })

    it('names the loading card for assistive technology', () => {
      const { rerender } = render(<PlaceCard loading />)
      expect(screen.getByRole('progressbar', { name: 'Loading' })).toHaveAttribute('aria-busy', 'true')
      rerender(<PlaceCard loading loadingLabel="Loading place" />)
      expect(screen.getByRole('progressbar', { name: 'Loading place' })).toBeInTheDocument()
    })
  })
})

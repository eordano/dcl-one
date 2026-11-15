import { fireEvent, render } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { NavProfileButton } from './NavProfileButton'

describe('NavProfileButton', () => {
  describe('when the account has a face picture', () => {
    it('shows the picture and reports a load failure', () => {
      const onImageError = vi.fn()
      const { container } = render(
        <NavProfileButton imageUrl="/face.png" fallbackSrc="/logo.svg" onImageError={onImageError} />
      )
      const img = container.querySelector('img')!
      expect(img).toHaveAttribute('src', '/face.png')
      expect(img).toHaveAttribute('width', '48')
      expect(img).toHaveClass('image')
      expect((container.firstElementChild as HTMLElement).style.getPropertyValue('--nav-profile-color')).toBe('')
      fireEvent.error(img)
      expect(onImageError).toHaveBeenCalledTimes(1)
    })
  })

  describe('when the account has no face picture', () => {
    it('shows the fallback mark on the coloured disc', () => {
      const { container } = render(
        <NavProfileButton fallbackSrc="/logo.svg" color="rgb(1, 2, 3)" data-testid="disc" />
      )
      const disc = container.firstElementChild as HTMLElement
      expect(disc).toHaveAttribute('data-testid', 'disc')
      expect(disc.style.getPropertyValue('--nav-profile-color')).toBe('rgb(1, 2, 3)')
      const img = container.querySelector('img')!
      expect(img).toHaveAttribute('src', '/logo.svg')
      expect(img).toHaveAttribute('alt', '')
      expect(img).toHaveClass('fallback')
    })
  })
})

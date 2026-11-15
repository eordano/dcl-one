import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest'
import { EmailConfirmation, type EmailConfirmationProps } from './EmailConfirmation'

describe('EmailConfirmation', () => {
  let props: EmailConfirmationProps

  afterEach(() => {
    vi.resetAllMocks()
  })

  describe('when the link is invalid', () => {
    beforeEach(() => {
      props = {
        source: 'account',
        state: {
          kind: 'invalid',
          title: 'Missing Address',
          message: 'Please check your email link.'
        }
      }
      render(<EmailConfirmation {...props} />)
    })

    it('should show the refusal as the page heading', () => {
      expect(screen.getByRole('heading', { level: 1, name: 'Missing Address' })).toBeInTheDocument()
    })

    it('should offer no confirm button', () => {
      expect(screen.queryByRole('button', { name: 'Confirm Email' })).not.toBeInTheDocument()
    })
  })

  describe('when the challenge is shown', () => {
    let onConfirm: Mock<() => void>

    beforeEach(() => {
      onConfirm = vi.fn()
    })

    describe('and the challenge is solved', () => {
      beforeEach(() => {
        props = {
          source: 'credits',
          state: { kind: 'challenge', pending: false, canConfirm: true },
          challenge: <div data-testid="challenge" />,
          onConfirm
        }
        render(<EmailConfirmation {...props} />)
      })

      it('should render the challenge widget', () => {
        expect(screen.getByTestId('challenge')).toBeInTheDocument()
      })

      it('should confirm on click', async () => {
        await userEvent.click(screen.getByRole('button', { name: 'Confirm Email' }))
        expect(onConfirm).toHaveBeenCalledTimes(1)
      })
    })

    describe('and the challenge is not solved yet', () => {
      beforeEach(() => {
        props = {
          source: 'account',
          state: { kind: 'challenge', pending: false, canConfirm: false },
          challenge: <div />,
          onConfirm
        }
        render(<EmailConfirmation {...props} />)
      })

      it('should disable the confirm button', () => {
        expect(screen.getByRole('button', { name: 'Confirm Email' })).toBeDisabled()
      })
    })

    describe('and the request is pending', () => {
      beforeEach(() => {
        props = {
          source: 'account',
          state: { kind: 'challenge', pending: true, canConfirm: true },
          onConfirm
        }
        render(<EmailConfirmation {...props} />)
      })

      it('should show a disabled, busy confirm button', () => {
        expect(screen.getByRole('button', { name: 'Confirming...' })).toBeDisabled()
      })
    })

    describe('and a retryable refusal came back', () => {
      beforeEach(() => {
        props = {
          source: 'account',
          state: {
            kind: 'challenge',
            pending: false,
            canConfirm: false,
            error: 'The security check did not pass.'
          },
          onConfirm
        }
        render(<EmailConfirmation {...props} />)
      })

      it('should announce the refusal', () => {
        expect(screen.getByRole('alert')).toHaveTextContent('The security check did not pass.')
      })
    })
  })

  describe('when the email is confirmed', () => {
    describe('and the link came from the credits flow', () => {
      beforeEach(() => {
        props = { source: 'credits', state: { kind: 'confirmed', continueHref: '/shop' } }
        render(<EmailConfirmation {...props} />)
      })

      it('should link to the Shop', () => {
        expect(screen.getByRole('link', { name: 'Go to Shop' })).toHaveAttribute('href', '/shop')
      })
    })

    describe('and the link came from the account flow', () => {
      beforeEach(() => {
        props = { source: 'account', state: { kind: 'confirmed', continueHref: '/' } }
        render(<EmailConfirmation {...props} />)
      })

      it('should link back to Decentraland', () => {
        expect(screen.getByRole('link', { name: 'Go to Decentraland' })).toHaveAttribute(
          'href',
          '/'
        )
      })
    })
  })

  describe('when the confirmation failed for good', () => {
    beforeEach(() => {
      props = {
        source: 'account',
        state: { kind: 'failed', message: 'This link is invalid.', homeHref: '/' }
      }
      render(<EmailConfirmation {...props} />)
    })

    it('should announce the reason', () => {
      expect(screen.getByRole('alert')).toHaveTextContent('This link is invalid.')
    })
  })
})

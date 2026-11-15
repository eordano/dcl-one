// The readable names of the generated text styles (foundations/typography/typography.ts) and the
// CSS Module class of each, for `className={textClasses.headingXl}` in the web apps.
// test/text-styles.test.ts fails when an alias has no rule in the generated sheet or a generated style has
// no alias.
import classes from './foundations/typography/text-styles.module.css'
import type { TextStyleName } from './foundations/typography/typography'

export const textStyleNames = {
  heading1: 'heading/1',
  heading2: 'heading/2',
  heading3: 'heading/3',
  heading4: 'heading/4',
  heading5: 'heading/5',
  headingXl: 'heading/xl',
  headingLg: 'heading/lg',
  bodyLg: 'body/lg',
  bodyMd: 'body/md',
  bodyMdStrong: 'body/md-strong',
  bodySm: 'body/sm',
  bodySmStrong: 'body/sm-strong',
  bodyXs: 'body/xs',
  buttonXl: 'button/xl',
  buttonLg: 'button/lg',
  buttonMd: 'button/md',
  buttonSm: 'button/sm',
  labelMdCaps: 'label/md-caps',
  labelSmCaps: 'label/sm-caps',
  labelXsCaps: 'label/xs-caps',
  labelTag: 'label/tag',
  caption: 'caption',
  explorerNavLabel: 'explorer/nav-label',
  explorerBalance: 'explorer/balance',
  explorerButton: 'explorer/button',
  explorerButtonLg: 'explorer/button-lg',
  explorerBadge: 'explorer/badge',
  explorerBadgeSm: 'explorer/badge-sm',
  explorerTitle: 'explorer/title',
  explorerLabelCaps: 'explorer/label-caps',
  explorerInput: 'explorer/input',
  explorerCaptionCaps: 'explorer/caption-caps'
} as const satisfies Record<string, TextStyleName>

export type TextClassName = keyof typeof textStyleNames

/** The generated class of a text style: `text` followed by the style name in hex, as the generator writes it. */
export function generatedTextClass(name: TextStyleName): string {
  const hex = Array.from(new TextEncoder().encode(name), (byte) =>
    byte.toString(16).padStart(2, '0')
  )
  return `text${hex.join('')}`
}

export const textClasses = Object.fromEntries(
  Object.entries(textStyleNames).map(([readable, name]) => {
    const generated = generatedTextClass(name)
    return [readable, (classes as Record<string, string>)[generated] ?? generated]
  })
) as Record<TextClassName, string>

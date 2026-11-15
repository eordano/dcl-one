import { readFileSync } from 'node:fs'
import path from 'node:path'
import { describe, expect, it } from 'vitest'
import { textStyles } from '../foundations/typography/typography'
import { generatedTextClass, textClasses, textStyleNames } from '../text-styles'

const sheet = readFileSync(
  path.resolve(import.meta.dirname, '../foundations/typography/text-styles.module.css'),
  'utf8'
)

describe('textClasses', () => {
  describe('when compared with the generated text styles', () => {
    it('names every generated style', () => {
      expect(Object.values(textStyleNames).sort()).toEqual(Object.keys(textStyles).sort())
    })
  })

  describe.each(Object.entries(textStyleNames))('when the alias is %s', (alias, name) => {
    it('resolves to a class that the generated sheet styles', () => {
      const generated = generatedTextClass(name)
      expect(sheet).toContain(`:where(.${generated})`)
      expect(textClasses[alias as keyof typeof textClasses]).toBe(generated)
    })
  })
})

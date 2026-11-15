// url=<DCL_FIGMA_FILE>?node-id=15724-536
// source=components/JumpInCTA/JumpInCTA.tsx
// component=JumpInCTA
// contract=1
// states=hover,disabled,loading
import figma from 'figma'
const instance = figma.selectedInstance

const size = instance.getEnum('size', { xl: 'xl', lg: 'lg', md: 'md', sm: 'sm' })
const state = instance.getEnum('state', {
  default: '',
  hover: '',
  disabled: ' disabled',
  loading: ' loading'
})
const labelLayer = instance.findText('label')
const label = labelLayer.type === 'TEXT' ? labelLayer.textContent : ''

export default {
  example: figma.tsx`<JumpInCTA size="${size}"${state} label="${label}" href={href} />`,
  imports: ["import { JumpInCTA } from '@dcl/one-ui'"],
  id: 'jump-in-cta',
  metadata: { nestable: true }
}

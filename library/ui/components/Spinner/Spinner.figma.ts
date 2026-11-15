// url=<DCL_FIGMA_FILE>?node-id=4275-1730
// source=components/Spinner/Spinner.tsx
// component=Spinner
// contract=1
// states=step
import figma from 'figma'
const instance = figma.selectedInstance

const variant = instance.getEnum('Property 1', { Dark: 'dark', Light: 'light' })

export default {
  example: figma.tsx`<Spinner variant="${variant}" />`,
  imports: ["import { Spinner } from '@dcl/one-ui'"],
  id: 'spinner'
}

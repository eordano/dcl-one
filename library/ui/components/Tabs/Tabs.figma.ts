// url=<DCL_FIGMA_FILE>?node-id=15674-34262
// source=components/Tabs/Tabs.tsx
// component=Tabs
// contract=1
// states=Status,Device
import figma from 'figma'
const instance = figma.selectedInstance

const selected = instance.getEnum('Status', { Selected: ' selected', Default: '' })
const device = instance.getEnum('Device', { Desktop: 'desktop', Mobile: 'mobile' })
const labelLayer = instance.findText('Overview')
const label = labelLayer.type === 'TEXT' ? labelLayer.textContent : ''

export default {
  example: figma.tsx`<Tabs label="${label}" device="${device}"${selected} />`,
  imports: ["import { Tabs } from '@dcl/one-ui'"],
  id: 'tabs',
  metadata: { nestable: true }
}

// url=<DCL_FIGMA_FILE>?node-id=15808-46232
// source=components/TabsBar/TabsBar.tsx
// component=TabsBar
// contract=1
// states=page,viewport
import figma from 'figma'
const instance = figma.selectedInstance

const page = instance.getEnum('page', { create: 'Create', shop: 'Shop', learn: 'Learn' })

export default {
  example: figma.tsx`<TabsBar label="${page}">{tabs}</TabsBar>`,
  imports: ["import { TabsBar, Tabs } from '@dcl/one-ui'"],
  id: 'tabs-bar'
}

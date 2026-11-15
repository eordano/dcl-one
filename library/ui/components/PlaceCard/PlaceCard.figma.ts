// url=<DCL_FIGMA_FILE>?node-id=16189-2716
// source=components/PlaceCard/PlaceCard.tsx
// component=PlaceCard
// contract=1
// states=Hover,loading
import figma from 'figma'
const instance = figma.selectedInstance

const titleLayer = instance.findText('Place name')
const title = titleLayer.type === 'TEXT' ? titleLayer.textContent : ''
const coordsLayer = instance.findText('0,0')
const coords = coordsLayer.type === 'TEXT' ? coordsLayer.textContent : ''

export default {
  example: figma.tsx`<PlaceCard title="${title}" coords="${coords}" href={href} jumpHref={jumpHref} image={image} players={players} creator={creator} />`,
  imports: ["import { PlaceCard } from '@dcl/one-ui'"],
  id: 'place-card'
}

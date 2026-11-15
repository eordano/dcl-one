// url=<DCL_FIGMA_FILE>?node-id=3959-1973
// source=components/SearchBar/SearchBar.tsx
// component=SearchBar
// contract=1
// states=state,product
import figma from 'figma'
const instance = figma.selectedInstance

const placeholderLayer = instance.findText('Search item')
const placeholder = placeholderLayer.type === 'TEXT' ? placeholderLayer.textContent : ''

export default {
  example: figma.tsx`<SearchBar label="Search" placeholder="${placeholder}" value={value} onValueChange={setValue} onClear={clear} />`,
  imports: ["import { SearchBar } from '@dcl/one-ui'"],
  id: 'search-bar'
}

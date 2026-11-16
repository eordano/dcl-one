// The reel is read unsigned, so even your own passport lists only PUBLIC photos.

import { useCallback, useState } from 'react'
import { EmptyState, Spinner } from '../../design'
import type { GalleryPhotoMeta } from '@explorer-core/engine/protocol'
import type { ReelWrite } from '../session/useEngineSession'
import type { ChatUser } from '../chat/ProfileCardPresentation'
import { PhotoDetail } from './PhotoDetail'
import { fetchPublicReelPage } from './publicReels'
import { ReelGrid, type ReelGridLayout, type ReelGridMenu } from './ReelGrid'
import { useReelList } from './useReelList'
import { useOwnReelActions, type ReelWrites } from './useOwnReelActions'
import styles from './PassportPhotos.module.css'

// The passport panel is narrow, so four tiles share its width and keep a 272:201 shape.
const PASSPORT_LAYOUT: ReelGridLayout = {
  columns: 4,
  tileWidth: 272,
  tileHeight: 201,
  fitWidth: true,
  groupByMonth: false,
  paddingTop: 2,
  paddingBottom: 12,
  paddingLeft: 0,
  monthSpacing: 20,
  optionButtonHeight: 40
}

export interface PassportPhotoActions {
  setPublic: (id: string, isPublic: boolean) => Promise<ReelWrite>
  remove: (id: string) => Promise<ReelWrite>
  metas: Record<string, GalleryPhotoMeta | null | undefined>
  loadPhoto: (id: string) => void
  onTeleport: (x: number, y: number) => void
  onViewPerson?: (user: ChatUser) => void
}

export function PassportPhotos({ address, isSelf, actions }: { address: string; isSelf: boolean; actions?: PassportPhotoActions }): React.JSX.Element {
  const key = address.toLowerCase()
  const fetchPage = useCallback((offset: number, limit: number) => fetchPublicReelPage(key, offset, limit), [key])
  const list = useReelList(key, fetchPage)
  const [index, setIndex] = useState<number | null>(null)
  const writes = useOwnReelActions(actions ?? NO_WRITES, list, { publicOnly: true })
  const menu: ReelGridMenu | null =
    isSelf && actions != null ? { showDelete: true, onSetPublic: writes.setPublic, onDelete: (photo) => void writes.remove(photo) } : null

  const selected = index != null ? list.photos[index] : null

  return (
    <section className={styles.section}>
      {!list.loaded ? (
        <div className={styles.center}>
          <Spinner size={34} />
        </div>
      ) : list.error && list.photos.length === 0 ? (
        <EmptyState variant="inline" tone="error" title="Photos couldn't be loaded" actions={[{ label: 'RETRY', onClick: list.retry }]} />
      ) : list.photos.length === 0 ? (
        <EmptyState
          variant="inline"
          title="No photos here yet!"
          subtitle="✨ Set your photos as public to make them visible in your Profile & Communities ✨"
        />
      ) : (
        <ReelGrid
          photos={list.photos}
          layout={PASSPORT_LAYOUT}
          menu={menu}
          onOpen={setIndex}
          onVisibleIndex={list.loadMore}
          onRetry={list.error ? list.retry : undefined}
          aria-label="Photos"
        />
      )}
      {selected != null && index != null && (
        <PhotoDetail
          photos={list.photos}
          index={index}
          meta={actions?.metas[selected.id]}
          isSelf={isSelf}
          onIndex={(i) => {
            setIndex(i)
            list.loadMore(i)
          }}
          onLoadMeta={actions?.loadPhoto ?? noop}
          onClose={() => setIndex(null)}
          onTeleport={(x, y) => {
            setIndex(null)
            actions?.onTeleport(x, y)
          }}
          onDelete={
            menu != null
              ? (photo) =>
                  void writes.remove(photo).then((removed) => {
                    if (removed) setIndex(null)
                  })
              : undefined
          }
          onViewPerson={actions?.onViewPerson}
        />
      )}
    </section>
  )
}

const noop = (): void => {}
// Never called: without `actions` there is no menu.
const NO_WRITES: ReelWrites = { setPublic: async () => ({ ok: false }), remove: async () => ({ ok: false }) }

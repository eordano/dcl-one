import { useState } from 'react'
import { EmptyState, Icon, Spinner } from '../../design'
import { MainMenuShell } from '../menu/MainMenuShell'
import { PhotoDetail } from './PhotoDetail'
import { ReelGrid, type ReelGridLayout, type ReelGridMenu } from './ReelGrid'
import { useReelList } from './useReelList'
import { useOwnReelActions } from './useOwnReelActions'
import type { GalleryState, ProfileState } from '../session/useEngineSession'
import type { ChatUser } from '../chat/ProfileCardPresentation'
import styles from './GalleryPage.module.css'

// Six columns of 306x230 fill the 1920 canvas with its 30 px padding.
const GALLERY_LAYOUT: ReelGridLayout = {
  columns: 6,
  tileWidth: 306,
  tileHeight: 230,
  groupByMonth: true,
  paddingTop: 30,
  paddingBottom: 30,
  paddingLeft: 30,
  monthSpacing: 20,
  optionButtonHeight: 41
}

export function GalleryPage({
  gallery,
  profile,
  onNavigate,
  onTeleport,
  onViewProfile
}: {
  gallery: GalleryState
  profile: ProfileState
  onNavigate: (page: string) => void
  /** Jump to a photo's parcel (closes the gallery). */
  onTeleport: (x: number, y: number) => void
  /** Open a captured person's passport. */
  onViewProfile?: (user: ChatUser) => void
}): React.JSX.Element | null {
  const [index, setIndex] = useState<number | null>(null)
  // The page stays mounted while closed, so each open resets it and reads the reel from the top:
  // photos taken since the last open show up.
  const [opened, setOpened] = useState(0)
  const [wasOpen, setWasOpen] = useState(gallery.open)
  if (gallery.open !== wasOpen) {
    setWasOpen(gallery.open)
    if (gallery.open) {
      setOpened((n) => n + 1)
      setIndex(null)
    }
  }
  const list = useReelList(gallery.open ? `own:${opened}` : null, gallery.fetchPage)
  const actions = useOwnReelActions(gallery, list)
  const menu: ReelGridMenu = { showDelete: true, onSetPublic: actions.setPublic, onDelete: (photo) => void actions.remove(photo) }

  if (!gallery.open) return null

  const p = profile.data
  const selected = index != null ? list.photos[index] : null
  const pct = list.max > 0 ? Math.min(100, (list.total / list.max) * 100) : 0

  return (
    <MainMenuShell
      active="gallery"
      profileName={p?.name}
      profilePicture={p?.picture}
      profileAddress={p?.address}
      profileClaimed={p?.hasClaimedName}
      onNavigate={onNavigate}
      onClose={gallery.toggle}
      flushBody
    >
      <div className={styles.head}>
        <h1 className={styles.title}>Gallery</h1>
        {list.loaded && list.max > 0 && (
          <div className={styles.storage} title={`${list.total} of ${list.max} photos used`}>
            <div className={styles.storageBar}>
              <div className={styles.storageFill} style={{ '--fill': `${pct}%` } as React.CSSProperties} />
            </div>
            <span className={styles.storageText}>
              Storage {list.total}/{list.max} photos taken
            </span>
          </div>
        )}
      </div>

      {!list.loaded ? (
        <div className={styles.center}>
          <Spinner />
        </div>
      ) : list.error && list.photos.length === 0 ? (
        <EmptyState
          variant="screen"
          tone="error"
          title="Your photos couldn't be loaded"
          subtitle="Check your connection and try again."
          actions={[{ label: 'RETRY', onClick: list.retry }]}
        />
      ) : list.photos.length === 0 ? (
        <EmptyState
          variant="screen"
          iconWash
          icon={<Icon name="gallery" size={56} />}
          title="There are no photos yet"
          subtitle={
            <span className={styles.emptyBody}>
              Use the <span className={styles.emptyCamera}>camera</span> to save incredible memories with your friends!
            </span>
          }
        />
      ) : (
        <ReelGrid
          photos={list.photos}
          layout={GALLERY_LAYOUT}
          menu={menu}
          onOpen={setIndex}
          onVisibleIndex={list.loadMore}
          onRetry={list.error ? list.retry : undefined}
          aria-label="Photos"
        />
      )}

      {selected && index != null && (
        <PhotoDetail
          photos={list.photos}
          index={index}
          meta={gallery.metas[selected.id]}
          isSelf
          onIndex={(i) => {
            setIndex(i)
            list.loadMore(i)
          }}
          onLoadMeta={gallery.loadPhoto}
          onClose={() => setIndex(null)}
          onTeleport={(x, y) => {
            setIndex(null)
            gallery.toggle() // close the gallery so the teleport lands in-world
            onTeleport(x, y)
          }}
          onDelete={(photo) =>
            void actions.remove(photo).then((removed) => {
              if (removed) setIndex(null)
            })
          }
          onViewPerson={onViewProfile}
        />
      )}
    </MainMenuShell>
  )
}

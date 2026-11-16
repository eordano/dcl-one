// Smart wrapper for the full-screen passport: resolves a user by address from the profile store, kicks off
// the rich-profile fetch, and renders the presentational ProfilePassport. Opened as a popup via
// openPassport() (from the profile card's "View Passport" and the sidebar's own profile), so it lives
// in the HUD-wide popup layer and reads the session via useSession() like the profile card.
import { offersInvite, useInviteToCommunityItems } from '../communities/InviteToCommunity'
import { useEffect, useRef, useState } from 'react'
import { openPopup, showConfirm } from '../../design'
import { useSession } from '../session/SessionContext'
import { requestPassport, useProfile } from '../session/profileStore'
import { relationshipOf } from '@explorer-core/lib/relationship'
import { openNameEdit } from './NameEditModal'
import { ProfilePassport } from './ProfilePassport'
import type { Profile } from '@explorer-core/engine/protocol'
import { splitName, userColor } from '@explorer-core/lib/identity'
import { hexToColor3 } from '@explorer-core/lib/color'
import { confirmBlock, confirmUnblock, confirmUnfriend, reportUser } from '../friends/friendDialogs'
import { openFriendRequest } from '../friends/FriendRequestPopup'
import { startCallWith } from '../voiceChat/callGate'

export function Passport({
  userId,
  onClose,
  onDirtyChange,
  discarded
}: {
  userId: string
  onClose: () => void
  onDirtyChange?: (dirty: boolean) => void
  /** Set when the player closed with "Discard": the name colour is dropped with the form edits. */
  discarded?: { current: boolean }
}): React.JSX.Element {
  const session = useSession()
  const { requestOwnedNames } = session.profile
  // Fetch the rich profile (badges/about) on open; render identity-only until it lands.
  useEffect(() => {
    requestPassport(userId)
  }, [userId])

  const a = userId.toLowerCase()
  const isSelf = !!session.profile.data && session.profile.data.address.toLowerCase() === a
  const known = useProfile(userId)
  const stored: Profile =
    known ??
    (isSelf && session.profile.data
      ? session.profile.data
      : { address: userId, name: userId, hasClaimedName: false, isGuest: false })

  // The name colour picked on your own passport: previewed here, deployed once as it closes, as
  // unity-explorer does (PassportController saves ClaimedNameColor on close). Every close path
  // (×, Escape, the scrim, Message) unmounts this component, so the save lives in the cleanup.
  const [pickedColor, setPickedColor] = useState<string | null>(null)
  const pickedRef = useRef(pickedColor)
  pickedRef.current = pickedColor
  const saveNameColorRef = useRef(session.profile.saveNameColor)
  saveNameColorRef.current = session.profile.saveNameColor
  const claimedRef = useRef(stored.hasClaimedName)
  claimedRef.current = stored.hasClaimedName
  // The colour the profile already shows, so picking it back deploys nothing.
  const shownRef = useRef(userColor(stored.address, stored.name, stored.hasClaimedName, stored.nameColor))
  shownRef.current = userColor(stored.address, stored.name, stored.hasClaimedName, stored.nameColor)
  // Still this account's own passport: a colour picked before an account switch is not the next one's.
  const ownRef = useRef(isSelf)
  ownRef.current = isSelf
  useEffect(
    () => () => {
      const picked = pickedRef.current
      if (picked == null || discarded?.current === true || !claimedRef.current || !ownRef.current) return
      if (picked.toLowerCase() === shownRef.current.toLowerCase()) return
      saveNameColorRef.current(hexToColor3(picked))
    },
    [discarded]
  )
  const profile: Profile = pickedColor != null ? { ...stored, nameColor: hexToColor3(pickedColor) } : stored
  const friendUser = { address: userId, name: profile.name, picture: profile.picture }
  const inviteItems = useInviteToCommunityItems(userId, offersInvite(isSelf))

  // Only your own passport can be edited, so the claimed-name list is only worth fetching there.
  useEffect(() => {
    if (isSelf) requestOwnedNames()
  }, [isSelf, requestOwnedNames])

  return (
    <ProfilePassport
      profile={profile}
      isSelf={isSelf}
      onDirtyChange={onDirtyChange}
      editing={
        isSelf && session.profile.data != null
          ? {
              saving: session.profile.saving,
              error: session.profile.saveError,
              save: session.profile.save,
              dismissError: session.profile.dismissSaveError,
              editName: () => void openNameEdit(),
              pickNameColor: setPickedColor
            }
          : undefined
      }
      relationship={relationshipOf(session.friends, userId)}
      blockedBy={session.friends.blockedBy.includes(a)}
      onAddFriend={() => openFriendRequest('send', friendUser)}
      onMessage={(a) => {
        onClose()
        session.chat.openConversation(a)
      }}
      onCall={(a) => {
        onClose()
        startCallWith(session, a)
      }}
      onUnfriend={() => void confirmUnfriend(friendUser).then((ok) => ok && session.friends.act('delete', userId))}
      onCancelRequest={() => session.friends.act('cancel', userId)}
      onAcceptRequest={() => openFriendRequest('accept', friendUser)}
      onUnblock={() => void confirmUnblock(splitName(friendUser.name).base).then((ok) => ok && session.friends.act('unblock', userId))}
      onBlock={() => void confirmBlock(splitName(friendUser.name).base).then((ok) => ok && session.friends.act('block', userId))}
      onReport={() => void reportUser(session.profile.data?.address, userId)}
      inviteItems={inviteItems}
      onClose={onClose}
      photoActions={{
        setPublic: session.gallery.setPublic,
        remove: session.gallery.remove,
        metas: session.gallery.metas,
        loadPhoto: session.gallery.loadPhoto,
        onTeleport: (x, y) => {
          onClose()
          session.map.teleport(x, y)
        },
        onViewPerson: (u) => openPassport(u.address)
      }}
    />
  )
}

/** Open a user's full-screen passport as a popup. */
export function openPassport(userId: string): () => void {
  // Unsaved edits, kept current by the passport. A stray click on the scrim is refused outright;
  // a deliberate close — the ×, or the Escape/Cancel action, which the popup layer routes through
  // the same guard — asks first.
  const dirty = { current: false }
  // "Discard" throws away every unsaved change, the picked name colour included.
  const discarded = { current: false }
  return openPopup(
    (close) => (
      <Passport
        userId={userId}
        onClose={close}
        discarded={discarded}
        onDirtyChange={(d) => {
          dirty.current = d
        }}
      />
    ),
    {
      backdropClickCloses: () => !dirty.current,
      // Sync `true` when there is nothing to lose, so an ordinary close stays as immediate as it
      // was; only an edit in progress opens the dialog.
      confirmClose: () =>
        !dirty.current ||
        showConfirm({
          title: 'Discard changes?',
          body: 'Your edits to this profile will be lost.',
          confirmLabel: 'Discard',
          cancelLabel: 'Keep editing'
        }).then((ok) => {
          if (ok) discarded.current = true
          return ok
        })
    }
  )
}

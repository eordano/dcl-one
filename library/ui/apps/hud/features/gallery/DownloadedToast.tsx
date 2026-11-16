import { useState } from 'react'
import { cefOpenDownloads } from '@explorer-core/lib/cefNativeBridge'
import styles from './DownloadedToast.module.css'

export function DownloadedToast({ message, dir }: { message: string; dir: string }): React.JSX.Element {
  const [opened, setOpened] = useState(false)
  return (
    <>
      {message}
      <button
        type="button"
        className={styles.folder}
        aria-label={`Open ${dir}`}
        disabled={opened}
        onClick={() => {
          setOpened(true)
          cefOpenDownloads()
        }}
      >
        {dir}
      </button>
    </>
  )
}

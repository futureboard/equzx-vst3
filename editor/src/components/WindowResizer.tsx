import { useCallback, useEffect, useRef, useState } from 'react'

/**
 * Bounds and default, mirroring `src/editor.rs`. The plugin floors any size it
 * is handed at the same numbers, so a drag that went below them would just come
 * back clamped and the grip would appear to stick.
 */
export const WINDOW_MIN_WIDTH = 640
export const WINDOW_MIN_HEIGHT = 420
export const WINDOW_DEFAULT_WIDTH = 1400
export const WINDOW_DEFAULT_HEIGHT = 900
const WINDOW_MAX_WIDTH = 3840
const WINDOW_MAX_HEIGHT = 2400

/**
 * Room to leave for the frame the host wraps around us.
 *
 * A plugin window is never alone: the DAW adds a title bar, and often a toolbar
 * strip above the editor as well — FL Studio's is around sixty pixels. Dragging
 * to a size that leaves no room for that means asking for a window the host
 * cannot give, and what comes back is a size the user did not choose.
 */
const HOST_CHROME_WIDTH = 32
const HOST_CHROME_HEIGHT = 140

/**
 * The largest window that can actually be honoured here.
 *
 * Bounded by the display rather than by a number picked out of the air: no host
 * will hand out a window bigger than the screen, so letting the grip drag past
 * it only produces a silent snap back to whatever the host decided instead.
 */
function maxSize(scale: number): { width: number; height: number } {
  const screen = window.screen
  // `screen` reports device pixels however the webview is zoomed, and the sizes
  // here are logical, so a logical pixel costs `scale` of the screen's.
  const div = scale > 0 ? scale : 1
  const width = screen?.availWidth ? screen.availWidth / div - HOST_CHROME_WIDTH : WINDOW_MAX_WIDTH
  const height = screen?.availHeight
    ? screen.availHeight / div - HOST_CHROME_HEIGHT
    : WINDOW_MAX_HEIGHT
  return {
    width: Math.max(WINDOW_MIN_WIDTH, Math.min(WINDOW_MAX_WIDTH, width)),
    height: Math.max(WINDOW_MIN_HEIGHT, Math.min(WINDOW_MAX_HEIGHT, height)),
  }
}

interface Props {
  /** Hand the plugin a new window size, in logical pixels. */
  onResize: (width: number, height: number) => void
  /** Display scale the editor is drawn at; 1 at 100%. */
  scale: number
}

function clamp(value: number, min: number, max: number): number {
  return Math.round(Math.min(Math.max(value, min), max))
}

/**
 * The grip in the bottom-right corner of the plugin window.
 *
 * # Why the drag is measured in screen coordinates
 *
 * The obvious implementation — capture the pointer, and read the size off
 * `clientX` plus wherever inside the grip it was grabbed — does not survive
 * contact with a resizing webview. Every size we ask for re-bounds the webview's
 * host window, and that drops the page's pointer capture, so a drag would move a
 * few pixels and then die.
 *
 * So there is no pointer capture here. Listeners go on the window, the way
 * `Knob` and `PanelResizer` already do it, and the size is worked out from how
 * far the pointer has travelled *across the screen* since the press. Screen
 * coordinates don't move when the window resizes underneath them, and measuring
 * from the press rather than accumulating deltas means that even if a few events
 * are missed — which happens when a shrinking window leaves the cursor briefly
 * outside it — the next one that arrives lands on the right size rather than
 * somewhere downstream of the drift.
 *
 * Only meaningful inside the plugin; a page can't resize its own browser window.
 */
export function WindowResizer({ onResize, scale }: Props) {
  const [dragging, setDragging] = useState(false)
  /** Where the press happened, and how big the window was at the time. */
  const start = useRef<{ x: number; y: number; w: number; h: number } | null>(null)

  // The host does real work for every size it is handed — it relays the resize
  // to the DAW, which relays it back — so a drag reporting ten sizes a frame is
  // nine window layouts done and thrown away.
  const pending = useRef<{ width: number; height: number } | null>(null)
  const frame = useRef(0)

  const flush = useCallback(() => {
    frame.current = 0
    const next = pending.current
    pending.current = null
    if (next) onResize(next.width, next.height)
  }, [onResize])

  useEffect(() => {
    if (!dragging) return

    const move = (ev: PointerEvent) => {
      const from = start.current
      if (!from) return
      const max = maxSize(scale)
      pending.current = {
        width: clamp(from.w + (ev.screenX - from.x), WINDOW_MIN_WIDTH, max.width),
        height: clamp(from.h + (ev.screenY - from.y), WINDOW_MIN_HEIGHT, max.height),
      }
      if (!frame.current) frame.current = requestAnimationFrame(flush)
    }
    const up = () => {
      setDragging(false)
      start.current = null
      // The last move of a drag is usually the one that mattered, and it may
      // still be sitting in the frame that never came.
      if (frame.current) cancelAnimationFrame(frame.current)
      flush()
    }

    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
    window.addEventListener('pointercancel', up)
    return () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      window.removeEventListener('pointercancel', up)
    }
  }, [dragging, flush, scale])

  useEffect(
    () => () => {
      if (frame.current) cancelAnimationFrame(frame.current)
    },
    [],
  )

  return (
    <div
      aria-label="Resize the plugin window"
      title="Drag to resize the window · double-click to reset"
      // A generous square in the very corner, sat above the floating band panel
      // whose rounded corner it overlaps. The drawn chevron is small; the part
      // you can actually hit is the whole 36px of it, because a resize grip you
      // have to aim at is a resize grip that doesn't get used.
      className={`fixed bottom-0 right-0 z-[60] flex h-9 w-9 cursor-nwse-resize touch-none items-end justify-end p-1 transition-colors ${
        dragging ? 'text-neon' : 'text-white/30 hover:text-white/70'
      }`}
      onPointerDown={(ev) => {
        if (ev.button !== 0) return
        ev.preventDefault()
        start.current = {
          x: ev.screenX,
          y: ev.screenY,
          w: window.innerWidth,
          h: window.innerHeight,
        }
        setDragging(true)
      }}
      onDoubleClick={() => {
        const max = maxSize(scale)
        onResize(
          Math.min(WINDOW_DEFAULT_WIDTH, max.width),
          Math.min(WINDOW_DEFAULT_HEIGHT, max.height),
        )
      }}
    >
      <svg width={16} height={16} viewBox="0 0 16 16" aria-hidden>
        <g stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" fill="none">
          <line x1="15" y1="4" x2="4" y2="15" />
          <line x1="15" y1="9" x2="9" y2="15" />
          <line x1="15" y1="14" x2="14" y2="15" />
        </g>
      </svg>
    </div>
  )
}

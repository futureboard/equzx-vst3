import { useCallback, useEffect, useRef, useState } from 'react'

/**
 * Bounds and default, mirroring `src/editor.rs`. The plugin floors any size it
 * is handed at the same numbers, so a drag that went below them would just come
 * back clamped and the grip would appear to stick.
 */
export const WINDOW_MIN_WIDTH = 640
export const WINDOW_MIN_HEIGHT = 420
export const WINDOW_DEFAULT_WIDTH = 1260
export const WINDOW_DEFAULT_HEIGHT = 760
const WINDOW_MAX_WIDTH = 3840
const WINDOW_MAX_HEIGHT = 2400

interface Props {
  /** Hand the plugin a new window size, in CSS pixels. */
  onResize: (width: number, height: number) => void
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
export function WindowResizer({ onResize }: Props) {
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
      pending.current = {
        width: clamp(from.w + (ev.screenX - from.x), WINDOW_MIN_WIDTH, WINDOW_MAX_WIDTH),
        height: clamp(from.h + (ev.screenY - from.y), WINDOW_MIN_HEIGHT, WINDOW_MAX_HEIGHT),
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
  }, [dragging, flush])

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
      onDoubleClick={() => onResize(WINDOW_DEFAULT_WIDTH, WINDOW_DEFAULT_HEIGHT)}
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

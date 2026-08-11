import { Dropdown } from './ui/Menu'
import type { ChannelView } from '../dsp/bands'

interface Props {
  channelView: ChannelView
  dbRange: number
  onChannelView: (v: ChannelView) => void
  onDbRange: (r: number) => void
}

const VIEWS: { value: ChannelView; label: string }[] = [
  { value: 'all', label: 'Stereo' },
  { value: 'left', label: 'Left' },
  { value: 'right', label: 'Right' },
  { value: 'mid', label: 'Mid' },
  { value: 'side', label: 'Side' },
]

const RANGES = [6, 12, 18, 30]

/**
 * What the plot is showing — which slice of the stereo image, and how many dB
 * tall. Parked over the top-left of the display for the same reason the
 * analyser controls sit at the top-right: they describe the picture, so they
 * belong on it rather than in the header. The two overlays are deliberately the
 * same shape, one at each upper corner.
 */
export function ViewOverlay({ channelView, dbRange, onChannelView, onDbRange }: Props) {
  return (
    <div className="glass glass-overlay flex items-center gap-1.5 rounded-full p-1">
      <Dropdown
        label="View"
        value={channelView}
        options={VIEWS.map((v) => ({ value: v.value, label: v.label }))}
        onChange={(v) => onChannelView(v as ChannelView)}
      />
      <Dropdown
        label="Range"
        value={String(dbRange)}
        options={RANGES.map((r) => ({ value: String(r), label: `± ${r} dB` }))}
        onChange={(v) => onDbRange(Number(v))}
      />
    </div>
  )
}

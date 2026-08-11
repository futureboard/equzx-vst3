import { Dropdown } from './ui/Menu'
import type { AnalyzerMode } from './EQDisplay'

interface Props {
  analyzerMode: AnalyzerMode
  spectrumSmoothing: number
  onAnalyzerMode: (m: AnalyzerMode) => void
  onSpectrumSmoothing: (v: number) => void
}

const MODES: { value: AnalyzerMode; label: string }[] = [
  { value: 'off', label: 'Off' },
  { value: 'pre', label: 'Pre' },
  { value: 'post', label: 'Post' },
  { value: 'both', label: 'Pre + Post' },
]

const SMOOTHING = [
  { value: 0, label: 'Raw' },
  { value: 1 / 24, label: '1/24 oct' },
  { value: 1 / 12, label: '1/12 oct' },
  { value: 1 / 6, label: '1/6 oct' },
  { value: 1 / 3, label: '1/3 oct' },
]

/**
 * Analyser controls, parked over the top-right of the plot instead of in the
 * header — they describe the spectrum, so they belong next to it. The pane
 * frosts whatever the analyser is drawing behind it while its own contents sit
 * back until pointed at, so it never competes with the curve.
 */
export function AnalyzerOverlay({
  analyzerMode,
  spectrumSmoothing,
  onAnalyzerMode,
  onSpectrumSmoothing,
}: Props) {
  return (
    <div className="glass glass-overlay flex items-center gap-1.5 rounded-full p-1">
      <Dropdown
        label="Analyzer"
        value={analyzerMode}
        options={MODES.map((m) => ({ value: m.value, label: m.label }))}
        onChange={(v) => onAnalyzerMode(v as AnalyzerMode)}
        align="end"
      />
      <Dropdown
        label="Smooth"
        value={String(spectrumSmoothing)}
        options={SMOOTHING.map((o) => ({ value: String(o.value), label: o.label }))}
        onChange={(v) => onSpectrumSmoothing(Number(v))}
        align="end"
      />
    </div>
  )
}

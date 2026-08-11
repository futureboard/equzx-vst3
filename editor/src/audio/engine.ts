/**
 * What the UI needs from whatever is making the sound.
 *
 * There are two implementations. `AudioEngine` runs the EQ in Web Audio, which
 * is how the standalone page works — drop a file, hear the curve. `PluginBridge`
 * runs inside the VST3/CLAP build, where the DSP lives in Rust and the host
 * supplies the audio; it answers the same questions by talking to the plugin.
 *
 * The display doesn't care which one it has, so everything it asks for is here
 * and nothing else is.
 */

/**
 * One analyser trace, already reduced to log-spaced points.
 *
 * Log-spaced rather than raw FFT bins because that is what the display draws and
 * because the plugin has to send this over a JSON bridge — a few hundred points
 * carry everything a logarithmic axis can show, where four thousand linear bins
 * would be mostly wasted on the top octave.
 */
export interface SpectrumCurve {
  /** dB per point, from `fMin` to `fMax` inclusive, spaced logarithmically. */
  db: Float32Array
  fMin: number
  fMax: number
  /** Bottom and top of the drawable range, for mapping dB to pixels. */
  floorDb: number
  ceilDb: number
}

export interface EqEngine {
  readonly sampleRate: number
  /** Is there anything to listen to? A plugin always says yes. */
  readonly hasAudio: boolean
  /** Latest analyser trace, or null when the analyser has nothing yet. */
  getSpectrum(which: 'pre' | 'post'): SpectrumCurve | null
  /** Dynamic gain offset for a band, in dB (0 when not dynamic or not engaged). */
  getDelta(id: number): number
  /** Measured band level in dBFS, for the threshold meter. */
  getLevel(id: number): number
}

/** Points in a curve produced on the JS side. Matches the plugin's `LOG_POINTS`. */
export const CURVE_POINTS = 512

/** Display floor and ceiling, shared so both engines land on the same pixels. */
export const FLOOR_DB = -110
export const CEIL_DB = -5

/**
 * Fractional index into a log-spaced curve for a frequency. Below `fMin` or
 * above `fMax` this runs off the ends; callers clamp.
 */
export function curveIndex(curve: SpectrumCurve, freq: number): number {
  const t = Math.log(freq / curve.fMin) / Math.log(curve.fMax / curve.fMin)
  return t * (curve.db.length - 1)
}

/**
 * Reduce linear-spaced FFT bins onto log-spaced points, in dB. The mirror of
 * `log_reduce` in the Rust analyser, and for the same reason: near 20 Hz several
 * output points fall inside one bin, so the value has to be interpolated or the
 * curve draws a staircase; near 20 kHz dozens of bins fall inside one point, so
 * the loudest is kept and narrow peaks survive to the display.
 *
 * `binsDb` holds dB values (what `getFloatFrequencyData` returns), so the peak
 * is picked in dB directly.
 */
export function binsToCurve(
  binsDb: Float32Array,
  binHz: number,
  out: Float32Array,
  fMin: number,
  fMax: number,
  floorDb: number,
) {
  const n = out.length
  const lastBin = binsDb.length - 1
  const ratio = fMax / fMin

  let prevPos = 0
  for (let j = 0; j < n; j++) {
    const pos = (fMin * Math.pow(ratio, j / (n - 1))) / binHz
    const nextPos = j + 1 < n ? (fMin * Math.pow(ratio, (j + 1) / (n - 1))) / binHz : pos
    const lo = j === 0 ? pos : (pos + prevPos) / 2
    const hi = j + 1 === n ? pos : (pos + nextPos) / 2
    prevPos = pos

    if (hi - lo < 1) {
      const i = Math.min(Math.max(Math.floor(pos), 0), lastBin)
      const next = Math.min(i + 1, lastBin)
      const t = Math.min(Math.max(pos - i, 0), 1)
      const a = Number.isFinite(binsDb[i]) ? binsDb[i] : floorDb
      const b = Number.isFinite(binsDb[next]) ? binsDb[next] : floorDb
      out[j] = a * (1 - t) + b * t
    } else {
      const a = Math.min(Math.max(Math.floor(lo), 0), lastBin)
      const b = Math.min(Math.max(Math.ceil(hi), a + 1), lastBin + 1)
      let peak = -Infinity
      for (let i = a; i < b; i++) if (binsDb[i] > peak) peak = binsDb[i]
      out[j] = Number.isFinite(peak) ? peak : floorDb
    }
    if (out[j] < floorDb) out[j] = floorDb
  }
}

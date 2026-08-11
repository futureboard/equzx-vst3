import type { Band } from '../dsp/bands'
import { CEIL_DB, FLOOR_DB, type EqEngine, type SpectrumCurve } from './engine'

/**
 * The UI's side of the plugin bridge.
 *
 * Inside the VST3/CLAP build the DSP is Rust and the host owns the audio, so
 * this class does none of the work — it forwards edits as actions and caches the
 * analyser and meter data the plugin pushes back. It satisfies the same
 * {@link EqEngine} contract the Web Audio engine does, which is why the display
 * and band panel need no idea which one they're talking to.
 *
 * Band identity is the plugin's slot index. The plugin exposes a fixed array of
 * band slots so the host sees a parameter list that never changes shape, and the
 * UI's `Band.id` is that index — which is what keeps an automation lane attached
 * to the band the user drew.
 */

declare global {
  interface Window {
    sendToPlugin?: (msg: unknown) => void
    onPluginMessage?: (msg: unknown) => void
  }
}

/** Everything the plugin knows that the UI has to mirror. */
export interface PluginState {
  bands: Band[]
  outputGain: number
  bypass: boolean
  sampleRate: number
  maxBands: number
  /** Opaque view state the plugin stores with the session. */
  ui: string
}

interface FrameMessage {
  type: 'frame'
  pre: string
  post: string
  level: number[]
  delta: number[]
}

interface StateMessage extends PluginState {
  type: 'state'
}

/** Frequency span of the plugin's analyser curve. Mirrors `analyzer.rs`. */
const F_MIN = 20
const F_MAX = 22000

function decodeCurve(b64: string, into: Float32Array): boolean {
  if (!b64) return false
  const bin = atob(b64)
  if (bin.length !== into.length) return false
  const span = CEIL_DB - FLOOR_DB
  for (let i = 0; i < bin.length; i++) {
    into[i] = FLOOR_DB + (bin.charCodeAt(i) / 255) * span
  }
  return true
}

export class PluginBridge implements EqEngine {
  /** Is the page running inside the plugin's webview at all? */
  static available(): boolean {
    return typeof window !== 'undefined' && typeof window.sendToPlugin === 'function'
  }

  sampleRate = 48000
  maxBands = 24
  readonly hasAudio = true

  private preDb = new Float32Array(0)
  private postDb = new Float32Array(0)
  private preReady = false
  private postReady = false
  private levels: number[] = []
  private deltas: number[] = []

  private stateListeners = new Set<(state: PluginState) => void>()

  constructor() {
    window.onPluginMessage = (msg: unknown) => this.receive(msg)
  }

  /** Ask for the current state. Answered with a `state` message. */
  init() {
    this.send({ type: 'init' })
  }

  onState(listener: (state: PluginState) => void): () => void {
    this.stateListeners.add(listener)
    return () => this.stateListeners.delete(listener)
  }

  // --- EqEngine ----------------------------------------------------------

  getSpectrum(which: 'pre' | 'post'): SpectrumCurve | null {
    const ready = which === 'pre' ? this.preReady : this.postReady
    if (!ready) return null
    return {
      db: which === 'pre' ? this.preDb : this.postDb,
      fMin: F_MIN,
      fMax: Math.min(F_MAX, this.sampleRate / 2 - 1),
      floorDb: FLOOR_DB,
      ceilDb: CEIL_DB,
    }
  }

  getDelta(id: number): number {
    return this.deltas[id] ?? 0
  }

  getLevel(id: number): number {
    return this.levels[id] ?? -100
  }

  // --- actions -----------------------------------------------------------

  addBand(slot: number, band: Partial<Band>) {
    this.send({ type: 'addBand', slot, band: wire(band) })
  }

  setBand(slot: number, patch: Partial<Band>) {
    this.send({ type: 'setBand', slot, band: wire(patch) })
  }

  removeBand(slot: number) {
    this.send({ type: 'removeBand', slot })
  }

  setSolo(slot: number | null) {
    this.send({ type: 'solo', slot })
  }

  setBypass(value: boolean) {
    this.send({ type: 'bypass', value })
  }

  setOutputGain(value: number) {
    this.send({ type: 'outputGain', value })
  }

  /** Replace every band at once — an A/B swap, a preset recall, a reset. */
  loadState(bands: Band[], outputGain: number) {
    this.send({
      type: 'loadState',
      outputGain,
      bands: bands.map((band, i) => ({ slot: band.id ?? i, ...wire(band) })),
    })
  }

  setUiState(value: string) {
    this.send({ type: 'uiState', value })
  }

  resize(width: number, height: number) {
    this.send({ type: 'resize', width: Math.round(width), height: Math.round(height) })
  }

  private send(msg: unknown) {
    window.sendToPlugin?.(msg)
  }

  private receive(msg: unknown) {
    const m = msg as FrameMessage | StateMessage
    if (!m || typeof m !== 'object') return

    if (m.type === 'frame') {
      if (this.preDb.length !== 0 || m.pre) {
        // The curve length is fixed by the plugin; size to the first frame.
        const n = m.pre ? atob(m.pre).length : this.preDb.length
        if (this.preDb.length !== n) {
          this.preDb = new Float32Array(n)
          this.postDb = new Float32Array(n)
        }
      }
      this.preReady = decodeCurve(m.pre, this.preDb)
      this.postReady = decodeCurve(m.post, this.postDb)
      this.levels = m.level
      this.deltas = m.delta
      return
    }

    if (m.type === 'state') {
      this.sampleRate = m.sampleRate
      this.maxBands = m.maxBands
      const state: PluginState = {
        bands: m.bands,
        outputGain: m.outputGain,
        bypass: m.bypass,
        sampleRate: m.sampleRate,
        maxBands: m.maxBands,
        ui: m.ui,
      }
      for (const listener of this.stateListeners) listener(state)
    }
  }
}

/**
 * Strip a band down to the fields the plugin owns.
 *
 * `id` is the slot, which travels separately, and anything else the UI keeps on
 * a band is its own business — sending it would only give the plugin's parser
 * something to reject.
 */
function wire(band: Partial<Band>): Record<string, unknown> {
  const out: Record<string, unknown> = {}
  const copy = <K extends keyof Band>(key: K) => {
    if (band[key] !== undefined) out[key] = band[key]
  }
  copy('type')
  copy('channel')
  copy('freq')
  copy('gain')
  copy('q')
  copy('slope')
  copy('enabled')
  copy('dynamic')
  copy('dynMode')
  copy('dynRange')
  copy('threshold')
  copy('attack')
  copy('release')
  return out
}

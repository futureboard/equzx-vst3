import { butterworthQs } from '../dsp/biquad'
import {
  CHANNEL_DOMAIN,
  IS_CUT,
  USES_GAIN,
  usesFirstBus,
  usesSecondBus,
  type Band,
  type Domain,
} from '../dsp/bands'
import { dynamicStep, rmsDb } from '../dsp/dynamics'
import {
  CURVE_POINTS,
  binsToCurve,
  type EqEngine,
  type SpectrumCurve,
} from './engine'

export type EngineState = 'empty' | 'loading' | 'ready' | 'playing'

interface Listeners {
  onStateChange?: (state: EngineState) => void
  onEnded?: () => void
}

/** Sidechain path that measures how much energy sits in one band's region. */
interface Detector {
  filter: BiquadFilterNode
  analyser: AnalyserNode
  buf: Float32Array<ArrayBuffer>
  /** Smoothed engagement, 0..1. */
  env: number
  /** Last measured band level in dBFS, for the meter. */
  level: number
  /** Node currently feeding the filter — the full input, or one M/S bus. */
  source: AudioNode | null
}

/**
 * The Web Audio spec fixes AnalyserNode's fftSize range at [32, 32768], so that is
 * the hard ceiling — past it you need a custom FFT over an AudioWorklet's raw
 * samples rather than an AnalyserNode.
 */
const MAX_FFT_SIZE = 32768

/** Visualiser FFT size. Larger = finer frequency detail but a longer, laggier window. */
export const FFT_SIZE = Math.min(16384, MAX_FFT_SIZE)

/** Bins the analyser hands back per frame — always half the FFT size. */
export const ANALYSER_BINS = FFT_SIZE / 2

/** Frequency span of the curve handed to the display. Matches the plugin's. */
const F_MIN = 20
const F_MAX = 22000

/**
 * Web Audio graph. With every band set to stereo it is one serial chain:
 *
 *   source -> inputGain -+-> preAnalyser
 *                        +-> [sidechain taps] -> [detector filter -> analyser] per dynamic band
 *                        |
 *                        +-> [band biquads...] -> [solo] -> outputGain -> postAnalyser -> out
 *
 * As soon as one band is channel-specific the chain splits into a pair of buses.
 * A band has to be filtered in the view its channel belongs to — left/right or
 * mid/side — so the chain carries the signal in whichever one the next band
 * needs and converts in place when it crosses between them, exactly as the
 * plugin's Rust engine does. A conversion is a four-gain matrix:
 *
 *   a -+-> [0.5] -+-> mid       mid -+-> [1] -+-> left
 *      \-> [0.5] -+-> side          \-> [1] -+-> right
 *   b -+-> [0.5] -/                side -+-> [ 1] -/
 *      \-> [-0.5] /                      \-> [-1]
 *
 * Stereo bands are instantiated twice, once per bus, which is identical to
 * filtering the pair directly: the transform is linear and the two filters match.
 *
 * The band nodes mirror `dsp/bands.ts` exactly (same Butterworth cascades), so the
 * drawn curve and the audible result are the same filter.
 */
export class AudioEngine implements EqEngine {
  readonly ctx: AudioContext
  readonly preAnalyser: AnalyserNode
  readonly postAnalyser: AnalyserNode

  /**
   * Scratch for the log-spaced curves handed to the display. The explicit
   * `ArrayBuffer` argument is what `getFloatFrequencyData` insists on — a
   * plain `Float32Array` could be backed by a `SharedArrayBuffer`.
   */
  private preBins: Float32Array<ArrayBuffer>
  private postBins: Float32Array<ArrayBuffer>
  private preCurve = new Float32Array(CURVE_POINTS)
  private postCurve = new Float32Array(CURVE_POINTS)

  private inputGain: GainNode
  private outputGain: GainNode
  /**
   * Chain nodes keyed by band id. A band owns one cascade per path it sits in —
   * one for a serial or single-channel band, two for a stereo band in M/S mode.
   */
  private bandNodes = new Map<number, BiquadFilterNode[][]>()
  private soloNodes: BiquadFilterNode[] = []
  private detectors = new Map<number, Detector>()

  // --- channel splitting ---
  /** Forces a stereo pair ahead of the splitter, so mono material still lands in both. */
  private stereoIn: GainNode
  private splitter: ChannelSplitterNode
  private merger: ChannelMergerNode
  /**
   * Gain nodes making up the domain conversions and bus heads of the current
   * chain. How many there are depends on the bands, so they are built per
   * rebuild and torn down at the start of the next one.
   */
  private matrixNodes: GainNode[] = []

  /**
   * Permanent sidechain taps off the input, one per channel view.
   *
   * Dynamic bands measure the signal as it arrives rather than as earlier bands
   * left it, so these hang off `inputGain` and never move — which also means a
   * band changing channel re-points its detector without touching the chain.
   */
  private tapStereo: GainNode
  private tapSplitter: ChannelSplitterNode
  private taps: Record<'left' | 'right' | 'mid' | 'side', GainNode>

  private buffer: AudioBuffer | null = null
  private source: AudioBufferSourceNode | null = null
  private startedAt = 0
  private pausedAt = 0
  private playing = false

  private bands: Band[] = []
  private soloId: number | null = null
  private bypassed = false
  private topology = ''

  /** Current dynamic gain offset per band id, in dB. Read by the display. */
  private deltas = new Map<number, number>()
  private pumpHandle = 0
  private lastPump = 0

  private listeners: Listeners
  loop = true
  fileName = ''

  constructor(listeners: Listeners = {}) {
    this.listeners = listeners
    this.ctx = new AudioContext()
    this.inputGain = this.ctx.createGain()
    this.outputGain = this.ctx.createGain()

    this.splitter = this.ctx.createChannelSplitter(2)
    this.merger = this.ctx.createChannelMerger(2)
    const gain = (v: number) => {
      const g = this.ctx.createGain()
      g.gain.value = v
      return g
    }

    // A splitter up-mixes with 'discrete' interpretation, which pads a mono input
    // with a silent second channel — that would put the whole file in L and leave
    // side equal to mid. Forcing a speakers-mode stereo pair first copies mono to
    // both, so mid carries the material and side comes out silent, as it should.
    this.stereoIn = gain(1)
    this.stereoIn.channelCount = 2
    this.stereoIn.channelCountMode = 'explicit'
    this.stereoIn.channelInterpretation = 'speakers'
    // Sidechain taps, wired once and left alone.
    this.tapStereo = gain(1)
    this.tapStereo.channelCount = 2
    this.tapStereo.channelCountMode = 'explicit'
    this.tapStereo.channelInterpretation = 'speakers'
    this.tapSplitter = this.ctx.createChannelSplitter(2)
    this.taps = { left: gain(1), right: gain(1), mid: gain(1), side: gain(1) }

    this.tapStereo.connect(this.tapSplitter)
    this.tapSplitter.connect(this.taps.left, 0)
    this.tapSplitter.connect(this.taps.right, 1)
    const encTap = (from: 0 | 1, to: GainNode, v: number) => {
      const g = gain(v)
      this.tapSplitter.connect(g, from)
      g.connect(to)
    }
    encTap(0, this.taps.mid, 0.5)
    encTap(1, this.taps.mid, 0.5)
    encTap(0, this.taps.side, 0.5)
    encTap(1, this.taps.side, -0.5)

    this.preAnalyser = this.ctx.createAnalyser()
    this.postAnalyser = this.ctx.createAnalyser()
    for (const a of [this.preAnalyser, this.postAnalyser]) {
      a.fftSize = FFT_SIZE
      // Inter-frame averaging: ~75 ms at 60 fps, against a ~340 ms analysis window.
      a.smoothingTimeConstant = 0.7
      a.minDecibels = -110
      a.maxDecibels = -5
    }

    this.preBins = new Float32Array(this.preAnalyser.frequencyBinCount)
    this.postBins = new Float32Array(this.postAnalyser.frequencyBinCount)

    this.inputGain.connect(this.preAnalyser)
    this.outputGain.connect(this.postAnalyser)
    this.outputGain.connect(this.ctx.destination)
    this.rebuild()

    this.lastPump = performance.now()
    this.pump = this.pump.bind(this)
    this.pumpHandle = requestAnimationFrame(this.pump)
  }

  get sampleRate() {
    return this.ctx.sampleRate
  }
  get duration() {
    return this.buffer?.duration ?? 0
  }
  get isPlaying() {
    return this.playing
  }
  get hasAudio() {
    return this.buffer !== null
  }

  /** Current playhead in seconds. */
  get position(): number {
    if (!this.buffer) return 0
    if (!this.playing) return this.pausedAt
    const raw = this.pausedAt + (this.ctx.currentTime - this.startedAt)
    return this.loop ? raw % this.buffer.duration : Math.min(raw, this.buffer.duration)
  }

  /** Dynamic gain offset for a band, in dB (0 when not dynamic or not engaged). */
  getDelta(id: number): number {
    return this.deltas.get(id) ?? 0
  }

  /** Measured band level in dBFS, for the threshold meter. */
  getLevel(id: number): number {
    return this.detectors.get(id)?.level ?? -100
  }

  /**
   * Latest analyser trace as a log-spaced curve.
   *
   * The plugin build sends this shape over its bridge already reduced; doing the
   * reduction here as well is what lets the display draw both without knowing
   * where the numbers came from.
   */
  getSpectrum(which: 'pre' | 'post'): SpectrumCurve {
    const analyser = which === 'pre' ? this.preAnalyser : this.postAnalyser
    const bins = which === 'pre' ? this.preBins : this.postBins
    const curve = which === 'pre' ? this.preCurve : this.postCurve

    analyser.getFloatFrequencyData(bins)
    const fMax = Math.min(F_MAX, this.sampleRate / 2 - 1)
    binsToCurve(bins, this.sampleRate / 2 / bins.length, curve, F_MIN, fMax, analyser.minDecibels)

    return {
      db: curve,
      fMin: F_MIN,
      fMax,
      floorDb: analyser.minDecibels,
      ceilDb: analyser.maxDecibels,
    }
  }

  // --- graph -------------------------------------------------------------

  /** Identity of the node topology; a change here means the graph must be rebuilt. */
  private signature(): string {
    const solo = this.soloId ?? 'none'
    const chain = this.bypassed
      ? 'bypass'
      : this.bands
          .map((b) => `${b.id}:${b.type}:${b.channel}:${b.enabled ? 1 : 0}:${b.slope}`)
          .join('|')
    return `${chain}#${solo}`
  }

  /** The bands that end up in the graph, in order. */
  private activeBands(): Band[] {
    if (this.bypassed) return []
    if (this.soloId !== null) return this.bands.filter((b) => b.id === this.soloId)
    return this.bands.filter((b) => b.enabled)
  }

  private rebuild() {
    for (const groups of this.bandNodes.values()) {
      for (const nodes of groups) for (const n of nodes) n.disconnect()
    }
    for (const n of this.soloNodes) n.disconnect()
    for (const n of this.matrixNodes) n.disconnect()
    this.inputGain.disconnect()
    for (const n of [this.stereoIn, this.splitter, this.merger]) n.disconnect()

    this.inputGain.connect(this.preAnalyser)
    this.inputGain.connect(this.tapStereo)

    this.bandNodes = new Map()
    this.soloNodes = []
    this.matrixNodes = []

    const active = this.activeBands()
    const soloBand = this.soloId !== null ? (this.bands.find((b) => b.id === this.soloId) ?? null) : null
    const split = active.some((b) => b.channel !== 'stereo')

    let tail: AudioNode = this.inputGain
    if (split) {
      // The pair starts, and must end, as left/right.
      this.inputGain.connect(this.stereoIn)
      this.stereoIn.connect(this.splitter)
      let a: AudioNode = this.busHead(this.splitter, 0)
      let b: AudioNode = this.busHead(this.splitter, 1)
      let domain: Domain = 'lr'

      for (const band of active) {
        // A stereo band asks for no domain in particular, so it runs in whichever
        // one the chain is already in — one filter on both buses commutes with
        // the transform between them.
        const want = CHANNEL_DOMAIN[band.channel]
        if (want && want !== domain) {
          ;[a, b] = this.convert(a, b, want)
          domain = want
        }

        const groups: BiquadFilterNode[][] = []
        if (usesFirstBus(band.channel)) {
          const nodes = this.createBandNodes(band)
          for (const n of nodes) {
            a.connect(n)
            a = n
          }
          groups.push(nodes)
        }
        if (usesSecondBus(band.channel)) {
          const nodes = this.createBandNodes(band)
          for (const n of nodes) {
            b.connect(n)
            b = n
          }
          groups.push(nodes)
        }
        this.bandNodes.set(band.id, groups)
      }

      // Soloing a band that acts on one bus isolates that bus, so you hear the
      // slice on its own rather than folded back into its opposite.
      const soloDomain = soloBand ? CHANNEL_DOMAIN[soloBand.channel] : null
      if (soloBand && soloDomain) {
        if (soloDomain !== domain) {
          ;[a, b] = this.convert(a, b, soloDomain)
          domain = soloDomain
        }
        const mute = this.matrixGain(0)
        if (usesFirstBus(soloBand.channel)) {
          b.connect(mute)
          b = mute
        } else {
          a.connect(mute)
          a = mute
        }
      }

      if (domain !== 'lr') [a, b] = this.convert(a, b, 'lr')
      a.connect(this.merger, 0, 0)
      b.connect(this.merger, 0, 1)
      tail = this.merger
    } else {
      for (const band of active) {
        const nodes = this.createBandNodes(band)
        for (const node of nodes) {
          tail.connect(node)
          tail = node
        }
        this.bandNodes.set(band.id, [nodes])
      }
    }

    if (soloBand) {
      for (const node of this.createSoloNodes(soloBand)) {
        tail.connect(node)
        tail = node
        this.soloNodes.push(node)
      }
    }

    tail.connect(this.outputGain)
    this.connectDetectors()
    this.topology = this.signature()
  }

  /** A gain node belonging to the current chain, torn down on the next rebuild. */
  private matrixGain(value: number): GainNode {
    const g = this.ctx.createGain()
    g.gain.value = value
    this.matrixNodes.push(g)
    return g
  }

  /** One output of the splitter, as a node the chain can build onto. */
  private busHead(from: ChannelSplitterNode, channel: 0 | 1): GainNode {
    const head = this.matrixGain(1)
    from.connect(head, channel)
    return head
  }

  /**
   * Move the pair between domains, returning the new bus heads.
   *
   * Encoding is (a+b)/2 and (a-b)/2; decoding sums and differences them straight
   * back. Four gains into two summing nodes — Web Audio adds everything that
   * lands on the same input, so the matrix needs no explicit adder.
   */
  private convert(a: AudioNode, b: AudioNode, to: Domain): [AudioNode, AudioNode] {
    const [aa, ab, ba, bb] = to === 'ms' ? [0.5, 0.5, 0.5, -0.5] : [1, 1, 1, -1]
    const outA = this.matrixGain(1)
    const outB = this.matrixGain(1)
    const leg = (from: AudioNode, to_: GainNode, v: number) => {
      const g = this.matrixGain(v)
      from.connect(g)
      g.connect(to_)
    }
    leg(a, outA, aa)
    leg(b, outA, ab)
    leg(a, outB, ba)
    leg(b, outB, bb)
    return [outA, outB]
  }

  private createBandNodes(band: Band): BiquadFilterNode[] {
    if (IS_CUT[band.type]) {
      const kind: BiquadFilterType = band.type === 'lowcut' ? 'highpass' : 'lowpass'
      return butterworthQs(band.slope / 6).map((q) => {
        const n = this.ctx.createBiquadFilter()
        n.type = kind
        n.frequency.value = this.safeFreq(band.freq)
        // Web Audio takes Q in dB for lowpass/highpass.
        n.Q.value = 20 * Math.log10(q)
        return n
      })
    }

    const n = this.ctx.createBiquadFilter()
    n.type = (
      {
        bell: 'peaking',
        lowshelf: 'lowshelf',
        highshelf: 'highshelf',
        notch: 'notch',
        bandpass: 'bandpass',
      } as Record<string, BiquadFilterType>
    )[band.type]
    n.frequency.value = this.safeFreq(band.freq)
    n.Q.value = band.q
    n.gain.value = band.gain + this.getDelta(band.id)
    return [n]
  }

  /** Solo listens through the region the band acts on. */
  private createSoloNodes(band: Band): BiquadFilterNode[] {
    const make = (type: BiquadFilterType, q: number) => {
      const n = this.ctx.createBiquadFilter()
      n.type = type
      n.frequency.value = this.safeFreq(band.freq)
      n.Q.value = q
      return n
    }
    switch (band.type) {
      case 'lowcut':
      case 'lowshelf':
        return [make('lowpass', 0)]
      case 'highcut':
      case 'highshelf':
        return [make('highpass', 0)]
      default:
        return [make('bandpass', Math.max(band.q, 0.7))]
    }
  }

  private safeFreq(f: number) {
    return Math.min(Math.max(f, 10), this.sampleRate / 2 - 1)
  }

  /** Push band values into the graph, rebuilding only when the topology changed. */
  setBands(bands: Band[]) {
    this.bands = bands
    this.syncDetectors(bands)

    if (this.signature() !== this.topology) {
      this.rebuild()
      return
    }

    const t = this.ctx.currentTime
    for (const band of bands) {
      const groups = this.bandNodes.get(band.id)
      if (!groups) continue
      for (const nodes of groups) {
        if (IS_CUT[band.type]) {
          for (const n of nodes) n.frequency.setTargetAtTime(this.safeFreq(band.freq), t, 0.008)
        } else {
          const n = nodes[0]
          n.frequency.setTargetAtTime(this.safeFreq(band.freq), t, 0.008)
          n.Q.setTargetAtTime(band.q, t, 0.008)
          // A dynamic band's gain belongs to the pump loop; don't fight it here.
          if (!this.isDynamic(band)) n.gain.setTargetAtTime(band.gain, t, 0.008)
        }
      }
    }

    const soloBand = bands.find((b) => b.id === this.soloId)
    if (soloBand) {
      for (const n of this.soloNodes) {
        n.frequency.setTargetAtTime(this.safeFreq(soloBand.freq), t, 0.008)
      }
    }
  }

  private isDynamic(band: Band): boolean {
    return band.dynamic && band.enabled && USES_GAIN[band.type]
  }

  // --- dynamics ----------------------------------------------------------

  /**
   * A dynamic band reacts to the signal it actually filters, so it listens to
   * the tap for its own channel. A stereo band hears the mono sum, which is what
   * an AnalyserNode on the input would measure anyway — and is the mid tap.
   *
   * The taps hang off the input rather than off the chain, so this is the same
   * answer whatever the bands around it are doing.
   */
  private detectorSource(band: Band | undefined): AudioNode {
    if (!band || band.channel === 'stereo') return this.taps.mid
    return this.taps[band.channel]
  }

  /** (Re)wire every detector's input. Safe to call repeatedly — edges are deduped. */
  private connectDetectors() {
    for (const [id, d] of this.detectors) {
      const src = this.detectorSource(this.bands.find((b) => b.id === id))
      if (d.source && d.source !== src) {
        try {
          d.source.disconnect(d.filter)
        } catch {
          // A rebuild already tore that edge down; disconnecting a dead one throws.
        }
      }
      src.connect(d.filter)
      d.source = src
    }
  }

  /** Create/remove/retune the sidechain detectors so they track the dynamic bands. */
  private syncDetectors(bands: Band[]) {
    const wanted = new Set(bands.filter((b) => this.isDynamic(b)).map((b) => b.id))

    for (const [id, d] of this.detectors) {
      if (wanted.has(id)) continue
      try {
        d.source?.disconnect(d.filter)
      } catch {
        // Edge already gone with the last rebuild.
      }
      d.filter.disconnect()
      d.analyser.disconnect()
      this.detectors.delete(id)
      this.deltas.delete(id)
    }

    for (const band of bands) {
      if (!this.isDynamic(band)) continue
      let d = this.detectors.get(band.id)
      if (!d) {
        const filter = this.ctx.createBiquadFilter()
        const analyser = this.ctx.createAnalyser()
        analyser.fftSize = 2048
        filter.connect(analyser)
        const source = this.detectorSource(band)
        source.connect(filter)
        d = {
          filter, analyser, buf: new Float32Array(analyser.fftSize), env: 0, level: -100, source,
        }
        this.detectors.set(band.id, d)
      }
      // Listen to the slice of spectrum the band acts on.
      const t = this.ctx.currentTime
      if (band.type === 'lowshelf') {
        d.filter.type = 'lowpass'
        d.filter.Q.setTargetAtTime(0, t, 0.01)
      } else if (band.type === 'highshelf') {
        d.filter.type = 'highpass'
        d.filter.Q.setTargetAtTime(0, t, 0.01)
      } else {
        d.filter.type = 'bandpass'
        d.filter.Q.setTargetAtTime(Math.max(band.q, 0.5), t, 0.01)
      }
      d.filter.frequency.setTargetAtTime(this.safeFreq(band.freq), t, 0.01)
    }
  }

  /**
   * Control-rate dynamics: measure each dynamic band's level, smooth it with the
   * band's attack/release, and drive its filter gain. Running at frame rate rather
   * than audio rate keeps this in plain Web Audio nodes; the cost is that attack
   * times below roughly a frame (~16 ms) are floored by the update interval.
   */
  private pump(now: number) {
    this.pumpHandle = requestAnimationFrame(this.pump)
    const dt = Math.min((now - this.lastPump) / 1000, 0.1)
    this.lastPump = now
    if (dt <= 0) return

    const t = this.ctx.currentTime

    for (const band of this.bands) {
      const d = this.detectors.get(band.id)
      if (!d || !this.isDynamic(band)) continue

      d.analyser.getFloatTimeDomainData(d.buf)
      d.level = rmsDb(d.buf)

      const step = dynamicStep(band, d.level, d.env, dt)
      d.env = step.env
      this.deltas.set(band.id, step.delta)

      for (const nodes of this.bandNodes.get(band.id) ?? []) {
        nodes[0]?.gain.setTargetAtTime(band.gain + step.delta, t, 0.01)
      }
    }
  }

  setSolo(id: number | null) {
    this.soloId = id
    this.rebuild()
  }

  setBypass(on: boolean) {
    this.bypassed = on
    this.rebuild()
  }

  setOutputGain(db: number) {
    this.outputGain.gain.setTargetAtTime(Math.pow(10, db / 20), this.ctx.currentTime, 0.02)
  }

  // --- transport ---------------------------------------------------------

  async loadFile(file: File) {
    this.emit('loading')
    this.stop()
    const data = await file.arrayBuffer()
    this.buffer = await this.ctx.decodeAudioData(data)
    this.fileName = file.name
    this.pausedAt = 0
    this.emit('ready')
  }

  async play() {
    if (!this.buffer || this.playing) return
    if (this.ctx.state === 'suspended') await this.ctx.resume()

    const src = this.ctx.createBufferSource()
    src.buffer = this.buffer
    src.loop = this.loop
    src.connect(this.inputGain)
    src.onended = () => {
      if (this.source === src && !this.loop) {
        this.playing = false
        this.pausedAt = 0
        this.source = null
        this.emit('ready')
        this.listeners.onEnded?.()
      }
    }
    src.start(0, this.pausedAt % this.buffer.duration)
    this.source = src
    this.startedAt = this.ctx.currentTime
    this.playing = true
    this.emit('playing')
  }

  pause() {
    if (!this.playing) return
    const pos = this.position
    this.stopSource()
    this.pausedAt = pos
    this.playing = false
    this.emit('ready')
  }

  stop() {
    this.stopSource()
    this.pausedAt = 0
    this.playing = false
    if (this.buffer) this.emit('ready')
  }

  seek(seconds: number) {
    const was = this.playing
    this.stopSource()
    this.playing = false
    this.pausedAt = Math.max(0, Math.min(seconds, this.duration))
    if (was) void this.play()
  }

  setLoop(on: boolean) {
    this.loop = on
    if (this.source) this.source.loop = on
  }

  private stopSource() {
    if (!this.source) return
    this.source.onended = null
    try {
      this.source.stop()
    } catch {
      /* already stopped */
    }
    this.source.disconnect()
    this.source = null
  }

  private emit(state: EngineState) {
    this.listeners.onStateChange?.(state)
  }

  dispose() {
    cancelAnimationFrame(this.pumpHandle)
    this.stopSource()
    void this.ctx.close()
  }
}

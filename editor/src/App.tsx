import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { animate, stagger } from 'animejs'
import { AudioEngine } from './audio/AudioEngine'
import { PluginBridge, type PluginState } from './audio/PluginBridge'
import type { EqEngine } from './audio/engine'
import { EQDisplay, type AnalyzerMode } from './components/EQDisplay'
import { AnalyzerOverlay } from './components/AnalyzerOverlay'
import { BandStrip } from './components/BandStrip'
import { Header } from './components/Header'
import { Transport } from './components/Transport'
import { PanelResizer } from './components/PanelResizer'
import {
  MAX_BANDS,
  defaultBands,
  freeSlot,
  makeBand,
  makeBandInSlot,
  type Band,
  type ChannelView,
} from './dsp/bands'
import { cloneSnapshot, emptySnapshot, type Snapshot } from './state/presets'

/**
 * The same UI drives two very different things.
 *
 * On a plain page it owns a Web Audio graph: drop a file in, hear the curve.
 * Inside the VST3/CLAP build there is a {@link PluginBridge} instead — the DSP
 * is Rust, the host supplies the audio, and every edit becomes a plugin
 * parameter change so the DAW can automate and save it. Which one is in play is
 * decided once, here, and the rest of the tree only sees an `EqEngine`.
 */
const PANEL_DEFAULT = 232
const PANEL_MIN = 176
const PANEL_KEY = 'equzfree.panelHeight'

function readPanelHeight(): number {
  try {
    const v = Number(localStorage.getItem(PANEL_KEY))
    return Number.isFinite(v) && v >= PANEL_MIN ? v : PANEL_DEFAULT
  } catch {
    return PANEL_DEFAULT
  }
}

/**
 * One engine for the page lifetime. Module scope rather than state so it survives
 * StrictMode's double-mount without opening a second AudioContext.
 */
let sharedEngine: AudioEngine | null = null
function getWebEngine(): AudioEngine {
  sharedEngine ??= new AudioEngine()
  return sharedEngine
}

let sharedBridge: PluginBridge | null = null
function getBridge(): PluginBridge | null {
  if (!PluginBridge.available()) return null
  sharedBridge ??= new PluginBridge()
  return sharedBridge
}

/** View state the plugin stores with the session, but never automates. */
interface UiState {
  dbRange: number
  analyzerMode: AnalyzerMode
  spectrumSmoothing: number
  channelView: ChannelView
  panelHeight: number
  slot: 'A' | 'B'
  parked: Snapshot
}

export default function App() {
  const bridge = useMemo(getBridge, [])
  const webEngine = bridge ? null : getWebEngine()
  const engine: EqEngine = bridge ?? (webEngine as AudioEngine)

  const [bands, setBands] = useState<Band[]>(defaultBands)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [soloId, setSoloId] = useState<number | null>(null)
  const [bypassed, setBypassed] = useState(false)
  const [dbRange, setDbRange] = useState(18)
  const [analyzerMode, setAnalyzerMode] = useState<AnalyzerMode>('both')
  const [spectrumSmoothing, setSpectrumSmoothing] = useState(1 / 12)
  const [channelView, setChannelView] = useState<ChannelView>('all')
  const [outputGain, setOutputGain] = useState(0)
  const [maxBands, setMaxBands] = useState(MAX_BANDS)

  const [fileName, setFileName] = useState('')
  const [loading, setLoading] = useState(false)
  const [playing, setPlaying] = useState(false)
  const [loop, setLoop] = useState(true)
  const [position, setPosition] = useState(0)
  const [duration, setDuration] = useState(0)
  const [dragOver, setDragOver] = useState(false)
  const [panelHeight, setPanelHeight] = useState(readPanelHeight)
  const [viewportH, setViewportH] = useState(() => window.innerHeight)

  // A/B: the live state is whichever slot is active; the other one is parked here.
  const [slot, setSlot] = useState<'A' | 'B'>('A')
  const [parked, setParked] = useState<Snapshot>(emptySnapshot)

  const shellRef = useRef<HTMLDivElement>(null)
  const dropRef = useRef<HTMLDivElement>(null)
  const dragDepth = useRef(0)

  // Both bars float over the display, so the plot has to be told how much room to
  // leave for them — the header wraps and the bottom panel is user-resizable, so
  // neither height is a constant.
  const headerRef = useRef<HTMLDivElement>(null)
  const bottomRef = useRef<HTMLDivElement>(null)
  const [headerH, setHeaderH] = useState(70)
  const [bottomH, setBottomH] = useState(280)
  useEffect(() => {
    const observe = (el: HTMLElement | null, set: (h: number) => void) => {
      if (!el) return () => {}
      const ro = new ResizeObserver(([entry]) => set(entry.contentRect.height))
      ro.observe(el)
      return () => ro.disconnect()
    }
    const stopTop = observe(headerRef.current, setHeaderH)
    const stopBottom = observe(bottomRef.current, setBottomH)
    return () => {
      stopTop()
      stopBottom()
    }
  }, [])

  /**
   * The band list, readable synchronously.
   *
   * Every band edit is two things at once: React state, and a message to the
   * plugin. The message is a side effect, so it can't live in a `setBands`
   * updater — React is free to run one twice. But reading `bands` from the
   * render closure instead means two edits in the same turn both see the list
   * as it was before either of them: click twice quickly and the second click
   * claims the slot the first one just took. So mutations go through
   * `commitBands`, which moves the ref and the state together.
   */
  const bandsRef = useRef(bands)
  const commitBands = useCallback((next: Band[]) => {
    bandsRef.current = next
    setBands(next)
  }, [])

  // --- plugin state ------------------------------------------------------
  /**
   * The plugin only pushes state that came from somewhere the UI can't see —
   * host automation, a DAW preset recall, undo. The UI's own edits are applied
   * locally and echoed straight to the plugin, so they never arrive back here.
   */
  const uiRestored = useRef(false)

  const applyUiState = useCallback((raw: string) => {
    if (!raw) return
    try {
      const ui = JSON.parse(raw) as Partial<UiState>
      if (typeof ui.dbRange === 'number') setDbRange(ui.dbRange)
      if (ui.analyzerMode) setAnalyzerMode(ui.analyzerMode)
      if (typeof ui.spectrumSmoothing === 'number') setSpectrumSmoothing(ui.spectrumSmoothing)
      if (ui.channelView) setChannelView(ui.channelView)
      if (typeof ui.panelHeight === 'number') setPanelHeight(ui.panelHeight)
      if (ui.slot === 'A' || ui.slot === 'B') setSlot(ui.slot)
      if (ui.parked && Array.isArray(ui.parked.bands)) setParked(ui.parked)
    } catch {
      // Nothing the session saved is worth breaking the editor over.
    }
  }, [])

  useEffect(() => {
    if (!bridge) return
    const stop = bridge.onState((state: PluginState) => {
      commitBands(state.bands)
      setOutputGain(state.outputGain)
      setBypassed(state.bypass)
      setMaxBands(state.maxBands)
      // View state is restored once, on the first message. After that it belongs
      // to the user, and reapplying it would undo whatever they just changed.
      if (!uiRestored.current) {
        uiRestored.current = true
        applyUiState(state.ui)
      }
    })
    bridge.init()
    return stop
  }, [bridge, applyUiState, commitBands])

  // Push view state back for the session to hold. Everything here is cheap and
  // changes rarely, so a plain effect is enough — no debounce needed.
  useEffect(() => {
    if (!bridge || !uiRestored.current) return
    const ui: UiState = {
      dbRange,
      analyzerMode,
      spectrumSmoothing,
      channelView,
      panelHeight,
      slot,
      parked,
    }
    bridge.setUiState(JSON.stringify(ui))
  }, [bridge, dbRange, analyzerMode, spectrumSmoothing, channelView, panelHeight, slot, parked])

  // --- engine sync -------------------------------------------------------
  // Only the Web Audio engine needs pushing at; the plugin was told as it happened.
  useEffect(() => webEngine?.setBands(bands), [webEngine, bands])
  useEffect(() => webEngine?.setSolo(soloId), [webEngine, soloId])
  useEffect(() => webEngine?.setBypass(bypassed), [webEngine, bypassed])
  useEffect(() => webEngine?.setOutputGain(outputGain), [webEngine, outputGain])
  useEffect(() => webEngine?.setLoop(loop), [webEngine, loop])

  // Poll the playhead; the engine owns the clock.
  useEffect(() => {
    if (!webEngine) return
    let raf = 0
    const tick = () => {
      raf = requestAnimationFrame(tick)
      setPosition(webEngine.position)
      if (webEngine.isPlaying !== playing) setPlaying(webEngine.isPlaying)
    }
    raf = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(raf)
  }, [webEngine, playing])

  // --- band editing ------------------------------------------------------
  const patch = useCallback(
    (id: number, p: Partial<Band>) => {
      commitBands(bandsRef.current.map((b) => (b.id === id ? { ...b, ...p } : b)))
      bridge?.setBand(id, p)
    },
    [bridge, commitBands],
  )

  // A band created while looking at one channel belongs to that channel — otherwise
  // it would appear in a view that isn't showing it.
  // Messages to the plugin are side effects, so they are sent from the callback
  // itself and never from inside a state updater — React is free to run an
  // updater twice, and the plugin would hear the edit twice with it.
  const addBand = useCallback(
    (freq = 1000, gain = 0): number | null => {
      const current = bandsRef.current
      if (current.length >= maxBands) return null
      const shape = {
        freq,
        gain,
        q: 1,
        channel: channelView === 'all' ? ('stereo' as const) : channelView,
      }
      let band: Band
      if (bridge) {
        // In the plugin the id is a parameter slot, so it has to be a free one
        // rather than the next number from a counter.
        const slot = freeSlot(current, maxBands)
        if (slot === null) return null
        band = makeBandInSlot(slot, shape)
        bridge.addBand(slot, band)
      } else {
        band = makeBand(shape)
      }
      commitBands([...current, band].sort((a, b) => a.freq - b.freq))
      setSelectedId(band.id)
      return band.id
    },
    [channelView, bridge, maxBands, commitBands],
  )

  const changeSolo = useCallback(
    (id: number | null) => {
      setSoloId(id)
      bridge?.setSolo(id)
    },
    [bridge],
  )

  const removeBand = useCallback(
    (id: number) => {
      commitBands(bandsRef.current.filter((b) => b.id !== id))
      setSelectedId((cur) => (cur === id ? null : cur))
      if (soloId === id) changeSolo(null)
      bridge?.removeBand(id)
    },
    [bridge, soloId, changeSolo, commitBands],
  )

  const changeBypass = useCallback(
    (next: boolean | ((v: boolean) => boolean)) => {
      const value = typeof next === 'function' ? next(bypassed) : next
      setBypassed(value)
      bridge?.setBypass(value)
    },
    [bridge, bypassed],
  )

  const changeOutputGain = useCallback(
    (db: number) => {
      setOutputGain(db)
      bridge?.setOutputGain(db)
    },
    [bridge],
  )

  // --- A/B compare & presets ---------------------------------------------
  const snapshot = useCallback((): Snapshot => ({ bands, outputGain }), [bands, outputGain])

  /**
   * Replace everything at once — an A/B swap, a preset, a reset.
   *
   * The plugin's bands live in fixed slots, so a snapshot that came from
   * somewhere else (the other A/B slot, a preset file) has to be re-slotted
   * before it is loaded, or two bands would claim the same parameters.
   */
  const applySnapshot = useCallback(
    (snap: Snapshot) => {
      let next = snap.bands
      if (bridge) {
        next = snap.bands.slice(0, maxBands).map((band, i) => ({ ...band, id: i }))
        bridge.loadState(next, snap.outputGain)
      }
      commitBands(next)
      setOutputGain(snap.outputGain)
      // Ids differ between slots and presets, so nothing stays selected across a swap.
      setSelectedId(null)
      setSoloId(null)
      bridge?.setSolo(null)
    },
    [bridge, maxBands, commitBands],
  )

  const reset = useCallback(() => {
    applySnapshot(emptySnapshot())
    changeBypass(false)
  }, [applySnapshot, changeBypass])

  const switchSlot = useCallback(() => {
    const live = snapshot()
    applySnapshot(parked)
    setParked(live)
    setSlot((s) => (s === 'A' ? 'B' : 'A'))
  }, [snapshot, parked, applySnapshot])

  const copyToOther = useCallback(() => {
    setParked(cloneSnapshot(snapshot()))
  }, [snapshot])

  // The panel may never squeeze the EQ display out of existence. Both floating
  // bars and their margins come out of the same budget, hence the deep reserve.
  const panelMax = Math.max(PANEL_MIN, viewportH - 380)
  const panelH = Math.min(panelHeight, panelMax)

  useEffect(() => {
    const onResize = () => setViewportH(window.innerHeight)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  const changePanelHeight = useCallback((h: number) => {
    setPanelHeight(h)
    try {
      localStorage.setItem(PANEL_KEY, String(h))
    } catch {
      /* storage blocked — the height just won't persist */
    }
  }, [])

  // --- file loading ------------------------------------------------------
  const loadFile = useCallback(
    async (file: File) => {
      if (!webEngine) return
      setLoading(true)
      try {
        await webEngine.loadFile(file)
        setFileName(file.name)
        setDuration(webEngine.duration)
        await webEngine.play()
        setPlaying(true)
      } catch (err) {
        console.error(err)
        setFileName(`Could not decode "${file.name}"`)
      } finally {
        setLoading(false)
      }
    },
    [webEngine],
  )

  // --- drag & drop -------------------------------------------------------
  // Only meaningful on the page: in a plugin the host is the source of audio.
  useEffect(() => {
    if (!webEngine) return
    const onEnter = (ev: DragEvent) => {
      ev.preventDefault()
      dragDepth.current++
      if (ev.dataTransfer?.types.includes('Files')) setDragOver(true)
    }
    const onOver = (ev: DragEvent) => ev.preventDefault()
    const onLeave = (ev: DragEvent) => {
      ev.preventDefault()
      dragDepth.current = Math.max(0, dragDepth.current - 1)
      if (dragDepth.current === 0) setDragOver(false)
    }
    const onDrop = (ev: DragEvent) => {
      ev.preventDefault()
      dragDepth.current = 0
      setDragOver(false)
      const file = ev.dataTransfer?.files?.[0]
      if (file) void loadFile(file)
    }
    window.addEventListener('dragenter', onEnter)
    window.addEventListener('dragover', onOver)
    window.addEventListener('dragleave', onLeave)
    window.addEventListener('drop', onDrop)
    return () => {
      window.removeEventListener('dragenter', onEnter)
      window.removeEventListener('dragover', onOver)
      window.removeEventListener('dragleave', onLeave)
      window.removeEventListener('drop', onDrop)
    }
  }, [loadFile, webEngine])

  useEffect(() => {
    if (!dropRef.current) return
    animate(dropRef.current, {
      opacity: dragOver ? [0, 1] : [1, 0],
      scale: dragOver ? [1.03, 1] : [1, 1.02],
      duration: 220,
      ease: 'outQuad',
    })
  }, [dragOver])

  // --- intro animation ---------------------------------------------------
  useEffect(() => {
    const nodes = shellRef.current?.querySelectorAll('[data-intro]')
    if (!nodes?.length) return
    animate(nodes, {
      opacity: [0, 1],
      translateY: [12, 0],
      duration: 620,
      delay: stagger(70),
      ease: 'outCubic',
    })
  }, [])

  const togglePlay = useCallback(() => {
    if (!webEngine?.hasAudio) return
    if (webEngine.isPlaying) {
      webEngine.pause()
      setPlaying(false)
    } else {
      void webEngine.play().then(() => setPlaying(true))
    }
  }, [webEngine])

  // --- keyboard ----------------------------------------------------------
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      const target = ev.target as HTMLElement
      const tag = target.tagName
      // Don't hijack keys while a preset name / slider / dropdown has focus.
      if (tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA' || target.isContentEditable) return
      if (ev.code === 'Space' && webEngine) {
        ev.preventDefault()
        togglePlay()
      } else if (ev.key === 'Delete' || ev.key === 'Backspace') {
        if (selectedId !== null) removeBand(selectedId)
      } else if (ev.key.toLowerCase() === 'b') {
        changeBypass((v) => !v)
      } else if (ev.key === 'Escape') {
        setSelectedId(null)
      } else if (ev.key.toLowerCase() === 'x') {
        switchSlot()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  })

  return (
    <div className="h-screen w-screen overflow-hidden bg-[#070708] text-white/90">
      <div ref={shellRef} className="relative flex h-full w-full flex-col bg-[#0b0b0d]">
        <div ref={headerRef} data-intro className="absolute inset-x-3 top-3 z-30">
          <Header
            channelView={channelView}
            dbRange={dbRange}
            outputGain={outputGain}
            bypassed={bypassed}
            slot={slot}
            otherSlotFilled={parked.bands.length > 0}
            getSnapshot={snapshot}
            onLoadSnapshot={applySnapshot}
            onSwitchSlot={switchSlot}
            onCopyToOther={copyToOther}
            onChannelView={setChannelView}
            onDbRange={setDbRange}
            onOutputGain={changeOutputGain}
            onBypass={changeBypass}
            onReset={reset}
          />
        </div>

        <div
          data-intro
          className="relative min-h-0 flex-1 bg-gradient-to-b from-[#0a0a0b] to-[#121214]"
          // 12px to clear each bar's offset from the window edge, plus a gap so
          // the plot never runs right up under a floating edge.
          style={{ paddingTop: headerH + 20, paddingBottom: bottomH + 20 }}
        >
          {/* Ambient light for the bar to pick up — glass over a flat field reads as plastic. */}
          <div
            className="pointer-events-none absolute inset-x-0 top-0 h-48 bg-[radial-gradient(120%_100%_at_18%_0%,rgba(255,77,157,0.18),transparent_60%),radial-gradient(90%_100%_at_88%_0%,rgba(255,211,228,0.10),transparent_62%)]"
            aria-hidden
          />
          <div className="relative h-full w-full">
            <EQDisplay
              bands={bands}
              selectedId={selectedId}
              soloId={soloId}
              bypassed={bypassed}
              dbRange={dbRange}
              analyzerMode={analyzerMode}
              spectrumSmoothing={spectrumSmoothing}
              channelView={channelView}
              engine={engine}
              canAdd={bands.length < maxBands}
              onPatch={patch}
              onSelect={setSelectedId}
              onSolo={changeSolo}
              onAdd={addBand}
              onRemove={removeBand}
            />
            {/* Sits inside the plot, top-right, over the analyser it controls. */}
            <div className="absolute right-3 top-3 z-20">
              <AnalyzerOverlay
                analyzerMode={analyzerMode}
                spectrumSmoothing={spectrumSmoothing}
                onAnalyzerMode={setAnalyzerMode}
                onSpectrumSmoothing={setSpectrumSmoothing}
              />
            </div>
          </div>
          {!engine.hasAudio && !loading && (
            <div className="pointer-events-none absolute inset-x-0 top-[62%] text-center text-[11px] text-white/25">
              Drop an audio file anywhere to hear the EQ · click the display to add a band ·
              scroll a handle for Q · right-drag a handle to solo · X swaps A/B
            </div>
          )}
          {bridge && bands.length === 0 && (
            <div className="pointer-events-none absolute inset-x-0 top-[62%] text-center text-[11px] text-white/25">
              Click the display to add a band · scroll a handle for Q · right-drag a handle to
              solo · X swaps A/B
            </div>
          )}
        </div>

        {/* Transport, resizer and band panel travel together as one floating slab. */}
        <div ref={bottomRef} data-intro className="absolute inset-x-3 bottom-3 z-30">
          <div className="glass overflow-hidden rounded-[22px]">
            {webEngine && (
              <Transport
                fileName={fileName}
                hasAudio={webEngine.hasAudio}
                loading={loading}
                playing={playing}
                loop={loop}
                position={position}
                duration={duration}
                onPlayPause={togglePlay}
                onStop={() => {
                  webEngine.stop()
                  setPlaying(false)
                }}
                onLoop={setLoop}
                onSeek={(t) => webEngine.seek(t)}
                onFile={loadFile}
              />
            )}

            <PanelResizer
              height={panelH}
              min={PANEL_MIN}
              max={panelMax}
              defaultHeight={PANEL_DEFAULT}
              onChange={changePanelHeight}
            />

            <BandStrip
              bands={bands}
              selectedId={selectedId}
              soloId={soloId}
              engine={engine}
              onSelect={setSelectedId}
              onPatch={patch}
              onRemove={removeBand}
              onSolo={changeSolo}
              height={panelH}
            />
          </div>
        </div>
      </div>

      {webEngine && (
        <div
          ref={dropRef}
          className={`fixed inset-0 z-50 grid place-items-center bg-black/70 backdrop-blur-sm ${
            dragOver ? '' : 'pointer-events-none opacity-0'
          }`}
        >
          <div className="rounded-[28px] border-2 border-dashed border-neon/70 px-16 py-12 text-center shadow-[0_0_60px_-10px_rgba(255,77,157,0.5)]">
            <div className="text-2xl font-semibold text-white">Drop audio to preview</div>
            <div className="mt-1 text-[12px] text-white/45">WAV · MP3 · FLAC · OGG · M4A</div>
          </div>
        </div>
      )}
    </div>
  )
}

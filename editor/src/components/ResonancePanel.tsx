import { useEffect, useRef } from 'react'
import { Chevron, LABEL, Menu } from './ui/Menu'
import { Knob } from './Knob'
import type { EqEngine } from '../audio/engine'
import type { Resonance } from '../dsp/resonance'

interface Props {
  resonance: Resonance
  engine: EqEngine
  onPatch: (patch: Partial<Resonance>) => void
}

/**
 * Header control for the adaptive resonance suppressor.
 *
 * The pill is the switch and the meter at once — click to arm the stage, and
 * while it is armed the number beside it is the deepest cut the bank is making
 * right now, which is the one thing you want to see without opening anything.
 * Everything that shapes *what* it cuts lives behind the chevron.
 */
export function ResonancePanel({ resonance, engine, onPatch }: Props) {
  const readout = useRef<HTMLSpanElement>(null)

  // The reduction moves every frame. Writing it into the node directly keeps a
  // sixty-hertz number from re-rendering the whole header to show it.
  useEffect(() => {
    const node = readout.current
    if (!node || !resonance.enabled) return
    let raf = 0
    let shown = ''
    const tick = () => {
      raf = requestAnimationFrame(tick)
      const peak = engine.getResonance?.()?.peak ?? 0
      const text = peak > 0.05 ? `−${peak.toFixed(1)}` : '0.0'
      if (text !== shown) {
        shown = text
        node.textContent = text
      }
    }
    raf = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(raf)
  }, [engine, resonance.enabled])

  const knob = (
    label: string,
    key: keyof Resonance,
    min: number,
    max: number,
    fallback: number,
    format: (v: number) => string,
    scale: 'linear' | 'log' = 'linear',
  ) => (
    <Knob
      label={label}
      value={resonance[key] as number}
      min={min}
      max={max}
      scale={scale}
      defaultValue={fallback}
      disabled={!resonance.enabled}
      format={format}
      onChange={(v) => onPatch({ [key]: v } as Partial<Resonance>)}
    />
  )

  return (
    <div className="flex shrink-0 items-center">
      <button
        type="button"
        onClick={() => onPatch({ enabled: !resonance.enabled })}
        title="Adaptive resonance suppression — cuts whatever stands out from the spectrum around it"
        className={`flex h-8 items-center gap-1.5 rounded-l-full rounded-r-none px-3 text-[11px] font-medium transition ${
          resonance.enabled ? 'neon-on' : 'glass-pill text-white/55 hover:text-white/85'
        }`}
      >
        <svg width={12} height={12} viewBox="0 0 12 12" fill="none" aria-hidden>
          {/* A peak being pressed down — the whole idea in twelve pixels. */}
          <path
            d="M1 9.5h2.2L6 3l2.8 6.5H11"
            stroke="currentColor"
            strokeWidth={1.5}
            strokeLinecap="round"
            strokeLinejoin="round"
          />
          <path d="M4 5.2h4" stroke="currentColor" strokeWidth={1.5} strokeLinecap="round" />
        </svg>
        Res
        {resonance.enabled && (
          <span ref={readout} className="w-8 text-right tabular-nums opacity-80">
            0.0
          </span>
        )}
      </button>

      <Menu
        align="end"
        panelClass="w-[292px]"
        title="Resonance settings"
        triggerClass="glass-pill flex h-8 items-center rounded-l-none rounded-r-full pl-1.5 pr-2"
        trigger={(open) => <Chevron open={open} />}
      >
        {() => (
          <div className="px-1.5 pb-2 pt-1">
            <div className={`${LABEL} px-1 pb-2`}>Adaptive resonance</div>

            <div className="grid grid-cols-4 justify-items-center gap-y-3">
              {knob('Depth', 'depth', 0, 100, 50, (v) => `${v.toFixed(0)}%`)}
              {knob('Sharp', 'sharpness', 0, 100, 50, (v) => `${v.toFixed(0)}%`)}
              {knob('Thresh', 'threshold', -12, 24, 6, (v) => `${v.toFixed(1)}`)}
              {knob('Mix', 'mix', 0, 100, 100, (v) => `${v.toFixed(0)}%`)}
              {knob('Attack', 'attack', 0.5, 100, 5, (v) => `${v.toFixed(1)}m`, 'log')}
              {knob('Rel', 'release', 5, 1000, 40, (v) => `${v.toFixed(0)}m`, 'log')}
              {knob('Low', 'low', 20, 2000, 20, fmtHz, 'log')}
              {knob('High', 'high', 500, 20000, 20000, fmtHz, 'log')}
            </div>

            <button
              type="button"
              onClick={() => onPatch({ delta: !resonance.delta })}
              disabled={!resonance.enabled}
              title="Hear only what the stage is removing"
              className={`mt-3 flex h-7 w-full items-center justify-center rounded-xl text-[11px] font-medium transition disabled:opacity-30 ${
                resonance.delta ? 'neon-on' : 'glass-pill text-white/60 hover:text-white/90'
              }`}
            >
              Listen to what's removed
            </button>

            <p className="px-1 pt-2 text-[10px] leading-relaxed text-white/35">
              Cuts only what stands proud of the spectrum around it, so a sloped mix passes
              through and a ringing peak does not. Zero latency.
            </p>
          </div>
        )}
      </Menu>
    </div>
  )
}

function fmtHz(v: number): string {
  return v >= 1000 ? `${(v / 1000).toFixed(v >= 10000 ? 0 : 1)}k` : `${v.toFixed(0)}`
}

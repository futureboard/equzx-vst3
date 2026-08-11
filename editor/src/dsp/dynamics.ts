import { DYN_KNEE_DB, type Band } from './bands'

export interface DynStep {
  /** Smoothed engagement, 0..1. */
  env: number
  /** Gain offset to apply to the band, in dB. */
  delta: number
}

/**
 * Engagement across the soft knee, 0..1, from how far the level sits past the
 * threshold in the engaging direction.
 *
 * Smoothstep rather than a straight ramp. A ramp is continuous but its slope
 * jumps at both corners, so the band goes from perfectly still to travelling at
 * full rate the instant the level touches the threshold — the grabbiness a soft
 * knee exists to remove. Both agree at half a knee in, where engagement is 0.5.
 *
 * Mirrors `knee` in `src/dsp/dynamics.rs`.
 */
export function knee(overDb: number): number {
  const t = Math.min(Math.max(overDb / DYN_KNEE_DB, 0), 1)
  return t * t * (3 - 2 * t)
}

/** One-pole move from `env` toward `target` over `tauMs`, `dt` seconds on. */
export function stepToward(env: number, target: number, tauMs: number, dt: number): number {
  const tau = Math.max(tauMs / 1000, 0.001)
  const next = env + (target - env) * (1 - Math.exp(-dt / tau))
  // A tail this far down is orders below anything a gain could show.
  return Math.abs(next) < 1e-6 ? 0 : next
}

/**
 * One control-rate step of a band's dynamics.
 *
 * The level's distance past the threshold maps across a `DYN_KNEE_DB` soft knee
 * to 0..1, which is then smoothed by a one-pole using attack going up and
 * release coming down. The band's gain moves by `env * dynRange`.
 */
export function dynamicStep(
  band: Pick<Band, 'threshold' | 'dynMode' | 'dynRange' | 'attack' | 'release'>,
  levelDb: number,
  env: number,
  dt: number,
): DynStep {
  const over = band.dynMode === 'above' ? levelDb - band.threshold : band.threshold - levelDb
  const target = knee(over)

  const tauMs = target > env ? band.attack : band.release
  const next = stepToward(env, target, tauMs, dt)

  return { env: next, delta: next * band.dynRange }
}

/** RMS of a time-domain block, in dBFS. */
export function rmsDb(buf: Float32Array): number {
  let sum = 0
  for (let i = 0; i < buf.length; i++) sum += buf[i] * buf[i]
  return 20 * Math.log10(Math.max(Math.sqrt(sum / buf.length), 1e-6))
}

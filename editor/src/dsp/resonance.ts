/**
 * The adaptive resonance suppressor, as the UI sees it.
 *
 * Mirrors `ResonanceParams` in `src/params.rs` and `ResonanceSettings` in
 * `src/dsp/resonance.rs`. Percentages travel 0..100 and frequencies in Hz, the
 * units this UI shows; the plugin converts them to the ratios its parameters
 * hold. The DSP itself has no counterpart on this side — the stage is a filter
 * bank in Rust, so it exists only in the plugin build.
 */
export interface Resonance {
  enabled: boolean
  /** Fraction of a band's excess to remove, as a percentage. */
  depth: number
  /** How narrow a peak has to be before it counts, as a percentage. */
  sharpness: number
  /** dB above the local average at which suppression starts. */
  threshold: number
  attack: number
  release: number
  low: number
  high: number
  mix: number
  /** Monitor what is being removed instead of what is being kept. */
  delta: boolean
}

export const defaultResonance = (): Resonance => ({
  enabled: false,
  depth: 50,
  sharpness: 50,
  threshold: 6,
  attack: 5,
  release: 40,
  low: 20,
  high: 20000,
  mix: 100,
  delta: false,
})

/**
 * Fallback layout of the suppression bank, for drawing before the first state
 * message lands. The plugin sends its own copy of these with every state, and
 * that copy is what the display actually uses.
 */
export const RES_BANDS = 60
export const RES_F_LO = 20
export const RES_BANDS_PER_OCTAVE = 6
export const RES_MAX_CUT_DB = 36

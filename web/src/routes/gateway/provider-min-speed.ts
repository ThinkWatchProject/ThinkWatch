/** A stream's window when none is set, in seconds. */
export const DEFAULT_MIN_SPEED_WINDOW_SECS = 20;

/**
 * The minimum generation speed as the PATCH sends it, or `null` when a
 * field is out of range: the speed blank (off) or a whole number from 1
 * to 1000, the window a whole number from 5 to 120.
 */
export function minSpeedFields(rate: string, window: string) {
  const whole = (v: string, lo: number, hi: number) =>
    /^\d+$/.test(v.trim()) && Number(v) >= lo && Number(v) <= hi ? Number(v) : null;
  const tokens = rate.trim() === '' ? null : whole(rate, 1, 1000);
  const secs = whole(window, 5, 120);
  if ((rate.trim() !== '' && tokens === null) || secs === null) return null;
  return { min_speed_tokens_per_sec: tokens, min_speed_window_secs: secs };
}

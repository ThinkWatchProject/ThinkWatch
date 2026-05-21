import { useEffect, useRef, useState } from 'react';

import { api } from '@/lib/api';
import { DashboardLiveSchema, WsTicketSchema, type DashboardLive } from '@/lib/schemas';

/**
 * Live snapshot via WebSocket. Falls back to a one-shot HTTP fetch if WS
 * can't connect (e.g. behind a proxy that doesn't speak the upgrade).
 *
 * Reconnects with exponential backoff up to 15s, pauses while the tab
 * is hidden, and re-mints a single-use ticket on every reconnect so
 * the JWT never appears in the WS URL.
 */
export function useLiveDashboard(range: string) {
  const [live, setLive] = useState<DashboardLive | null>(null);
  const [connected, setConnected] = useState(false);
  // Ref mirror so the WS callbacks can read "have we ever received data?"
  // without capturing a stale closure.
  const liveRef = useRef<DashboardLive | null>(null);
  liveRef.current = live;

  useEffect(() => {
    // Clear the previous range's snapshot so the panels (especially
    // the top-users leaderboard, which is range-scoped) show their
    // skeleton during the reconnect handshake instead of rendering
    // old-window data under the new range's eyebrow. The ticket mint
    // + WS upgrade + first frame round-trip is typically <500ms but
    // can stretch on a cold-start; without this the UI would show a
    // mismatched window for that interval with no loading signal.
    setLive(null);

    let ws: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let cancelled = false;
    let backoff = 1000;

    // Detach all handlers from a WebSocket before closing it so the
    // close event can't trigger a reconnect we don't want — used both
    // when the tab goes hidden and when the effect tears down.
    const closeQuietly = (w: WebSocket | null) => {
      if (!w) return;
      w.onopen = null;
      w.onmessage = null;
      w.onerror = null;
      w.onclose = null;
      try {
        w.close();
      } catch {
        // ignore — best-effort cleanup
      }
    };

    const connect = async () => {
      if (cancelled) return;
      // Don't open a fresh socket while the tab is hidden — otherwise
      // a range toggle (which re-runs this effect) would start a new
      // WS stream in the background with no visibility transition to
      // close it. The visibilitychange handler re-invokes `connect`
      // when the tab becomes visible again.
      if (document.hidden) return;
      // If a previous socket is still around (e.g. an errored one
      // whose onclose hasn't fired yet), detach it so its delayed
      // close can't queue another reconnect on top of this one.
      closeQuietly(ws);
      ws = null;
      // Auth tokens live in HttpOnly cookies now, so the page JS
      // can't pre-check "are we logged in". We just try to mint
      // the WS ticket — if the user isn't authenticated the api
      // client gets a 401 and routes them through the standard
      // refresh-then-redirect flow.
      // Mint a single-use ticket via authenticated POST. The ticket is
      // bound to the user_id and expires in 30s; the WS endpoint atomically
      // consumes it. This keeps the JWT out of the WS URL (which would
      // otherwise leak through access logs, browser history, and Referer
      // headers).
      let ticket: string;
      try {
        const res = await api<{ ticket: string }>('/api/dashboard/ws-ticket', {
          method: 'POST',
          schema: WsTicketSchema,
        });
        ticket = res.ticket;
      } catch {
        scheduleReconnect();
        return;
      }
      // Re-check visibility AND cancellation after the ticket await.
      // The pre-await `document.hidden` guard misses the window where
      // the tab flips hidden mid-mint: `onVis` runs its closeQuietly
      // on a still-null `ws`, then we continue and create a WebSocket
      // in the now-hidden tab with no listener to clean it up until
      // the next visibility transition.
      if (cancelled || document.hidden) return;

      const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
      const apiBase = import.meta.env.VITE_API_BASE ?? '';
      // Range goes on the WS URL so the server-side snapshot loop
      // knows which window the leaderboard should cover. The effect
      // re-runs when `range` changes, dropping and re-establishing
      // the socket — that's cheap (ticket mint + WS upgrade) and
      // happens only when the operator toggles 24h / 7d / 30d.
      const httpUrl = new URL(
        `${apiBase}/api/dashboard/ws?ticket=${encodeURIComponent(ticket)}&range=${encodeURIComponent(range)}`,
        window.location.origin,
      );
      const wsUrl = `${proto}://${httpUrl.host}${httpUrl.pathname}${httpUrl.search}`;

      try {
        ws = new WebSocket(wsUrl);
      } catch {
        scheduleReconnect();
        return;
      }

      ws.onopen = () => {
        backoff = 1000;
        setConnected(true);
      };
      ws.onmessage = (ev) => {
        try {
          const payload = JSON.parse(ev.data) as DashboardLive;
          setLive(payload);
        } catch (err) {
          // Surface parse failures so they're visible in devtools.
          console.error('dashboard ws parse failed', err, ev.data);
        }
      };
      ws.onerror = () => {
        // Surface state, the close handler will trigger reconnect.
        setConnected(false);
      };
      ws.onclose = () => {
        setConnected(false);
        // First close — try a one-shot HTTP fetch so the user sees data
        // immediately even if WS is unavailable. Reads via ref so it
        // sees the latest state, not a stale closure capture.
        if (liveRef.current === null) {
          api<DashboardLive>(`/api/dashboard/live?range=${encodeURIComponent(range)}`, {
            schema: DashboardLiveSchema,
          })
            .then(setLive)
            .catch((err) => {
              // The WS closed and HTTP fallback also failed — the user
              // will see the "disconnected" indicator but should also
              // know data is missing. Log to console for debugging.
              // Don't toast here: the reconnect loop will retry shortly
              // and a toast per close would spam the UI.
              console.warn('[dashboard] live fallback fetch failed:', err);
            });
        }
        scheduleReconnect();
      };
    };

    const scheduleReconnect = () => {
      if (cancelled) return;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      reconnectTimer = setTimeout(() => {
        void connect();
      }, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };

    void connect();

    const onVis = () => {
      if (document.hidden) {
        // Detach handlers BEFORE closing — otherwise the queued
        // onclose would call scheduleReconnect and we'd silently
        // reconnect in the background while the tab is hidden.
        closeQuietly(ws);
        ws = null;
        if (reconnectTimer) {
          clearTimeout(reconnectTimer);
          reconnectTimer = null;
        }
      } else if (!ws || ws.readyState === WebSocket.CLOSED) {
        void connect();
      }
    };
    document.addEventListener('visibilitychange', onVis);

    return () => {
      cancelled = true;
      document.removeEventListener('visibilitychange', onVis);
      if (reconnectTimer) clearTimeout(reconnectTimer);
      closeQuietly(ws);
      ws = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [range]);

  return { live, connected };
}

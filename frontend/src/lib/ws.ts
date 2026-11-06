// Thin WebSocket wrapper: auto protocol, PING keepalive, typed events.

import type { ServerEvent } from './protocol';

export interface SocketOpts {
  onEvent: (msg: ServerEvent) => void | Promise<void>;
  onClose?: (byUser: boolean) => void;
  onOpen?: () => void;
  serialize?: (handler: () => void | Promise<void>) => void;
  heartbeatMs?: number;
  reconnectDelayMs?: number;
  autoReconnect?: boolean;
}

export class CallSocket<TOut> {
  private ws: WebSocket | null = null;
  private url: string;
  private opts: SocketOpts;
  private chain: Promise<void> = Promise.resolve();
  private heartbeat: ReturnType<typeof setInterval> | null = null;
  private closedByUser = false;
  /** Messages queued while the socket is down (timestamped so stale ones
   *  never flush after a long outage). */
  private outbox: Array<[number, string]> = [];

  constructor(url: string, opts: SocketOpts) {
    this.url = url;
    this.opts = opts;
  }

  connect() {
    this.closedByUser = false;
    this.open();
  }

  private open() {
    this.close( /* silent */ true);
    const ws = new WebSocket(this.url);
    this.ws = ws;

    ws.onopen = () => {
      this.startHeartbeat();
      this.opts.onOpen?.();
      // flush anything queued while disconnected (drop messages > 30s old)
      while (this.outbox.length && this.ws?.readyState === WebSocket.OPEN) {
        const [ts, text] = this.outbox.shift()!;
        if (Date.now() - ts > 30000) continue;
        this.ws.send(text);
      }
    };
    ws.onmessage = (e) => {
      let msg: ServerEvent;
      try {
        msg = JSON.parse(e.data);
      } catch {
        return;
      }
      if (msg.event === 'PONG') return;
      const run = () => this.opts.onEvent(msg);
      const serialize = this.opts.serialize ?? ((fn) => { this.chain = this.chain.then(fn).catch(console.error); });
      serialize(run);
    };
    const handleClose = () => {
      this.stopHeartbeat();
      if (this.ws === ws) this.ws = null;
      this.opts.onClose?.(this.closedByUser);
      if (!this.closedByUser && (this.opts.autoReconnect ?? false)) {
        setTimeout(() => {
          if (!this.closedByUser) this.open();
        }, this.opts.reconnectDelayMs ?? 1500);
      }
    };
    ws.onclose = handleClose;
    ws.onerror = () => { /* onclose follows */ };
  }

  /** Raw binary frame (e.g. live PCM audio for emotion analysis).
   *  Never queued — if the socket is down the chunk is simply dropped. */
  sendBinary(data: ArrayBuffer | Blob): boolean {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data);
      return true;
    }
    return false;
  }

  send(payload: TOut): boolean {
    const text = JSON.stringify(payload);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(text);
      return true;
    }
    // Socket down (reconnecting): queue instead of dropping. The message is
    // flushed on the next open. Cap the outbox so a dead server can't grow it.
    if (!this.closedByUser) {
      if (this.outbox.length < 200) this.outbox.push([Date.now(), text]);
      return true;
    }
    return false;
  }

  private startHeartbeat() {
    this.stopHeartbeat();
    this.heartbeat = setInterval(() => {
      this.send({ action: 'PING' } as TOut);
    }, this.opts.heartbeatMs ?? 5000);
  }

  private stopHeartbeat() {
    if (this.heartbeat) clearInterval(this.heartbeat);
    this.heartbeat = null;
  }

  close(silent = false) {
    if (silent) {
      const s = this.ws;
      this.ws = null;
      try { s?.close(); } catch { /* ignore */ }
      this.stopHeartbeat();
      return;
    }
    this.closedByUser = true;
    this.stopHeartbeat();
    this.outbox = []; // user-initiated close: drop queued messages
    const s = this.ws;
    this.ws = null;
    try { s?.close(); } catch { /* ignore */ }
  }
}

export function wsUrl(path: string): string {
  const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${window.location.host}${path}`;
}

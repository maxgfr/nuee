// @ts-check
// Direct lanes between a joined page and a presenter page, over one WebRTC data channel each.
// RTCPeerConnection lives here, on the main thread (workers cannot create one in every
// browser); frames cross to the iroh worker with postMessage through the bridge. Signalling
// goes through iroh (sendRtc / `rtc` events). Only STUN URLs announced by the presenter are
// used, none by default (host candidates only), and never a TURN server.
//
// Lane protocol (crates/nuee-net/src/rtc.rs): the worker sends its results on the channel,
// the presenter acknowledges each one; when the channel closes, unacknowledged results go
// again through iroh and the presenter ignores a second copy of a settled lease.
/** @typedef {import('./bridge.js').Bridge} Bridge */
/** @typedef {import('../api').NueeEvent} NueeEvent */

/** Peer connections a presenter accepts at once. */
export const MAX_LANES = 64;
/** How long a joined page waits for its lane before taking work through iroh only. */
export const LANE_WAIT_MS = 8000;

/** Counters for the live views (and the e2e checks). */
export const stats = { created: 0, opened: 0, open: 0, framesIn: 0, framesOut: 0 };

/** Every live lane, closed together on page hide. @type {Set<{ close(): void }>} */
const live = new Set();

if (typeof addEventListener === 'function') {
  addEventListener('pagehide', () => closeAll());
}

if (typeof location !== 'undefined' && new URL(location.href).searchParams.get('e2e') === '1') {
  /** @type {Record<string, unknown>} */ (/** @type {unknown} */ (globalThis)).nueeRtc = { stats, closeAll };
}

/** Closes every lane of this page. */
export function closeAll() {
  for (const l of [...live]) l.close();
}

/** @param {string[]} stun @returns {RTCIceServer[]} */
export function iceServers(stun) {
  return stun.filter((u) => /^stuns?:/i.test(u)).map((urls) => ({ urls }));
}

/** @param {RTCIceCandidate | null} c */
function candidatePayload(c) {
  return c ? JSON.stringify(c.toJSON()) : '';
}

/** @param {MessageEvent} e */
function frameOf(e) {
  return e.data instanceof ArrayBuffer ? new Uint8Array(e.data) : null;
}

/**
 * Applies remote candidates once the remote description is set.
 * @param {RTCPeerConnection} pc
 */
function candidateQueue(pc) {
  /** @type {RTCIceCandidateInit[]} */
  let waiting = [];
  let ready = false;
  /** @param {RTCIceCandidateInit} c */
  const add = (c) =>
    pc.addIceCandidate(c).catch((e) => console.warn('ICE candidate refused', e));
  return {
    /** @param {string} payload */
    push(payload) {
      if (payload === '') return;
      const c = /** @type {RTCIceCandidateInit} */ (JSON.parse(payload));
      if (ready) add(c);
      else waiting.push(c);
    },
    flush() {
      ready = true;
      for (const c of waiting) add(c);
      waiting = [];
    },
  };
}

/**
 * The presenter side: answers the offers of its workers and serves their lanes.
 * @param {Bridge} bridge
 * @param {number} session the host session
 * @param {string[]} stun
 * @param {(peer: string) => void} [onResult] called for each result received on a lane
 */
export function hostLanes(bridge, session, stun, onResult) {
  /** @type {Map<string, { pc: RTCPeerConnection, ice: ReturnType<typeof candidateQueue> }>} */
  const peers = new Map();

  /** @param {string} peer */
  const drop = (peer) => {
    const p = peers.get(peer);
    if (!p) return;
    peers.delete(peer);
    p.pc.close();
  };

  /** @param {string} peer @param {string} sdp */
  async function answer(peer, sdp) {
    drop(peer);
    if (peers.size >= MAX_LANES) return;
    const pc = new RTCPeerConnection({ iceServers: iceServers(stun) });
    stats.created += 1;
    const ice = candidateQueue(pc);
    peers.set(peer, { pc, ice });
    pc.onicecandidate = (e) => {
      bridge.call('sendRtc', { session, peer, kind: 'ice', payload: candidatePayload(e.candidate) }).catch((err) => console.warn('signalling', err));
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'failed' || pc.connectionState === 'closed') drop(peer);
    };
    pc.ondatachannel = (ev) => {
      const dc = ev.channel;
      if (dc.label !== 'nuee') {
        dc.close();
        return;
      }
      dc.binaryType = 'arraybuffer';
      dc.onopen = () => {
        stats.opened += 1;
        stats.open += 1;
      };
      dc.onclose = () => {
        stats.open = Math.max(0, stats.open - 1);
        drop(peer);
      };
      dc.onmessage = (m) => {
        const frame = frameOf(m);
        if (!frame) return;
        bridge.call('laneFrame', { session, peer, frame }, { move: true }).then(
          (ack) => {
            if (!ack || dc.readyState !== 'open') return;
            stats.framesIn += 1;
            dc.send(ack);
            if (onResult) onResult(peer);
          },
          (err) => {
            console.warn('lane frame refused', err);
            dc.close();
          },
        );
      };
    };
    await pc.setRemoteDescription({ type: 'offer', sdp });
    ice.flush();
    const local = await pc.createAnswer();
    await pc.setLocalDescription(local);
    await bridge.call('sendRtc', { session, peer, kind: 'answer', payload: local.sdp ?? '' });
  }

  const unsubscribe = bridge.onEvent((e, s) => {
    if (s !== session) return;
    if (e.type === 'peerLeft') drop(e.peer);
    if (e.type !== 'rtc') return;
    if (e.kind === 'offer') {
      answer(e.peer, e.payload).catch((err) => {
        console.warn('WebRTC answer failed', err);
        drop(e.peer);
      });
    } else if (e.kind === 'ice') {
      const p = peers.get(e.peer);
      if (p) p.ice.push(e.payload);
    }
  });

  const handle = {
    close() {
      live.delete(handle);
      unsubscribe();
      for (const peer of [...peers.keys()]) drop(peer);
    },
  };
  live.add(handle);
  return handle;
}

/**
 * The joined side: opens the lane to the coordinator. `onSettled(open)` is called once,
 * when the lane opened or when it could not (then everything stays on iroh).
 * @param {Bridge} bridge
 * @param {number} session the join session
 * @param {string} coordinator endpoint id of the coordinator
 * @param {string[]} stun
 * @param {(open: boolean) => void} onSettled
 */
export function joinLane(bridge, session, coordinator, stun, onSettled) {
  const pc = new RTCPeerConnection({ iceServers: iceServers(stun) });
  stats.created += 1;
  const ice = candidateQueue(pc);
  const dc = pc.createDataChannel('nuee', { ordered: true });
  dc.binaryType = 'arraybuffer';
  let settled = false;
  let up = false;
  let closed = false;
  /** @param {boolean} open */
  const settle = (open) => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    onSettled(open);
  };
  const timer = setTimeout(() => settle(false), LANE_WAIT_MS);

  const down = () => {
    if (up) {
      up = false;
      stats.open = Math.max(0, stats.open - 1);
      bridge.call('laneClosed', { session }).catch((e) => console.warn('lane fallback', e));
    }
    settle(false);
  };

  const unsubscribe = bridge.onEvent((e, s) => {
    if (s !== session) return;
    if (e.type === 'lane') {
      if (dc.readyState === 'open') {
        dc.send(e.frame);
        stats.framesOut += 1;
      } else {
        down();
      }
    } else if (e.type === 'rtc' && e.peer === coordinator) {
      if (e.kind === 'answer') {
        pc.setRemoteDescription({ type: 'answer', sdp: e.payload }).then(
          () => ice.flush(),
          (err) => {
            console.warn('WebRTC answer refused', err);
            handle.close();
          },
        );
      } else if (e.kind === 'ice') {
        ice.push(e.payload);
      }
    }
  });

  pc.onicecandidate = (e) => {
    bridge.call('sendRtc', { session, peer: coordinator, kind: 'ice', payload: candidatePayload(e.candidate) }).catch((err) => console.warn('signalling', err));
  };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'failed') handle.close();
  };
  dc.onopen = () => {
    bridge.call('laneOpen', { session }).then(
      () => {
        up = true;
        stats.opened += 1;
        stats.open += 1;
        settle(true);
      },
      (e) => {
        console.warn('lane not opened', e);
        handle.close();
      },
    );
  };
  dc.onclose = () => handle.close();
  dc.onmessage = (m) => {
    const frame = frameOf(m);
    if (!frame) return;
    bridge.call('laneFrame', { session, frame }, { move: true }).catch((e) => console.warn('lane ack', e));
  };

  const handle = {
    close() {
      if (closed) return;
      closed = true;
      live.delete(handle);
      unsubscribe();
      down();
      pc.close();
    },
  };
  live.add(handle);

  (async () => {
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await bridge.call('sendRtc', { session, peer: coordinator, kind: 'offer', payload: offer.sdp ?? '' });
  })().catch((e) => {
    console.warn('WebRTC offer failed', e);
    handle.close();
  });
  return handle;
}

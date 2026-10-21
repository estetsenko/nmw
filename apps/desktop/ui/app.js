// NMW desktop UI. The Rust side owns the mesh; this file only renders what
// it reports (`snapshot` polling plus `nmw-event` events) and forwards
// sends. All user-supplied text is rendered with textContent.
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// One packet carries MAX_DATA (MAX_GROUP_DATA in a channel) bytes; longer
// text goes as up to 16 parts with a 4-byte header each (nmw-core text.rs).
const MAX_DATA = 181;
const MAX_GROUP_DATA = 151;
const PART_HEADER = 4;
const MAX_PARTS = 16;
const MAX_TEXT = MAX_PARTS * (MAX_DATA - PART_HEADER);
const MAX_GROUP_TEXT = MAX_PARTS * (MAX_GROUP_DATA - PART_HEADER);
// Files go to contacts in chunks (nmw-core file.rs); photos are shrunk first
// because every kilobyte costs about 10 s of airtime per hop.
const MAX_FILE = 32 * 1024;
const PHOTO_TARGET = 12 * 1024;
const PHOTO_MAX_SIDE = 640;
// Shown inline; anything else (SVG included) is offered as a file to save.
const INLINE_IMAGES = ['image/jpeg', 'image/png', 'image/webp', 'image/gif'];
const LOG_LIMIT = 300;
const $ = (id) => document.getElementById(id);

// A conversation is a contact (`c:<addr>`) or a channel (`g:<id>`).
const state = {
  me: null,
  contacts: new Map(), // addr -> {addr, name, hops}
  channels: new Map(), // id -> {id, name, public, key}
  // conversation key -> [{dir, text, seq, at_ms, status, name}], or for a file
  // {dir, file: {id, name, mime, size, data}, at_ms, status, done, count}
  chats: new Map(),
  unread: new Map(), // conversation key -> count
  early: new Map(), // `${to}:${seq}` -> status that arrived before send_text returned
  selected: null, // conversation key
  log: [],
};

const encoder = new TextEncoder();

function shortAddr(addr) {
  return addr.slice(0, 8);
}

function fmtClock(ms) {
  const s = Math.floor(ms / 1000);
  const pad = (n) => String(n).padStart(2, '0');
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}`;
}

function fmtSize(n) {
  return n < 1024 ? `${n} B` : `${(n / 1024).toFixed(1)} KB`;
}

function routeText(hops) {
  if (hops === null || hops === undefined) return 'No route yet';
  if (hops === 0) return 'Direct';
  return hops === 1 ? 'Via 1 repeater' : `Via ${hops} repeaters`;
}

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function showError(id, msg) {
  const e = $(id);
  e.textContent = msg || '';
  e.hidden = !msg;
}

function hue(hex) {
  return parseInt(hex.slice(0, 4), 16) % 360;
}

function isChannel(key) {
  return key !== null && key.startsWith('g:');
}

function selectedContact() {
  return state.selected && !isChannel(state.selected) ? state.contacts.get(state.selected.slice(2)) : null;
}

function selectedChannel() {
  return isChannel(state.selected) ? state.channels.get(state.selected.slice(2)) : null;
}

function maxText() {
  return isChannel(state.selected) ? MAX_GROUP_TEXT : MAX_TEXT;
}

// Number of packets `text` goes out as, cutting at character boundaries the
// way nmw-core's text::split does.
function partCount(text) {
  const single = isChannel(state.selected) ? MAX_GROUP_DATA : MAX_DATA;
  if (encoder.encode(text).length <= single) return 1;
  const room = single - PART_HEADER;
  let parts = 1;
  let used = 0;
  for (const ch of text) {
    const n = encoder.encode(ch).length;
    if (used + n > room) {
      parts += 1;
      used = 0;
    }
    used += n;
  }
  return parts;
}

// ---- sidebar ----

function upsertContact(c) {
  const prev = state.contacts.get(c.addr);
  // A placeholder made from a typed address takes the real name later.
  state.contacts.set(c.addr, { ...prev, ...c });
}

// Line icons for channel avatars: a globe for public, a padlock for private.
const ICONS = {
  public: ['M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18', 'M3 12h18', 'M12 3c3 3.2 3 14.8 0 18', 'M12 3c-3 3.2-3 14.8 0 18'],
  private: ['M6 11h12v9H6z', 'M8.5 11V8a3.5 3.5 0 0 1 7 0v3'],
  attach: ['M20 11.5l-8 8a5 5 0 0 1-7-7l8.5-8.5a3.3 3.3 0 0 1 4.7 4.7l-8.4 8.4a1.7 1.7 0 0 1-2.4-2.4l7.6-7.6'],
  file: ['M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z', 'M14 3v5h5'],
};

function icon(name) {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  for (const d of ICONS[name]) {
    const path = document.createElementNS(ns, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

// `avatar` is the text to show, or a node such as an icon.
function listItem(key, avatar, avatarHue, name, sub, extraCls) {
  const li = el('li', 'contact' + (key === state.selected ? ' selected' : ''));
  li.dataset.key = key;
  li.tabIndex = 0;
  const avatarEl = el('span', 'avatar' + (extraCls ? ` ${extraCls}` : ''), typeof avatar === 'string' ? avatar : '');
  if (typeof avatar !== 'string') avatarEl.append(avatar);
  avatarEl.style.background = `hsl(${avatarHue} 45% 42%)`;
  li.append(avatarEl);
  const info = el('div', 'info');
  info.append(el('div', 'name', name));
  info.append(el('div', 'sub', sub));
  li.append(info);
  const n = state.unread.get(key);
  if (n) li.append(el('span', 'unread', String(n)));
  li.addEventListener('click', () => select(key));
  li.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') select(key);
  });
  return li;
}

function renderSidebar() {
  const channels = $('channels');
  channels.replaceChildren();
  const chans = [...state.channels.values()].sort((a, b) => a.name.localeCompare(b.name));
  for (const c of chans) {
    const sub = c.public ? 'Public channel' : 'Private channel';
    const kind = c.public ? 'public' : 'private';
    channels.append(listItem(`g:${c.id}`, icon(kind), hue(c.id), c.name, sub, 'channel'));
  }

  const contacts = $('contacts');
  contacts.replaceChildren();
  const items = [...state.contacts.values()].sort((a, b) => {
    const bots = (c) => (c.name.endsWith(' bot') ? 0 : 1);
    return bots(a) - bots(b) || a.name.localeCompare(b.name);
  });
  for (const c of items) {
    const sub = `${shortAddr(c.addr)} · ${routeText(c.hops)}`;
    contacts.append(listItem(`c:${c.addr}`, c.name.slice(0, 1).toUpperCase(), hue(c.addr), c.name, sub));
  }
  $('contact-count').textContent = items.length ? String(items.length) : '';
  $('contacts-empty').hidden = items.length > 0;
}

function select(key) {
  state.selected = key;
  state.unread.delete(key);
  showError('send-error');
  renderSidebar();
  renderChat();
  updateBytes();
  $('text').focus();
}

// ---- chat ----

function messagesFor(key) {
  if (!state.chats.has(key)) state.chats.set(key, []);
  return state.chats.get(key);
}

function renderChatHead() {
  const contact = selectedContact();
  const channel = selectedChannel();
  if (contact) {
    $('chat-name').textContent = contact.name;
    $('chat-addr').textContent = contact.addr;
    $('chat-route').textContent = routeText(contact.hops);
  } else if (channel) {
    $('chat-name').textContent = channel.name;
    $('chat-addr').textContent = channel.public
      ? 'Anyone who knows the name can read this channel.'
      : 'Only people with the invite key can read this channel.';
    $('chat-route').textContent = 'No delivery receipts';
  }
  $('chat-addr').classList.toggle('desc', !!channel);
  $('share-key').hidden = !channel || channel.public;
  $('attach').hidden = !!channel;
  $('leave').hidden = !channel;
}

function renderChat() {
  const open = state.selected !== null && (selectedContact() || selectedChannel());
  $('chat-empty').hidden = !!open;
  $('chat-main').hidden = !open;
  if (!open) return;
  renderChatHead();
  const group = isChannel(state.selected);
  const list = $('messages');
  list.replaceChildren();
  for (const m of messagesFor(state.selected)) {
    const li = el('li', `msg ${m.dir}`);
    if (group && m.dir === 'in') {
      const sender = el('div', m.verified ? 'sender' : 'sender unverified', m.name);
      if (!m.verified) sender.title = 'Signature not checked: we never got this sender\'s announce.';
      li.append(sender);
    }
    if (m.file) {
      li.append(fileBubble(m));
    } else {
      li.append(el('div', 'bubble', m.text));
    }
    let meta = fmtClock(m.at_ms);
    let cls = 'meta';
    if (m.file) {
      [meta, cls] = fileMeta(m, meta, cls);
    } else if (m.dir === 'out' && group) {
      meta += ' · Sent';
    } else if (m.dir === 'out') {
      if (m.status === 'delivered') {
        meta += ' · Delivered';
        cls += ' delivered';
      } else if (m.status === 'failed') {
        meta += ' · Not delivered';
        cls += ' failed';
      } else {
        meta += ' · Sending…';
      }
    }
    li.append(el('div', cls, meta));
    list.append(li);
  }
  list.scrollTop = list.scrollHeight;
}

function fileBubble(m) {
  const f = m.file;
  const b = el('div', 'bubble file');
  if (f.data && INLINE_IMAGES.includes(f.mime)) {
    const img = el('img', 'photo');
    img.src = `data:${f.mime};base64,${f.data}`;
    img.alt = f.name;
    b.append(img);
  } else {
    const card = el('div', 'file-card');
    card.append(icon('file'));
    const info = el('div');
    info.append(el('div', 'file-name', f.name));
    info.append(el('div', 'file-size', fmtSize(f.size)));
    card.append(info);
    b.append(card);
  }
  if ((m.status === 'sending' || m.status === 'receiving') && m.count) {
    const bar = el('div', 'progress');
    const fill = el('div');
    fill.style.width = `${Math.round((100 * m.done) / m.count)}%`;
    bar.append(fill);
    b.append(bar);
  }
  if (m.dir === 'in' && f.data) {
    const save = el('button', 'ghost small save', m.saved ? 'Saved' : 'Save…');
    save.title = m.saved || 'Choose where to save this file';
    save.addEventListener('click', () => saveFile(m));
    b.append(save);
  }
  return b;
}

function fileMeta(m, meta, cls) {
  const part = m.count ? ` ${m.done}/${m.count}` : '';
  switch (m.status) {
    case 'sending':
      return [`${meta} · Sending${part}`, cls];
    case 'sent':
      return [`${meta} · Delivered`, `${cls} delivered`];
    case 'receiving':
      return [`${meta} · Receiving${part}`, cls];
    case 'failed':
      return [`${meta} · ${m.dir === 'out' ? 'Not delivered' : 'Transfer failed'}`, `${cls} failed`];
    default:
      return [m.saved ? `${meta} · Saved to ${m.saved}` : meta, cls];
  }
}

// The newest file message in `peer`'s chat with transfer `id` in that direction.
function findFile(peer, id, dir) {
  const list = messagesFor(`c:${peer}`);
  for (let i = list.length - 1; i >= 0; i--) {
    const m = list[i];
    if (m.file && m.file.id === id && m.dir === dir) return m;
  }
  return null;
}

function updateFile(peer, id, dir, change) {
  const m = findFile(peer, id, dir);
  if (!m) return;
  Object.assign(m, change);
  if (state.selected === `c:${peer}`) renderChat();
}

async function saveFile(m) {
  try {
    // null: the save dialog was cancelled.
    const path = await invoke('save_file', { name: m.file.name, data: m.file.data });
    if (path) m.saved = path;
    showError('send-error');
  } catch (err) {
    showError('send-error', String(err));
  }
  renderChat();
}

function setOutgoingStatus(to, seq, status) {
  const key = `c:${to}`;
  const m = messagesFor(key).find((x) => x.dir === 'out' && x.seq === seq);
  if (!m) {
    state.early.set(`${to}:${seq}`, status);
    return;
  }
  m.status = status;
  if (key === state.selected) renderChat();
}

function updateBytes() {
  const n = encoder.encode($('text').value).length;
  const max = maxText();
  const b = $('bytes');
  const parts = n > max ? 0 : partCount($('text').value);
  b.textContent = parts > 1 ? `${n}/${max} bytes · ${parts} parts` : `${n}/${max} bytes`;
  b.classList.toggle('over', n > max);
  $('send').disabled = n === 0 || n > max;
}

function receive(key, msg) {
  messagesFor(key).push(msg);
  if (key === state.selected) {
    renderChat();
  } else {
    state.unread.set(key, (state.unread.get(key) || 0) + 1);
  }
  renderSidebar();
}

async function send() {
  const key = state.selected;
  const text = $('text').value;
  if (!key || !text.trim()) return;
  if (encoder.encode(text).length > maxText()) {
    showError('send-error', `Messages are limited to ${maxText()} bytes.`);
    return;
  }
  showError('send-error');
  const at_ms = state.me ? state.me.time_ms : 0;
  try {
    if (isChannel(key)) {
      await invoke('send_group', { id: key.slice(2), text });
      messagesFor(key).push({ dir: 'out', text, at_ms });
    } else {
      const to = key.slice(2);
      const seq = await invoke('send_text', { to, text });
      const early = `${to}:${seq}`;
      const status = state.early.get(early) || 'sending';
      state.early.delete(early);
      messagesFor(key).push({ dir: 'out', text, seq, at_ms, status });
    }
    $('text').value = '';
    updateBytes();
    if (key === state.selected) renderChat();
  } catch (err) {
    showError('send-error', String(err));
  }
}

// ---- files ----

function readDataUrl(blob) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(r.result);
    r.onerror = () => reject(r.error);
    r.readAsDataURL(blob);
  });
}

function loadImage(url) {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error('could not read the image'));
    img.src = url;
  });
}

function canvasBlob(canvas, type, quality) {
  return new Promise((resolve) => canvas.toBlob(resolve, type, quality));
}

// Scale a photo down and re-encode it until it is about PHOTO_TARGET bytes.
async function shrinkPhoto(file) {
  const img = await loadImage(await readDataUrl(file));
  const canvas = document.createElement('canvas');
  let side = PHOTO_MAX_SIDE;
  let best = null;
  while (side >= 120) {
    const scale = Math.min(1, side / Math.max(img.naturalWidth, img.naturalHeight));
    canvas.width = Math.max(1, Math.round(img.naturalWidth * scale));
    canvas.height = Math.max(1, Math.round(img.naturalHeight * scale));
    canvas.getContext('2d').drawImage(img, 0, 0, canvas.width, canvas.height);
    for (const q of [0.7, 0.55, 0.4]) {
      // WebKit has no WebP encoder and falls back to PNG; use JPEG there.
      let blob = await canvasBlob(canvas, 'image/webp', q);
      if (!blob || blob.type !== 'image/webp') blob = await canvasBlob(canvas, 'image/jpeg', q);
      if (!blob) continue;
      if (!best || blob.size < best.size) best = blob;
      if (blob.size <= PHOTO_TARGET) return blob;
    }
    side = Math.round(side * 0.75);
  }
  return best;
}

function base64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
}

async function sendFile(file) {
  const key = state.selected;
  if (!key || isChannel(key)) return;
  let blob = file;
  let name = file.name || 'file';
  try {
    if (INLINE_IMAGES.includes(file.type) && file.type !== 'image/gif') {
      blob = await shrinkPhoto(file);
      if (!blob) throw new Error('could not shrink the photo');
      const ext = blob.type === 'image/webp' ? 'webp' : 'jpg';
      name = `${name.replace(/\.[^.]*$/, '')}.${ext}`;
    }
    if (blob.size > MAX_FILE) {
      showError('send-error', `Files are limited to ${MAX_FILE / 1024} KB; this one is ${fmtSize(blob.size)}.`);
      return;
    }
    const data = base64(new Uint8Array(await blob.arrayBuffer()));
    const mime = blob.type || 'application/octet-stream';
    const to = key.slice(2);
    const id = await invoke('send_file', { to, name, mime, data });
    showError('send-error');
    messagesFor(key).push({
      dir: 'out',
      file: { id, name, mime, size: blob.size, data },
      at_ms: state.me ? state.me.time_ms : 0,
      status: 'sending',
      done: 0,
      count: Math.ceil(blob.size / (MAX_DATA - 5)), // chunk header is 5 bytes
    });
    if (key === state.selected) renderChat();
  } catch (err) {
    showError('send-error', String(err.message || err));
  }
}

// ---- radio log ----

function logRow(e) {
  const tr = el('tr', e.dir);
  const signal = e.snr_db === null || e.snr_db === undefined ? '' : `${e.snr_db.toFixed(1)} dB`;
  const result = e.dir === 'tx' ? (e.airtime_ms ? `${e.airtime_ms.toFixed(0)} ms` : '') : e.outcome || '';
  const cells = [
    fmtClock(e.at_ms),
    e.dir === 'tx' ? 'TX' : 'RX',
    e.dir === 'tx' ? '' : e.peer,
    e.kind + (e.routed ? ' (routed)' : ''),
    String(e.len),
    e.path_len ? String(e.path_len) : '',
    signal,
    result,
  ];
  cells.forEach((text, i) => {
    tr.append(el('td', i === 7 && e.outcome ? e.outcome : '', text));
  });
  return tr;
}

function addLog(e) {
  state.log.unshift(e);
  const body = $('log');
  body.prepend(logRow(e));
  while (state.log.length > LOG_LIMIT) {
    state.log.pop();
    body.lastElementChild.remove();
  }
}

// ---- events and status ----

function onEvent(ev) {
  switch (ev.type) {
    case 'contact':
      upsertContact(ev.contact);
      renderSidebar();
      if (state.selected === `c:${ev.contact.addr}`) renderChatHead();
      break;
    case 'message':
      if (!state.contacts.has(ev.from)) {
        upsertContact({ addr: ev.from, name: `Unknown ${shortAddr(ev.from)}`, hops: null });
      }
      receive(`c:${ev.from}`, { dir: 'in', text: ev.text, seq: ev.seq, at_ms: ev.at_ms });
      break;
    case 'group_message':
      if (!state.channels.has(ev.channel)) break; // left meanwhile
      receive(`g:${ev.channel}`, {
        dir: 'in',
        text: ev.text,
        at_ms: ev.at_ms,
        // Names come from the sender's signed announce, never from the post.
        verified: ev.from_addr !== null,
        name: ev.from_addr !== null ? ev.name || `Node ${ev.from}` : `Unverified sender ${ev.from}`,
      });
      break;
    case 'delivered':
      setOutgoingStatus(ev.to, ev.seq, 'delivered');
      break;
    case 'failed':
      setOutgoingStatus(ev.to, ev.seq, 'failed');
      break;
    case 'file_offered':
      if (!state.contacts.has(ev.from)) {
        upsertContact({ addr: ev.from, name: `Unknown ${shortAddr(ev.from)}`, hops: null });
      }
      receive(`c:${ev.from}`, {
        dir: 'in',
        file: { id: ev.id, name: ev.name, mime: ev.mime, size: ev.size },
        at_ms: ev.at_ms,
        status: 'receiving',
        done: 0,
        count: 0,
      });
      break;
    case 'file_progress':
      updateFile(ev.peer, ev.id, ev.outgoing ? 'out' : 'in', { done: ev.done, count: ev.count });
      break;
    case 'file_received': {
      const m = findFile(ev.from, ev.id, 'in');
      if (m) {
        m.file.data = ev.data;
        m.status = 'received';
        m.at_ms = ev.at_ms;
        if (state.selected === `c:${ev.from}`) renderChat();
      }
      break;
    }
    case 'file_sent':
      updateFile(ev.to, ev.id, 'out', { status: 'sent' });
      break;
    case 'file_failed':
      updateFile(ev.peer, ev.id, ev.outgoing ? 'out' : 'in', { status: 'failed' });
      break;
    case 'radio':
      addLog(ev);
      break;
  }
}

function applySnapshot(snap) {
  const s = snap.status;
  state.me = s;
  $('me-name').textContent = s.name;
  $('copy-addr').textContent = shortAddr(s.addr);
  $('copy-addr').dataset.addr = s.addr;
  $('backend').textContent =
    s.backend === 'virtual' ? `Simulated mesh · ${s.nodes} nodes` : s.backend;
  $('airtime').textContent = `${s.airtime_pct.toFixed(2)}%`;
  $('clock').textContent = fmtClock(s.time_ms);
  for (const c of snap.contacts) upsertContact(c);
  state.channels = new Map(snap.channels.map((c) => [c.id, c]));
  renderSidebar();
  if (state.selected) renderChatHead();
}

async function refresh() {
  try {
    applySnapshot(await invoke('snapshot'));
  } catch (err) {
    console.error('snapshot failed', err);
  }
}

async function copyText(btn, text, label) {
  try {
    await navigator.clipboard.writeText(text);
    btn.textContent = 'Copied';
  } catch {
    btn.textContent = text; // fall back to showing it
  }
  setTimeout(() => {
    btn.textContent = label;
  }, 1200);
}

// ---- wiring ----

$('composer').addEventListener('submit', (e) => {
  e.preventDefault();
  send();
});
$('text').addEventListener('input', updateBytes);
$('text').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    send();
  }
});

$('new-chat').addEventListener('submit', (e) => {
  e.preventDefault();
  const addr = $('new-addr').value.replace(/[\s:]/g, '').toLowerCase();
  if (!/^[0-9a-f]{32}$/.test(addr)) {
    showError('new-chat-error', 'An address is 32 hex characters.');
    return;
  }
  if (state.me && addr === state.me.addr) {
    showError('new-chat-error', 'That is your own address.');
    return;
  }
  showError('new-chat-error');
  if (!state.contacts.has(addr)) {
    upsertContact({ addr, name: `Unknown ${shortAddr(addr)}`, hops: null });
  }
  $('new-addr').value = '';
  select(`c:${addr}`);
});

$('join-channel').addEventListener('submit', async (e) => {
  e.preventDefault();
  const name = $('channel-name').value.trim();
  const key = $('channel-key').value.trim() || null;
  if (!name) {
    showError('join-error', 'Type a channel name, like #public.');
    return;
  }
  try {
    const info = await invoke('join_channel', { name, key });
    showError('join-error');
    state.channels.set(info.id, info);
    $('channel-name').value = '';
    $('channel-key').value = '';
    select(`g:${info.id}`);
  } catch (err) {
    showError('join-error', String(err));
  }
});

$('share-key').addEventListener('click', () => {
  const c = selectedChannel();
  if (c) copyText($('share-key'), c.key, 'Copy invite key');
});

$('leave').addEventListener('click', async () => {
  const c = selectedChannel();
  if (!c) return;
  await invoke('leave_channel', { id: c.id });
  state.channels.delete(c.id);
  state.chats.delete(`g:${c.id}`);
  state.unread.delete(`g:${c.id}`);
  state.selected = null;
  renderSidebar();
  renderChat();
});

$('copy-addr').addEventListener('click', () => {
  const btn = $('copy-addr');
  const addr = btn.dataset.addr;
  if (!addr) return;
  btn.classList.add('copied');
  copyText(btn, addr, shortAddr(addr)).then(() =>
    setTimeout(() => btn.classList.remove('copied'), 1200),
  );
});

$('announce').addEventListener('click', () => invoke('announce'));

$('attach').append(icon('attach'));
$('attach').addEventListener('click', () => $('file-input').click());
$('file-input').addEventListener('change', () => {
  const file = $('file-input').files[0];
  $('file-input').value = '';
  if (file) sendFile(file);
});

$('toggle-log').addEventListener('click', () => {
  const btn = $('toggle-log');
  const on = btn.getAttribute('aria-pressed') !== 'true';
  btn.setAttribute('aria-pressed', String(on));
  $('radio').hidden = !on;
});

listen('nmw-event', (e) => onEvent(e.payload));
updateBytes();
refresh();
setInterval(refresh, 2000);

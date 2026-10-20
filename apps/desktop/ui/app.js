// NMW desktop UI. The Rust side owns the mesh; this file only renders what
// it reports (`snapshot` polling plus `nmw-event` events) and forwards
// sends. All user-supplied text is rendered with textContent.
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const MAX_TEXT = 181; // MAX_DATA in nmw-core
const LOG_LIMIT = 300;
const $ = (id) => document.getElementById(id);

const state = {
  me: null,
  contacts: new Map(), // addr -> {addr, name, hops, unread}
  chats: new Map(), // addr -> [{dir, text, seq, at_ms, status}]
  early: new Map(), // `${to}:${seq}` -> status that arrived before send_text returned
  selected: null,
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

// ---- contacts ----

function upsertContact(c) {
  const prev = state.contacts.get(c.addr);
  // A placeholder made from a typed address keeps its unread count but takes the real name.
  state.contacts.set(c.addr, { ...prev, ...c, unread: prev ? prev.unread : 0 });
}

function renderContacts() {
  const list = $('contacts');
  list.replaceChildren();
  const items = [...state.contacts.values()].sort((a, b) => {
    const bots = (c) => (c.name.endsWith(' bot') ? 0 : 1);
    return bots(a) - bots(b) || a.name.localeCompare(b.name);
  });
  for (const c of items) {
    const li = el('li', 'contact' + (c.addr === state.selected ? ' selected' : ''));
    li.dataset.addr = c.addr;
    li.tabIndex = 0;
    const avatar = el('span', 'avatar', c.name.slice(0, 1).toUpperCase());
    avatar.style.background = `hsl(${parseInt(c.addr.slice(0, 4), 16) % 360} 45% 42%)`;
    li.append(avatar);
    const info = el('div', 'info');
    info.append(el('div', 'name', c.name));
    info.append(el('div', 'sub', `${shortAddr(c.addr)} · ${routeText(c.hops)}`));
    li.append(info);
    if (c.unread) li.append(el('span', 'unread', String(c.unread)));
    li.addEventListener('click', () => selectContact(c.addr));
    li.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') selectContact(c.addr);
    });
    list.append(li);
  }
  $('contact-count').textContent = items.length ? String(items.length) : '';
  $('contacts-empty').hidden = items.length > 0;
}

function selectContact(addr) {
  state.selected = addr;
  const c = state.contacts.get(addr);
  if (c) c.unread = 0;
  showError('send-error');
  renderContacts();
  renderChat();
  $('text').focus();
}

// ---- chat ----

function messagesFor(addr) {
  if (!state.chats.has(addr)) state.chats.set(addr, []);
  return state.chats.get(addr);
}

function renderChatHead() {
  const c = state.contacts.get(state.selected);
  if (!c) return;
  $('chat-name').textContent = c.name;
  $('chat-addr').textContent = c.addr;
  $('chat-route').textContent = routeText(c.hops);
}

function renderChat() {
  const open = state.selected !== null;
  $('chat-empty').hidden = open;
  $('chat-main').hidden = !open;
  if (!open) return;
  renderChatHead();
  const list = $('messages');
  list.replaceChildren();
  for (const m of messagesFor(state.selected)) {
    const li = el('li', `msg ${m.dir}`);
    li.append(el('div', 'bubble', m.text));
    let meta = fmtClock(m.at_ms);
    let cls = 'meta';
    if (m.dir === 'out') {
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

function setOutgoingStatus(to, seq, status) {
  const m = messagesFor(to).find((x) => x.dir === 'out' && x.seq === seq);
  if (!m) {
    state.early.set(`${to}:${seq}`, status);
    return;
  }
  m.status = status;
  if (to === state.selected) renderChat();
}

function updateBytes() {
  const n = encoder.encode($('text').value).length;
  const b = $('bytes');
  b.textContent = `${n}/${MAX_TEXT} bytes`;
  b.classList.toggle('over', n > MAX_TEXT);
  $('send').disabled = n === 0 || n > MAX_TEXT;
}

async function send() {
  const to = state.selected;
  const text = $('text').value;
  if (!to || !text.trim()) return;
  if (encoder.encode(text).length > MAX_TEXT) {
    showError('send-error', `Messages are limited to ${MAX_TEXT} bytes.`);
    return;
  }
  showError('send-error');
  try {
    const seq = await invoke('send_text', { to, text });
    const key = `${to}:${seq}`;
    const status = state.early.get(key) || 'sending';
    state.early.delete(key);
    messagesFor(to).push({ dir: 'out', text, seq, at_ms: state.me ? state.me.time_ms : 0, status });
    $('text').value = '';
    updateBytes();
    if (to === state.selected) renderChat();
  } catch (err) {
    showError('send-error', String(err));
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
    const td = el('td', i === 7 && e.outcome ? e.outcome : '', text);
    tr.append(td);
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
      renderContacts();
      if (ev.contact.addr === state.selected) renderChatHead();
      break;
    case 'message': {
      if (!state.contacts.has(ev.from)) {
        upsertContact({ addr: ev.from, name: `Unknown ${shortAddr(ev.from)}`, hops: null });
      }
      messagesFor(ev.from).push({ dir: 'in', text: ev.text, seq: ev.seq, at_ms: ev.at_ms });
      if (ev.from === state.selected) {
        renderChat();
      } else {
        state.contacts.get(ev.from).unread += 1;
      }
      renderContacts();
      break;
    }
    case 'delivered':
      setOutgoingStatus(ev.to, ev.seq, 'delivered');
      break;
    case 'failed':
      setOutgoingStatus(ev.to, ev.seq, 'failed');
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
  renderContacts();
  if (state.selected) renderChatHead();
}

async function refresh() {
  try {
    applySnapshot(await invoke('snapshot'));
  } catch (err) {
    console.error('snapshot failed', err);
  }
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
  selectContact(addr);
});

$('copy-addr').addEventListener('click', async () => {
  const btn = $('copy-addr');
  const addr = btn.dataset.addr;
  if (!addr) return;
  try {
    await navigator.clipboard.writeText(addr);
    btn.classList.add('copied');
    btn.textContent = 'Copied';
  } catch {
    btn.textContent = addr; // fall back to showing it in full
  }
  setTimeout(() => {
    btn.classList.remove('copied');
    btn.textContent = shortAddr(addr);
  }, 1200);
});

$('announce').addEventListener('click', () => invoke('announce'));

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

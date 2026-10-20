// NMW desktop UI. The Rust side owns the mesh; this file only renders what
// it reports (`snapshot` polling plus `nmw-event` events) and forwards
// sends. All user-supplied text is rendered with textContent.
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const MAX_TEXT = 181; // MAX_DATA in nmw-core
const MAX_GROUP_TEXT = 182; // MAX_GROUP_DATA in nmw-core
const LOG_LIMIT = 300;
const $ = (id) => document.getElementById(id);

// A conversation is a contact (`c:<addr>`) or a channel (`g:<id>`).
const state = {
  me: null,
  contacts: new Map(), // addr -> {addr, name, hops}
  channels: new Map(), // id -> {id, name, public, key}
  chats: new Map(), // conversation key -> [{dir, text, seq, at_ms, status, name}]
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

// ---- sidebar ----

function upsertContact(c) {
  const prev = state.contacts.get(c.addr);
  // A placeholder made from a typed address takes the real name later.
  state.contacts.set(c.addr, { ...prev, ...c });
}

function listItem(key, avatarText, avatarHue, name, sub, extraCls) {
  const li = el('li', 'contact' + (key === state.selected ? ' selected' : ''));
  li.dataset.key = key;
  li.tabIndex = 0;
  const avatar = el('span', 'avatar' + (extraCls ? ` ${extraCls}` : ''), avatarText);
  avatar.style.background = `hsl(${avatarHue} 45% 42%)`;
  li.append(avatar);
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
    channels.append(listItem(`g:${c.id}`, '#', hue(c.id), c.name, sub, 'channel'));
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
    if (group && m.dir === 'in') li.append(el('div', 'sender', m.name));
    li.append(el('div', 'bubble', m.text));
    let meta = fmtClock(m.at_ms);
    let cls = 'meta';
    if (m.dir === 'out' && group) {
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
  b.textContent = `${n}/${max} bytes`;
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
        name: ev.name || `Unknown ${ev.from}`,
      });
      break;
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

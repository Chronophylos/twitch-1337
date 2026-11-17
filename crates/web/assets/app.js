const CSRF_COOKIE_RE = /(?:^|; )tw1337_csrf=([^;]+)/;

document.body.addEventListener('htmx:configRequest', (evt) => {
  if (evt.detail.verb !== 'get' && !evt.detail.headers['X-Csrf-Token']) {
    const m = document.cookie.match(CSRF_COOKIE_RE);
    if (m) evt.detail.headers['X-Csrf-Token'] = decodeURIComponent(m[1]);
  }
});

(function berlinTimer() {
  const root = document.getElementById('berlin-timer');
  if (!root) return;
  const labelEl = root.querySelector('.timer-label');
  const clockEl = root.querySelector('.timer-clock');
  const tzEl = root.querySelector('.timer-tz');
  if (!labelEl || !clockEl) return;

  // en-GB names Europe/Berlin "CET" / "CEST"; en-US would say "GMT+1".
  const tzFmt = new Intl.DateTimeFormat('en-GB', {
    timeZone: 'Europe/Berlin',
    timeZoneName: 'short',
  });
  function berlinTzAbbr(d) {
    return tzFmt.formatToParts(d).find((p) => p.type === 'timeZoneName')?.value ?? '';
  }

  const fmt = new Intl.DateTimeFormat('en-GB', {
    timeZone: 'Europe/Berlin',
    hour12: false,
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });

  function berlinHMS(d) {
    const parts = fmt.formatToParts(d);
    const get = (t) => parts.find((p) => p.type === t).value;
    return [+get('hour'), +get('minute'), +get('second')];
  }

  function pad(n) {
    return String(n).padStart(2, '0');
  }

  function tick() {
    const now = new Date();
    if (tzEl) tzEl.textContent = berlinTzAbbr(now);
    const [h, m, s] = berlinHMS(now);
    const armed = h === 13 && m === 37;
    if (armed) {
      root.classList.add('armed');
      labelEl.textContent = '1337 ARMED';
      clockEl.textContent = `13:37:${pad(s)}`;
    } else {
      root.classList.remove('armed');
      const nowSec = h * 3600 + m * 60 + s;
      const targetSec = 13 * 3600 + 37 * 60;
      let delta = targetSec - nowSec;
      if (delta <= 0) delta += 86400;
      const dh = Math.floor(delta / 3600);
      const dm = Math.floor((delta % 3600) / 60);
      const ds = delta % 60;
      labelEl.textContent = 'T-MINUS';
      clockEl.textContent = `${pad(dh)}:${pad(dm)}:${pad(ds)}`;
    }
  }
  tick();
  setInterval(tick, 1000);
})();

// ─── Platform modifier key ───────────────────────────────────────────────────
// Shortcuts use ⌘ on Apple platforms and Ctrl elsewhere; `[data-mod-key]`
// hints show whichever applies.
const IS_MAC = /mac|iphone|ipad/i.test(
  navigator.userAgentData?.platform ?? navigator.platform ?? '',
);
for (const el of document.querySelectorAll('[data-mod-key]')) {
  el.textContent = IS_MAC ? '⌘' : 'Ctrl';
}

// ─── Inline row filter ───────────────────────────────────────────────────────
// `input[data-row-filter]` (templates/row_filter.html) hides `[data-filter]`
// rows whose text blob doesn't contain the query.
(function liveFilter() {
  const input = document.querySelector('[data-row-filter]');
  if (!input) return;
  // Rows are looked up on every pass: htmx polls and "Load more" swap new
  // `[data-filter]` rows in after page load.
  function apply() {
    const q = input.value.trim().toLowerCase();
    for (const el of document.querySelectorAll('[data-filter]')) {
      const hay = (el.getAttribute('data-filter') || '').toLowerCase();
      el.style.display = !q || hay.includes(q) ? '' : 'none';
    }
  }
  input.addEventListener('input', apply);
  document.body.addEventListener('htmx:afterSettle', () => {
    if (input.value.trim()) apply();
  });
})();

// Predicates that report unsaved edits; consulted on `beforeunload`.
const unsavedGuards = [];

(function settingsPage() {
  const form = document.getElementById('settings-form');
  const saveBar = document.getElementById('settings-save-bar');
  if (!form || !saveBar) return;

  const countEl = saveBar.querySelector('[data-count]');
  const nounEl = saveBar.querySelector('[data-noun]');
  const previewEl = saveBar.querySelector('[data-preview]');
  const discardBtn = saveBar.querySelector('[data-discard]');

  // Baselines are captured at load. `#settings-form` carries
  // autocomplete="off" so Firefox can't restore pre-reload edits before this
  // runs and have them mistaken for the saved values.
  const rows = Array.from(form.querySelectorAll('.settings-row'))
    .map((row) => {
      const inputs = Array.from(row.querySelectorAll('input[name], select[name], textarea[name]'));
      const isRadio = inputs.length > 1 && inputs.every((i) => i.type === 'radio');
      const primary = inputs[0];
      if (!primary) return null;
      const baseline = isRadio
        ? (inputs.find((i) => i.checked)?.value ?? '')
        : primary.type === 'checkbox' ? (primary.checked ? 'true' : 'false') : primary.value;
      return {
        el: row,
        input: primary,
        inputs,
        initial: inputs.map((i) => ({ checked: i.checked, value: i.value })),
        isRadio,
        reset: row.querySelector('.row-reset'),
        section: row.dataset.section,
        baseline,
      };
    })
    .filter((r) => r);
  let dirtyCount = 0;
  unsavedGuards.push(() => dirtyCount > 0);


  const BYTES_UNIT_FACTOR = { B: 1, KiB: 1024, MiB: 1024 * 1024 };

  function pickBestBytesUnit(n) {
    if (n >= BYTES_UNIT_FACTOR.MiB) return 'MiB';
    if (n >= BYTES_UNIT_FACTOR.KiB) return 'KiB';
    return 'B';
  }

  function formatBytesIec(bytes) {
    const n = Number(bytes);
    if (!Number.isFinite(n)) return String(bytes);
    const unit = pickBestBytesUnit(n);
    const f = BYTES_UNIT_FACTOR[unit];
    const v = n / f;
    return `${n % f === 0 ? v : v.toFixed(2)} ${unit}`;
  }

  const cards = Array.from(form.querySelectorAll('.settings-card'));
  const navItems = Array.from(document.querySelectorAll('.settings-nav-item'));
  const navGroups = Array.from(document.querySelectorAll('.settings-nav-group')).map((el) => ({
    el,
    items: Array.from(el.querySelectorAll('.settings-nav-item')),
    badge: el.querySelector('.gdirty'),
  }));

  const STORAGE_KEY = 'settings-nav-groups-collapsed';
  let collapsed = new Set();
  try { collapsed = new Set(JSON.parse(localStorage.getItem(STORAGE_KEY) || '[]')); } catch { /* ignore corrupt JSON, start fresh */ }
  for (const { el } of navGroups) {
    const open = !collapsed.has(el.dataset.group);
    el.dataset.open = open ? 'true' : 'false';
    el.querySelector('.settings-nav-group-head')?.setAttribute('aria-expanded', open ? 'true' : 'false');
  }
  for (const head of document.querySelectorAll('[data-group-toggle]')) {
    head.addEventListener('click', () => {
      const group = head.closest('.settings-nav-group');
      if (!group) return;
      const id = group.dataset.group;
      const open = group.dataset.open === 'false';
      group.dataset.open = open ? 'true' : 'false';
      head.setAttribute('aria-expanded', open ? 'true' : 'false');
      if (open) collapsed.delete(id); else collapsed.add(id);
      localStorage.setItem(STORAGE_KEY, JSON.stringify([...collapsed]));
    });
  }

  function currentValue(input) {
    return input.type === 'checkbox' ? (input.checked ? 'true' : 'false') : input.value;
  }

  function rowCurrent(r) {
    if (r.isRadio) return r.inputs.find((i) => i.checked)?.value ?? '';
    return currentValue(r.input);
  }

  function applyDefault(r) {
    const def = r.input.dataset.default ?? '';
    if (r.isRadio) {
      for (const i of r.inputs) i.checked = i.value === def;
      return;
    }
    if (r.input.type === 'checkbox') {
      r.input.checked = def === 'true' || def === '1';
    } else {
      r.input.value = def;
    }
  }

  function refreshAll() {
    const dirtyKeys = [];
    const perSection = new Map();
    for (const r of rows) {
      const cur = rowCurrent(r);
      const dirty = cur !== r.baseline;
      const offDefault = cur !== (r.input.dataset.default ?? '');
      r.el.classList.toggle('is-dirty', dirty);
      if (r.reset) r.reset.hidden = !offDefault;
      if (r.input.type === 'range') {
        const valEl = r.el.querySelector('[data-range-value]');
        if (valEl) {
          const n = Number(r.input.value);
          valEl.textContent = Number.isFinite(n) ? `${Math.round(n * 100)}%` : r.input.value;
        }
      }
      if (!dirty) continue;
      dirtyKeys.push(r.input.dataset.key ?? r.input.name);
      if (r.section) perSection.set(r.section, (perSection.get(r.section) ?? 0) + 1);
    }

    for (const card of cards) {
      const n = perSection.get(card.dataset.section) ?? 0;
      const badge = card.querySelector('.card-dirty');
      if (badge) {
        badge.hidden = n === 0;
        badge.textContent = `${n} modified`;
      }
    }
    for (const item of navItems) {
      const n = perSection.get(item.dataset.target) ?? 0;
      const badge = item.querySelector('.ndirty');
      if (badge) {
        badge.hidden = n === 0;
        badge.textContent = String(n);
      }
    }

    for (const g of navGroups) {
      let n = 0;
      for (const item of g.items) n += perSection.get(item.dataset.target) ?? 0;
      if (g.badge) {
        g.badge.hidden = n === 0;
        g.badge.textContent = String(n);
      }
    }

    const total = dirtyKeys.length;
    dirtyCount = total;
    countEl.textContent = String(total);
    nounEl.textContent = total === 1 ? 'change' : 'changes';
    const preview = dirtyKeys.slice(0, 3).join(' · ');
    previewEl.textContent =
      total > 3 ? `${preview} +${total - 3}` : preview;
    saveBar.classList.toggle('visible', total > 0);
  }

  const bytesRows = Array.from(form.querySelectorAll('[data-bytes-row]'))
    .map((el) => ({
      el,
      hidden: el.querySelector('.bytes-canonical'),
      display: el.querySelector('.bytes-display'),
      buttons: Array.from(el.querySelectorAll('.bytes-unit')),
    }))
    .filter((b) => b.hidden && b.display);
  const bytesRowByHidden = new Map(bytesRows.map((b) => [b.hidden, b]));

  function displayInUnit(bytes, unit) {
    return parseFloat((bytes / BYTES_UNIT_FACTOR[unit]).toFixed(6)).toString();
  }

  // Magnitude rows (alerts.quake): hidden canonical input stores integer
  // tenths, the visible input shows the decimal magnitude.
  const magRows = Array.from(form.querySelectorAll('[data-mag-row]'))
    .map((el) => ({
      el,
      hidden: el.querySelector('.mag-canonical'),
      display: el.querySelector('.mag-display'),
    }))
    .filter((m) => m.hidden && m.display);
  const magRowByHidden = new Map(magRows.map((m) => [m.hidden, m]));

  function formatMag(tenths) {
    return (Number(tenths) / 10).toFixed(1);
  }


  function commitMag(m, magnitude) {
    const clamped = Math.max(0, Math.min(100, Math.round(magnitude * 10)));
    if (Number(m.hidden.value) === clamped) return;
    m.hidden.value = String(clamped);
    m.hidden.dispatchEvent(new Event('input', { bubbles: true }));
  }

  function syncMagRow(m) {
    if (document.activeElement === m.display) return;
    m.display.value = formatMag(m.hidden.value);
  }

  function activeUnit(b) {
    return b.buttons.find((btn) => btn.classList.contains('is-active'))?.dataset.unit
      ?? pickBestBytesUnit(Number(b.hidden.value) || 0);
  }

  function syncBytesRow(b) {
    const bytes = Number(b.hidden.value) || 0;
    const unit = activeUnit(b);
    if (document.activeElement !== b.display) b.display.value = displayInUnit(bytes, unit);
    for (const btn of b.buttons) {
      const active = btn.dataset.unit === unit;
      btn.classList.toggle('is-active', active);
      btn.setAttribute('aria-pressed', active ? 'true' : 'false');
    }
  }

  function commitBytes(b, bytes) {
    const clamped = Math.max(0, Math.round(bytes));
    if (Number(b.hidden.value) === clamped) return;
    b.hidden.value = String(clamped);
    b.hidden.dispatchEvent(new Event('input', { bubbles: true }));
  }

  for (const b of bytesRows) {
    b.display.addEventListener('input', () => {
      const v = Number(b.display.value);
      if (!Number.isFinite(v) || v < 0) return;
      commitBytes(b, v * BYTES_UNIT_FACTOR[activeUnit(b)]);
    });
    b.display.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        syncBytesRow(b);
        b.display.blur();
      } else if (e.key === 'Enter') {
        e.preventDefault();
        b.display.blur();
      }
    });
    b.display.addEventListener('blur', () => syncBytesRow(b));

    for (const btn of b.buttons) {
      btn.addEventListener('click', () => {
        for (const other of b.buttons) other.classList.toggle('is-active', other === btn);
        syncBytesRow(b);
      });
    }
  }

  for (const b of bytesRows) {
    const def = b.el.querySelector('[data-bytes-default]');
    if (def) def.textContent = formatBytesIec(def.dataset.bytesDefault);
    syncBytesRow(b);
  }

  for (const m of magRows) {
    m.display.addEventListener('input', () => {
      const v = Number(m.display.value);
      if (!Number.isFinite(v) || v < 0) return;
      commitMag(m, v);
    });
    m.display.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        syncMagRow(m);
        m.display.blur();
      } else if (e.key === 'Enter') {
        e.preventDefault();
        m.display.blur();
      }
    });
    m.display.addEventListener('blur', () => syncMagRow(m));
  }

  for (const m of magRows) {
    const def = m.el.querySelector('[data-mag-default]');
    if (def) def.textContent = formatMag(def.dataset.magDefault);
    syncMagRow(m);
  }

  form.addEventListener('input', refreshAll);
  form.addEventListener('change', (e) => {
    const t = e.target;
    if (t instanceof HTMLInputElement && t.type === 'radio') {
      const group = form.querySelectorAll(`input[type=radio][name="${t.name}"]`);
      for (const i of group) {
        i.closest('.segment')?.classList.toggle('is-active', i.checked);
      }
    }
    refreshAll();
  });

  form.addEventListener('click', (e) => {
    const btn = e.target.closest('.row-reset');
    if (!btn) return;
    const r = rows.find((row) => row.reset === btn);
    if (!r) return;
    applyDefault(r);
    const b = bytesRowByHidden.get(r.input);
    if (b) syncBytesRow(b);
    const m = magRowByHidden.get(r.input);
    if (m) syncMagRow(m);
    refreshAll();
    r.input.focus();
  });

  // Restore every control to its load-time value in place. A reload would
  // let Firefox's form restoration put the edits straight back.
  discardBtn?.addEventListener('click', () => {
    for (const r of rows) {
      r.inputs.forEach((input, i) => {
        input.checked = r.initial[i].checked;
        input.value = r.initial[i].value;
      });
    }
    for (const b of bytesRows) syncBytesRow(b);
    for (const m of magRows) syncMagRow(m);
    for (const seg of form.querySelectorAll('.segmented .segment')) {
      seg.classList.toggle('is-active', !!seg.querySelector('input')?.checked);
    }
    for (const card of cards) syncCardEnabled(card);
    refreshAll();
  });

  // Phone-width section picker; mirrors the nav's active section.
  const jump = document.querySelector('[data-settings-jump]');

  if (cards.length && 'IntersectionObserver' in window) {
    const byId = new Map(navItems.map((el) => [el.dataset.target, el]));
    let active = null;
    const setActive = (section) => {
      if (section === active) return;
      if (active) byId.get(active)?.classList.remove('active');
      active = section;
      byId.get(section)?.classList.add('active');
      const activeItem = byId.get(section);
      const activeGroup = activeItem?.closest('.settings-nav-group');
      for (const g of navGroups) g.el.classList.toggle('has-active', g.el === activeGroup);
      if (jump) jump.value = section;
    };
    setActive(cards[0].dataset.section);
    const io = new IntersectionObserver(
      (entries) => {
        const vis = entries
          .filter((e) => e.isIntersecting)
          .sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top)[0];
        if (vis) setActive(vis.target.dataset.section);
      },
      { rootMargin: '-72px 0px -60% 0px' },
    );
    for (const card of cards) io.observe(card);
  }

  function scrollToSection(section) {
    const target = document.getElementById('sec-' + section);
    if (!target) return false;
    const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    target.scrollIntoView({ behavior: reduce ? 'auto' : 'smooth', block: 'start' });
    return true;
  }
  for (const item of navItems) {
    item.addEventListener('click', (e) => {
      if (scrollToSection(item.dataset.target)) e.preventDefault();
    });
  }
  jump?.addEventListener('change', () => scrollToSection(jump.value));

  refreshAll();
})();

// ─── Segmented selector active-state ─────────────────────────────────────────
document.body.addEventListener('change', (evt) => {
  const radio = evt.target;
  if (!(radio instanceof HTMLInputElement) || radio.type !== 'radio') return;
  const wrap = radio.closest('.segmented');
  if (!wrap) return;
  for (const seg of wrap.querySelectorAll('.segment')) {
    seg.classList.toggle('is-active', seg.querySelector('input').checked);
  }
});

// ─── Card toggle dimming ─────────────────────────────────────────────────────
function syncCardEnabled(card) {
  const toggle = card.querySelector('input[type=checkbox][data-card-toggle]');
  if (!toggle) return;
  const on = toggle.checked;
  card.classList.toggle('is-card-off', !on);
  for (const ctrl of card.querySelectorAll('[data-card-enabled-by]')) {
    if (
      ctrl instanceof HTMLInputElement ||
      ctrl instanceof HTMLButtonElement ||
      ctrl instanceof HTMLSelectElement ||
      ctrl instanceof HTMLTextAreaElement
    ) {
      ctrl.disabled = !on;
    } else {
      ctrl.classList.toggle('is-disabled', !on);
    }
  }
}
document.querySelectorAll('.settings-card[data-section]').forEach(syncCardEnabled);
document.body.addEventListener('change', (evt) => {
  if (evt.target.hasAttribute?.('data-card-toggle')) {
    syncCardEnabled(evt.target.closest('.settings-card'));
  }
});

// ─── Live character counters ─────────────────────────────────────────────────
// Any `<textarea data-char-count maxlength=N>` gets a live "n / N" readout in
// the sibling `[data-char-count-for]` within its `.row`; `.over` flags the cap.
for (const ta of document.querySelectorAll('textarea[data-char-count]')) {
  const counter = ta.closest('.row')?.querySelector('[data-char-count-for]');
  if (!counter) continue;
  const max = parseInt(ta.getAttribute('maxlength'), 10) || 500;
  const render = () => {
    counter.textContent = `${ta.value.length} / ${max}`;
    counter.classList.toggle('over', ta.value.length >= max);
  };
  ta.addEventListener('input', render);
  render();
}

// ─── Error toast ─────────────────────────────────────────────────────────────
// htmx does not swap 4xx/5xx responses, so failed requests would otherwise be
// silent. The server answers htmx errors with short plain text (see
// `error::htmx_errors`); anything else falls back to the status code.
(function errorToast() {
  const region = document.getElementById('toast');
  if (!region) return;
  const TOAST_MS = 6000;
  let timer;

  function hide() {
    clearTimeout(timer);
    region.replaceChildren();
  }

  function show(text) {
    hide();
    const card = document.createElement('div');
    card.className = 'toast';
    const msg = document.createElement('span');
    msg.className = 'toast-msg';
    msg.textContent = text;
    const close = document.createElement('button');
    close.type = 'button';
    close.className = 'toast-close';
    close.setAttribute('aria-label', 'Dismiss');
    close.textContent = '✕';
    close.addEventListener('click', hide);
    card.append(msg, close);
    region.append(card);
    timer = setTimeout(hide, TOAST_MS);
  }

  document.body.addEventListener('htmx:responseError', (evt) => {
    const xhr = evt.detail.xhr;
    const isText = (xhr.getResponseHeader('Content-Type') || '').startsWith('text/plain');
    const text = isText ? xhr.responseText.trim() : '';
    show(text && text.length <= 300 ? text : `Request failed (${xhr.status})`);
  });
  document.body.addEventListener('htmx:sendError', () => {
    show('Request failed (network error)');
  });
})();

// Error page "← Back": history.back() keeps the user's form input. A tab
// with no earlier entry (new tab, bookmark) would make Back a no-op, so go
// home instead.
document.addEventListener('click', (evt) => {
  if (!evt.target.closest?.('[data-history-back]')) return;
  if (history.length > 1) history.back();
  else location.assign('/');
});

// ─── Unsaved-changes guard ───────────────────────────────────────────────────
// `form[data-unsaved-guard]` is dirty when any enabled, user-editable control
// differs from its server-rendered default (`defaultValue` / `defaultChecked`
// / `defaultSelected`). Hidden inputs are skipped: they aren't user-editable.
function formDirty(form) {
  const radioGroups = new Map();
  for (const el of form.elements) {
    if (el.type === 'radio' && el.name) {
      if (!radioGroups.has(el.name)) radioGroups.set(el.name, []);
      radioGroups.get(el.name).push(el);
      continue;
    }
    if (!el.name || el.type === 'hidden' || el.matches(':disabled')) continue;
    if (el.type === 'checkbox') {
      if (el.checked !== el.defaultChecked) return true;
    } else if (el instanceof HTMLSelectElement && el.multiple) {
      if (Array.from(el.options).some((o) => o.selected !== o.defaultSelected)) return true;
    } else if (el instanceof HTMLSelectElement) {
      // No `selected` attribute means the browser shows the first option.
      const def = Array.from(el.options).find((o) => o.defaultSelected) ?? el.options[0];
      if (el.value !== (def?.value ?? '')) return true;
    } else if ('defaultValue' in el && el.value !== el.defaultValue) {
      return true;
    }
  }
  // A radio group spans the whole form, disabled fieldsets included (the
  // profile form repeats `reasoning_effort` per provider). The parser leaves
  // only the last `checked` radio of a group checked, so that is the default.
  for (const radios of radioGroups.values()) {
    const def = radios.findLast((r) => r.defaultChecked);
    const cur = radios.find((r) => r.checked);
    if (def !== cur) return true;
  }
  return false;
}
for (const form of document.querySelectorAll('form[data-unsaved-guard]')) {
  unsavedGuards.push(() => formDirty(form));
}

let leavingViaSubmit = false;
// Only submitting a guarded form saves its edits; any other POST on the page
// (a card toggle, "Add member") would navigate away and drop them. Bubble
// phase on window runs after per-form handlers, so a cancelled submit (e.g.
// a declined delete confirm) keeps the guard armed.
window.addEventListener('submit', (evt) => {
  if (evt.defaultPrevented) return;
  if (evt.target.matches?.('#settings-form, form[data-unsaved-guard]')) leavingViaSubmit = true;
});
// A page restored from the back/forward cache keeps its JS state; re-arm.
window.addEventListener('pageshow', (evt) => {
  if (evt.persisted) leavingViaSubmit = false;
});
window.addEventListener('beforeunload', (evt) => {
  if (leavingViaSubmit || !unsavedGuards.some((dirty) => dirty())) return;
  evt.preventDefault();
  evt.returnValue = '';
});

// ─── Ctrl/Cmd+S submits `form[data-save-shortcut]` ───────────────────────────
(function saveShortcut() {
  const form = document.querySelector('form[data-save-shortcut]');
  if (!form) return;
  document.addEventListener('keydown', (evt) => {
    // Chrome fires key-less keydown events for autofill.
    if (evt.key?.toLowerCase() !== 's' || evt.altKey || evt.shiftKey) return;
    if (!(IS_MAC ? evt.metaKey : evt.ctrlKey)) return;
    evt.preventDefault();
    form.requestSubmit();
  });
})();

// ─── Phone nav drawer ────────────────────────────────────────────────────────
// Below the app.css phone breakpoint the sidebar is off-canvas. ☰ opens it;
// Esc and the scrim close it. Focus moves into the drawer on open and back to
// ☰ on close; the page behind is inert while the drawer is up.
(function navDrawer() {
  const toggle = document.querySelector('[data-drawer-toggle]');
  const drawer = document.getElementById('sidebar');
  if (!toggle || !drawer) return;
  const page = toggle.closest('.main');
  const phone = window.matchMedia('(max-width: 860px)');
  const isOpen = () => document.body.classList.contains('drawer-open');

  function setOpen(open, restoreFocus = true) {
    document.body.classList.toggle('drawer-open', open);
    toggle.setAttribute('aria-expanded', String(open));
    if (page) page.inert = open;
    if (open) {
      const first =
        drawer.querySelector('.nav-list > li.active .nav-item, .nav-group-link.active') ||
        drawer.querySelector('a[href], button');
      first?.focus();
    } else if (restoreFocus) {
      toggle.focus();
    }
  }

  toggle.addEventListener('click', () => setOpen(!isOpen()));
  for (const el of document.querySelectorAll('[data-drawer-close]')) {
    el.addEventListener('click', () => setOpen(false));
  }
  document.addEventListener('keydown', (evt) => {
    if (evt.key !== 'Escape' || !isOpen()) return;
    evt.preventDefault();
    setOpen(false);
  });
  // Widening past the breakpoint turns the drawer back into the rail.
  phone.addEventListener('change', (evt) => {
    if (!evt.matches && isOpen()) setOpen(false, false);
  });
})();

// ─── Confirm before submit ───────────────────────────────────────────────────
// `form[data-confirm="…"]` asks first; declining cancels the submit. Runs on
// `document`, before the unsaved-guard listener on `window` checks
// `defaultPrevented`.
document.addEventListener('submit', (evt) => {
  const msg = evt.target.dataset?.confirm;
  if (msg && !window.confirm(msg)) evt.preventDefault();
});

// ─── Percent bars ────────────────────────────────────────────────────────────
// `[data-pct=N]` sizes itself to N% wide. Templates carry no `style=""`
// attributes so the dashboard can run under a strict CSP; setting the width
// through the CSSOM is not an inline style.
(function pctBars() {
  const apply = (root) => {
    for (const el of root.querySelectorAll('[data-pct]')) {
      const pct = Math.min(100, Math.max(0, Number(el.dataset.pct) || 0));
      el.style.width = `${pct}%`;
    }
  };
  apply(document);
  document.body.addEventListener('htmx:load', (evt) => apply(evt.detail.elt));
})();

// ─── Live byte counter (memory editor) ───────────────────────────────────────
// `textarea[data-byte-cap=N]` drives `[data-bytes-used]`, `[data-bytes-fill]`
// and `[data-bytes-warn]` in its `.editor`. Counts UTF-8 bytes, like the
// server-side cap, not UTF-16 `.length`.
(function byteCounters() {
  const utf8 = new TextEncoder();
  for (const ta of document.querySelectorAll('textarea[data-byte-cap]')) {
    const box = ta.closest('.editor');
    const cap = parseInt(ta.dataset.byteCap, 10);
    if (!box || Number.isNaN(cap)) continue;
    const meter = box.querySelector('.bytes');
    const used = box.querySelector('[data-bytes-used]');
    const fill = box.querySelector('[data-bytes-fill]');
    const warn = box.querySelector('[data-bytes-warn]');
    const render = () => {
      const n = utf8.encode(ta.value).length;
      const over = n > cap;
      if (used) used.textContent = String(n);
      if (fill) fill.style.width = `${cap > 0 ? Math.min(100, Math.floor((n * 100) / cap)) : 100}%`;
      if (warn) warn.hidden = !over;
      meter?.classList.toggle('over', over);
    };
    ta.addEventListener('input', render);
    render();
  }
})();

// ─── Logs tail (/logs) ───────────────────────────────────────────────────────
// `[data-log-poll]` polls every 2s. Ticks are skipped while the tab is hidden
// or Pause is pressed; the `after=` cursor catches up on the next live tick.
// The view stays pinned to the newest row if it was at the bottom, and the
// DOM is capped at the server ring's size.
(function logsTail() {
  const rows = document.getElementById('log-rows');
  if (!rows) return;
  const MAX_ROWS = 2000;
  const pause = document.querySelector('[data-log-pause]');
  let paused = false;
  pause?.addEventListener('click', () => {
    paused = !paused;
    pause.setAttribute('aria-pressed', String(paused));
    pause.textContent = paused ? 'Resume' : 'Pause';
  });
  document.body.addEventListener('htmx:configRequest', (evt) => {
    if (!evt.target.closest?.('[data-log-poll]')) return;
    if (paused || document.hidden) evt.preventDefault();
  });
  let pinned = true;
  document.body.addEventListener('htmx:beforeSwap', (evt) => {
    if (evt.detail.target !== rows) return;
    pinned = rows.scrollHeight - rows.scrollTop - rows.clientHeight < 24;
  });
  document.body.addEventListener('htmx:afterSwap', (evt) => {
    if (evt.detail.target !== rows) return;
    while (rows.childElementCount > MAX_ROWS) rows.firstElementChild.remove();
    if (pinned) rows.scrollTop = rows.scrollHeight;
  });
  rows.scrollTop = rows.scrollHeight;
})();

// ─── Jump palette ────────────────────────────────────────────────────────────
// Ctrl/⌘+K, or `/` outside a text field, opens `dialog#palette` over the
// sidebar pages, the settings sections (only rendered on /settings) and page
// actions marked `[data-palette]`. Choosing an entry clicks the original
// element, so in-page handlers (settings section scroll) still run.
(function jumpPalette() {
  const dialog = document.getElementById('palette');
  if (!dialog) return;
  const input = dialog.querySelector('[role="combobox"]');
  const list = dialog.querySelector('[role="listbox"]');
  const empty = dialog.querySelector('[data-palette-empty]');
  let entries = [];
  let shown = [];
  let active = 0;
  let returnFocus = null;

  // Visible text of `el`, skipping `hidden` / `aria-hidden` decorations
  // (nav dots, dirty counters). A non-empty `data-palette` value wins.
  function labelOf(el) {
    if (el.dataset.palette) return el.dataset.palette;
    let out = '';
    (function walk(node) {
      for (const c of node.childNodes) {
        if (c.nodeType === Node.TEXT_NODE) out += c.textContent;
        else if (c.nodeType === Node.ELEMENT_NODE && !c.hidden && c.getAttribute('aria-hidden') !== 'true') walk(c);
      }
    })(el);
    return out.replace(/\s+/g, ' ').replace(/^\+\s*/, '').trim();
  }

  // Read at open time so htmx swaps since page load are reflected.
  function collect() {
    const out = [];
    const seen = new Set();
    // `extra` is matched but not shown in the label (a section's group name).
    const add = (el, group, extra = '') => {
      if (el.closest('[hidden]')) return;
      const label = labelOf(el);
      const key = `${group}\n${label}`;
      if (!label || seen.has(key)) return;
      seen.add(key);
      out.push({ el, label, group, hay: `${label} ${extra}`.trim().toLowerCase() });
    };
    for (const el of document.querySelectorAll('[data-palette]')) add(el, 'Action');
    for (const el of document.querySelectorAll('#sidebar .nav a[href]')) add(el, 'Page');
    for (const el of document.querySelectorAll('.settings-nav-item')) {
      const head = el.closest('.settings-nav-group')?.querySelector('.settings-nav-group-head .lbl');
      const groupName = head ? head.textContent.trim() : '';
      add(el, groupName ? `Settings · ${groupName}` : 'Settings', groupName);
    }
    return out;
  }

  // Fuzzy score; null unless every whitespace-separated token of the query
  // is a subsequence of `hay` (tokens match in any order, so "agents grok"
  // finds the Grok section of the Agents group). Consecutive hits and hits at
  // word starts rank higher, as does a shorter label (so "sett" puts
  // Settings above longer labels with the same hits).
  function score(hay, tokens) {
    let total = 0;
    for (const token of tokens) {
      let pos = -1;
      let run = 0;
      for (const ch of token) {
        const i = hay.indexOf(ch, pos + 1);
        if (i < 0) return null;
        run = i === pos + 1 ? run + 1 : 0;
        total += 1 + run * 2;
        if (i === 0 || /[\s·/_.-]/.test(hay[i - 1])) total += 3;
        pos = i;
      }
    }
    return total - hay.length / 100;
  }

  function setActive(i) {
    active = i;
    for (const li of list.children) {
      li.setAttribute('aria-selected', String(Number(li.dataset.index) === i));
    }
    const cur = list.children[i];
    if (cur) {
      input.setAttribute('aria-activedescendant', cur.id);
      cur.scrollIntoView({ block: 'nearest' });
    } else {
      input.removeAttribute('aria-activedescendant');
    }
  }

  function render() {
    const tokens = input.value.toLowerCase().split(/\s+/).filter(Boolean);
    shown = tokens.length
      ? entries
        .map((e) => ({ e, s: score(e.hay, tokens) }))
        .filter((m) => m.s !== null)
        .sort((a, b) => b.s - a.s)
        .map((m) => m.e)
      : entries;
    list.replaceChildren(
      ...shown.map((e, i) => {
        const li = document.createElement('li');
        li.id = `palette-opt-${i}`;
        li.className = 'palette-opt';
        li.setAttribute('role', 'option');
        li.dataset.index = String(i);
        const label = document.createElement('span');
        label.className = 'palette-opt-label';
        label.textContent = e.label;
        const group = document.createElement('span');
        group.className = 'palette-opt-group';
        group.textContent = e.group;
        li.append(label, group);
        return li;
      }),
    );
    empty.hidden = shown.length > 0;
    setActive(0);
  }

  function open() {
    if (dialog.open) return;
    returnFocus = document.activeElement;
    entries = collect();
    input.value = '';
    render();
    dialog.showModal();
    input.focus();
  }

  function go(entry) {
    dialog.close();
    entry.el.click();
  }

  // Covers Esc (native `cancel`), a backdrop click and choosing an entry.
  dialog.addEventListener('close', () => {
    const el = returnFocus;
    returnFocus = null;
    if (el && el.isConnected && typeof el.focus === 'function') el.focus();
  });
  // The box fills the dialog, so a click on the dialog itself is the backdrop.
  dialog.addEventListener('click', (evt) => {
    if (evt.target === dialog) dialog.close();
  });

  input.addEventListener('input', render);
  input.addEventListener('keydown', (evt) => {
    if (evt.key === 'ArrowDown' || evt.key === 'ArrowUp') {
      evt.preventDefault();
      if (!shown.length) return;
      const step = evt.key === 'ArrowDown' ? 1 : -1;
      setActive((active + step + shown.length) % shown.length);
    } else if (evt.key === 'Enter') {
      evt.preventDefault();
      if (shown[active]) go(shown[active]);
    }
  });
  list.addEventListener('click', (evt) => {
    const li = evt.target.closest('[role="option"]');
    if (li) go(shown[Number(li.dataset.index)]);
  });
  list.addEventListener('mousemove', (evt) => {
    const li = evt.target.closest('[role="option"]');
    if (li && Number(li.dataset.index) !== active) setActive(Number(li.dataset.index));
  });

  for (const btn of document.querySelectorAll('[data-palette-open]')) {
    btn.addEventListener('click', open);
  }

  const typing = (el) =>
    !!el && (['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName) || el.isContentEditable);
  document.addEventListener(
    'keydown',
    (evt) => {
      if (evt.altKey) return;
      const isK = evt.key?.toLowerCase() === 'k' && !evt.shiftKey;
      if (isK && (IS_MAC ? evt.metaKey : evt.ctrlKey)) {
        evt.preventDefault();
        if (dialog.open) dialog.close();
        else open();
      } else if (evt.key === '/' && !evt.metaKey && !evt.ctrlKey && !dialog.open && !typing(evt.target)) {
        // No shiftKey check: `/` is Shift+7 on German layouts.
        evt.preventDefault();
        open();
      }
    },
    true,
  );
})();

// ─── Polling pauses in background tabs ───────────────────────────────────────
// `[data-poll-visible]` polls with `hx-trigger="every Ns, visibilitychange
// from:document"`: ticks in a hidden tab are dropped here, and the
// visibilitychange trigger refreshes at once when the tab comes back.
// (An `every 15s [!document.hidden]` filter would need eval, which the CSP
// rules out.)
document.body.addEventListener('htmx:beforeRequest', (evt) => {
  if (document.hidden && evt.detail.elt.matches?.('[data-poll-visible]')) evt.preventDefault();
});

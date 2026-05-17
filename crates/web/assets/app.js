const CSRF_COOKIE_RE = /(?:^|; )tw1337_csrf=([^;]+)/;

document.body.addEventListener('htmx:configRequest', (evt) => {
  if (evt.detail.verb !== 'get') {
    const m = document.cookie.match(CSRF_COOKIE_RE);
    if (m) evt.detail.headers['X-Csrf-Token'] = decodeURIComponent(m[1]);
  }
});

(function berlinTimer() {
  const root = document.getElementById('berlin-timer');
  if (!root) return;
  const labelEl = root.querySelector('.timer-label');
  const clockEl = root.querySelector('.timer-clock');
  if (!labelEl || !clockEl) return;

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
    const [h, m, s] = berlinHMS(new Date());
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

document.addEventListener(
  'keydown',
  (evt) => {
    if (evt.key !== '/' || evt.metaKey || evt.ctrlKey || evt.altKey) return;
    const tag = (evt.target && evt.target.tagName) || '';
    if (tag === 'INPUT' || tag === 'TEXTAREA' || (evt.target && evt.target.isContentEditable)) {
      return;
    }
    const search = document.getElementById('page-search');
    if (search) {
      evt.preventDefault();
      search.focus();
      search.select();
    }
  },
  true,
);

(function liveFilter() {
  const search = document.getElementById('page-search');
  if (!search) return;
  const items = Array.from(document.querySelectorAll('[data-filter]')).map((el) => ({
    el,
    hay: (el.getAttribute('data-filter') || '').toLowerCase(),
  }));
  if (items.length === 0) return;
  search.addEventListener('input', () => {
    const q = search.value.trim().toLowerCase();
    for (const { el, hay } of items) {
      el.style.display = !q || hay.includes(q) ? '' : 'none';
    }
  });
})();

(function settingsPage() {
  const form = document.getElementById('settings-form');
  const saveBar = document.getElementById('settings-save-bar');
  if (!form || !saveBar) return;

  const countEl = saveBar.querySelector('[data-count]');
  const nounEl = saveBar.querySelector('[data-noun]');
  const previewEl = saveBar.querySelector('[data-preview]');
  const discardBtn = saveBar.querySelector('[data-discard]');

  const rows = Array.from(form.querySelectorAll('.settings-row'))
    .map((row) => {
      const inputs = Array.from(row.querySelectorAll('input[name]'));
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
        isRadio,
        reset: row.querySelector('.row-reset'),
        pretty: row.querySelector('.row-pretty'),
        section: row.dataset.section,
        baseline,
      };
    })
    .filter((r) => r);


  const BYTES_UNIT_FACTOR = { B: 1, KiB: 1024, MiB: 1024 * 1024 };

  function formatBytesIec(bytes) {
    const n = Number(bytes);
    if (!Number.isFinite(n) || n < 0) return `${bytes} B`;
    if (n >= 1048576 && n % 1048576 === 0) return `${n / 1048576} MiB`;
    if (n >= 1024 && n % 1024 === 0) return `${n / 1024} KiB`;
    if (n >= 1048576) return `${(n / 1048576).toFixed(2)} MiB`;
    if (n >= 1024) return `${(n / 1024).toFixed(2)} KiB`;
    return `${n} B`;
  }

  function formatPretty(input, prettyEl) {
    const unit = prettyEl?.dataset.prettyUnit ?? '';
    if (unit === 'bytes') return formatBytesIec(input.value);
    if (unit === 'bool') return input.checked ? 'On' : 'Off';
    if (input.type === 'text' || input.type === 'time' || input.type === 'email' || input.type === 'url') {
      return input.value || (input.placeholder ? `(${input.placeholder})` : '');
    }
    const n = Number(input.value);
    if (!Number.isFinite(n)) return input.value;
    if (unit === 'pct') return `${Math.round(n * 100)}%`;
    if (unit === 's') return n === 1 ? '1 second' : `${n} seconds`;
    if (unit === 'B') {
      if (n % 1024 === 0 && n >= 1024) return `${n / 1024} KiB`;
      return `${n} B`;
    }
    return unit ? `${n} ${unit}` : String(n);
  }
  const cards = Array.from(form.querySelectorAll('.settings-card'));
  const navItems = Array.from(document.querySelectorAll('.settings-nav-item'));

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
      if (r.pretty) {
        const isText = ['text', 'time', 'email', 'url'].includes(r.input.type);
        // Server already renders nuanced markup (placeholder/empty muted span)
        // for text-type rows; only overwrite the pretty when the user has
        // edited the value, otherwise leave the SSR markup intact.
        if (!isText || dirty) r.pretty.textContent = formatPretty(r.input, r.pretty);
      }
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

    syncAllBytesRows();

    const total = dirtyKeys.length;
    countEl.textContent = String(total);
    nounEl.textContent = total === 1 ? 'change' : 'changes';
    const preview = dirtyKeys.slice(0, 3).join(' · ');
    previewEl.textContent =
      total > 3 ? `${preview} +${total - 3}` : preview;
    saveBar.classList.toggle('visible', total > 0);
  }

  const bytesRows = Array.from(form.querySelectorAll('[data-bytes-row]'));

  function pickBestUnit(bytes) {
    if (bytes >= 1048576 && bytes % 1048576 === 0) return 'MiB';
    if (bytes >= 1024 && bytes % 1024 === 0) return 'KiB';
    return 'B';
  }

  function displayInUnit(bytes, unit) {
    const f = BYTES_UNIT_FACTOR[unit] ?? 1;
    const v = bytes / f;
    return Number(v.toFixed(6)).toString();
  }

  function syncBytesRow(row) {
    const hidden = row.querySelector('.bytes-canonical');
    const display = row.querySelector('.bytes-display');
    const buttons = Array.from(row.querySelectorAll('.bytes-unit'));
    const auxBytes = row.querySelector('[data-bytes-aux-bytes]');
    const auxPretty = row.querySelector('[data-bytes-aux-pretty]');
    if (!hidden || !display) return;
    const bytes = Number(hidden.value) || 0;
    const unit = display.dataset.unit || pickBestUnit(bytes);
    display.dataset.unit = unit;
    if (document.activeElement !== display) {
      display.value = displayInUnit(bytes, unit);
    }
    for (const b of buttons) {
      const active = b.dataset.unit === unit;
      b.classList.toggle('is-active', active);
      b.setAttribute('aria-pressed', active ? 'true' : 'false');
    }
    if (auxBytes) auxBytes.textContent = String(bytes);
    if (auxPretty) auxPretty.textContent = formatBytesIec(bytes);
  }

  function syncAllBytesRows() {
    for (const r of bytesRows) syncBytesRow(r);
  }

  function commitBytes(row, bytes) {
    const hidden = row.querySelector('.bytes-canonical');
    if (!hidden) return;
    const clamped = Math.max(0, Math.round(bytes));
    if (Number(hidden.value) === clamped) return;
    hidden.value = String(clamped);
    hidden.dispatchEvent(new Event('input', { bubbles: true }));
  }

  for (const row of bytesRows) {
    const display = row.querySelector('.bytes-display');
    const buttons = Array.from(row.querySelectorAll('.bytes-unit'));
    const presets = Array.from(row.querySelectorAll('.bytes-preset'));

    display?.addEventListener('input', () => {
      const v = Number(display.value);
      if (!Number.isFinite(v) || v < 0) return;
      const f = BYTES_UNIT_FACTOR[display.dataset.unit || 'B'] ?? 1;
      commitBytes(row, v * f);
    });
    display?.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        syncBytesRow(row);
        display.blur();
      } else if (e.key === 'Enter') {
        e.preventDefault();
        display.blur();
      }
    });
    display?.addEventListener('blur', () => syncBytesRow(row));

    for (const b of buttons) {
      b.addEventListener('click', () => {
        const newUnit = b.dataset.unit;
        if (!newUnit || !display) return;
        display.dataset.unit = newUnit;
        syncBytesRow(row);
      });
    }
    for (const p of presets) {
      p.addEventListener('click', () => {
        const bytes = Number(p.dataset.bytes) || 0;
        if (display) display.dataset.unit = pickBestUnit(bytes);
        commitBytes(row, bytes);
      });
    }
  }

  syncAllBytesRows();

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
    refreshAll();
    r.input.focus();
  });

  discardBtn?.addEventListener('click', () => location.reload());

  if (cards.length && 'IntersectionObserver' in window) {
    const byId = new Map(navItems.map((el) => [el.dataset.target, el]));
    let active = null;
    const setActive = (section) => {
      if (section === active) return;
      if (active) byId.get(active)?.classList.remove('active');
      active = section;
      byId.get(section)?.classList.add('active');
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

  for (const item of navItems) {
    item.addEventListener('click', (e) => {
      const target = document.getElementById('sec-' + item.dataset.target);
      if (!target) return;
      e.preventDefault();
      target.scrollIntoView({ behavior: 'smooth', block: 'start' });
    });
  }

  refreshAll();
})();

// ─── Model autocomplete datalist (Task 20) ───────────────────────────────────
async function refreshModelDatalist(input) {
  const url = input.dataset.modelsUrl;
  const scope = input.dataset.scope;
  if (!url || !scope) return;
  let body;
  try {
    const res = await fetch(url, { credentials: 'same-origin' });
    if (!res.ok) return;
    body = await res.json();
  } catch {
    return;
  }
  const dl = document.getElementById(`ai-models-${scope}`);
  if (!dl) return;
  while (dl.firstChild) dl.removeChild(dl.firstChild);
  for (const m of body.models ?? []) {
    const opt = document.createElement('option');
    opt.value = m.id;
    opt.label = m.label;
    dl.appendChild(opt);
  }
  if (body.error) {
    input.setAttribute('aria-errormessage', body.error);
  } else {
    input.removeAttribute('aria-errormessage');
  }
}

document.body.addEventListener('focusin', (evt) => {
  const t = evt.target;
  if (t instanceof HTMLInputElement && t.classList.contains('model-input')) {
    if (!t.dataset.modelsLoaded) {
      t.dataset.modelsLoaded = '1';
      refreshModelDatalist(t);
    }
  }
});

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

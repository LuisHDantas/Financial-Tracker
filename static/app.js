"use strict";

// --- Helpers -------------------------------------------------------------------

const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

const currency = new Intl.NumberFormat("pt-BR", { style: "currency", currency: "BRL" });
const compactCurrency = new Intl.NumberFormat("pt-BR", { style: "currency", currency: "BRL", notation: "compact" });
const money = (cents) => currency.format(cents / 100);
const shortMoney = (cents) => compactCurrency.format(cents / 100);

const monthLabel = (month, style = "long") => {
  const [year, m] = month.split("-").map(Number);
  const options = style === "long" ? { month: "long", year: "numeric" } : { month: "short" };
  const label = new Date(year, m - 1, 1).toLocaleDateString("pt-BR", options).replace(".", "");
  return label.charAt(0).toUpperCase() + label.slice(1); // "Outubro de 2026"
};
const dateLabel = (iso) => {
  const [year, m, d] = iso.split("-").map(Number);
  return new Date(year, m - 1, d).toLocaleDateString("pt-BR", { day: "2-digit", month: "short" }).replace(".", "");
};

const escapeHtml = (value) =>
  String(value ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

const option = (value, label, selected) =>
  `<option value="${escapeHtml(value)}"${value === selected ? " selected" : ""}>${escapeHtml(label)}</option>`;

async function api(path, options) {
  const response = await fetch(path, options);
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `${response.status} ${response.statusText}`);
  return body;
}

function toast(message) {
  const el = $("#toast");
  el.textContent = message;
  el.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (el.hidden = true), 2500);
}

// --- Categories and their colors ---------------------------------------------------

const PALETTE = [
  "#16a34a", "#9333ea", "#65a30d", "#ea580c", "#e11d48", "#0284c7", "#dc2626", "#7c3aed",
  "#db2777", "#0d9488", "#ca8a04", "#2563eb", "#c026d3", "#78716c", "#059669", "#64748b", "#94a3b8",
];
let categories = [];
const colorOf = (category) => PALETTE[Math.max(0, categories.indexOf(category)) % PALETTE.length];

const SOURCE_LABELS = { manual: "you", rule: "rule", ai: "AI", none: "?" };
const ACCOUNT_LABELS = { checking: "Checking", credit_card: "Credit card" };

function chip(t) {
  const category = t.category || "Other";
  const source = SOURCE_LABELS[t.category_source] || "";
  return `<button class="chip" style="--c:${colorOf(category)}" data-id="${escapeHtml(t.id)}"
    data-category="${escapeHtml(category)}" title="Click to change">${escapeHtml(category)}
    ${source ? `<small>${source}</small>` : ""}</button>`;
}

async function setCategory(id, category) {
  const t = await api(`/api/transactions/${encodeURIComponent(id)}/category`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ category }),
  });
  toast(`Saved: ${t.memo} → ${t.category}`);
  return t;
}

/// Clicking a chip turns it into a dropdown; picking a category saves it.
function wireChips(root) {
  $$(".chip[data-id]", root).forEach((chipEl) => {
    chipEl.onclick = () => {
      const select = document.createElement("select");
      select.className = "chip-select";
      select.innerHTML = categories.map((c) => option(c, c, chipEl.dataset.category)).join("");
      chipEl.replaceWith(select);
      select.focus();
      select.onblur = () => select.isConnected && select.replaceWith(chipEl);
      select.onchange = async () => {
        select.disabled = true;
        try {
          await setCategory(chipEl.dataset.id, select.value);
          route();
        } catch (e) {
          toast(e.message);
          select.replaceWith(chipEl);
        }
      };
    };
  });
}

// --- Views -----------------------------------------------------------------------

const state = { month: null };

function emptyState() {
  return `<div class="card empty">
    <p><b>No transactions yet.</b></p>
    <p>Save your Itaú statements (.pdf or .ofx) into <code>data/inbox</code>, then go to <a href="#/import">Import</a>.</p>
  </div>`;
}

async function overview(app) {
  const months = await api("/api/months");
  if (!months.length) {
    app.innerHTML = emptyState();
    return;
  }
  if (!months.includes(state.month)) state.month = months[0];
  const [s, trend] = await Promise.all([api(`/api/summary?month=${state.month}`), api("/api/trend")]);

  app.innerHTML = `
    <div class="toolbar">
      <h1>${monthLabel(s.month)}</h1>
      <select id="month">${months.map((m) => option(m, monthLabel(m), s.month)).join("")}</select>
    </div>
    <div class="kpis">
      ${kpi("Money in", money(s.money_in), "pos")}
      ${kpi("Money out", money(s.money_out), "neg")}
      ${kpi("Net", money(s.net), s.net >= 0 ? "pos" : "neg")}
      ${kpi("Transfers", money(s.transfers_out), "", `${money(s.transfers_in)} in · not counted above`)}
    </div>
    <div class="grid2">
      <section class="card"><h2>Spending by category</h2>${bars(s.categories, s.money_out)}</section>
      <section class="card trend"><h2>Last ${trend.length} month${trend.length === 1 ? "" : "s"}</h2>${trendChart(trend)}</section>
    </div>`;

  $("#month", app).onchange = (e) => {
    state.month = e.target.value;
    route();
  };
  $$(".bar-row", app).forEach((row) => {
    row.onclick = () => {
      location.hash = `#/transactions?${new URLSearchParams({ month: state.month, category: row.dataset.category })}`;
    };
  });
}

function kpi(label, value, tone, note = "") {
  return `<div class="card kpi"><div class="label">${label}</div>
    <div class="value ${tone}">${value}</div>${note ? `<div class="note">${note}</div>` : ""}</div>`;
}

function bars(spending, total) {
  if (!spending.length) return `<p class="muted">No spending this month.</p>`;
  const max = Math.max(...spending.map((s) => s.total));
  return `<div class="bars">${spending
    .map((s) => {
      const share = total ? Math.round((s.total / total) * 100) : 0;
      return `<div class="bar-row" data-category="${escapeHtml(s.category)}" title="${s.count} transaction${s.count === 1 ? "" : "s"}">
        <span class="bar-label">${escapeHtml(s.category)}</span>
        <span class="bar-track"><span class="bar-fill" style="width:${(s.total / max) * 100}%;background:${colorOf(s.category)}"></span></span>
        <span class="bar-amount">${money(s.total)}<small>${share}%</small></span>
      </div>`;
    })
    .join("")}</div>`;
}

/// Money in and out per month as two lines, drawn as inline SVG.
function trendChart(points) {
  const W = 520, H = 220, left = 56, right = 12, top = 12, bottom = 28;
  const max = Math.max(1, ...points.flatMap((p) => [p.money_in, p.money_out]));
  const x = (i) => left + (points.length === 1 ? (W - left - right) / 2 : (i * (W - left - right)) / (points.length - 1));
  const y = (v) => top + (1 - v / max) * (H - top - bottom);
  const line = (key) => points.map((p, i) => `${x(i).toFixed(1)},${y(p[key]).toFixed(1)}`).join(" ");
  const dots = (key, color) =>
    points.map((p, i) => `<circle cx="${x(i)}" cy="${y(p[key])}" r="3.5" fill="${color}"><title>${monthLabel(p.month)}: ${money(p[key])}</title></circle>`).join("");
  const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => f * max);
  const IN = "var(--pos)", OUT = "var(--neg)";
  return `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="Money in and out per month">
      <g class="grid">${ticks.map((v) => `<line x1="${left}" x2="${W - right}" y1="${y(v)}" y2="${y(v)}"/>`).join("")}</g>
      ${ticks.map((v) => `<text x="${left - 8}" y="${y(v) + 4}" text-anchor="end">${shortMoney(v)}</text>`).join("")}
      ${points.map((p, i) => `<text x="${x(i)}" y="${H - 8}" text-anchor="middle">${monthLabel(p.month, "short")}</text>`).join("")}
      <polyline points="${line("money_in")}" fill="none" stroke="${IN}" stroke-width="2.5" stroke-linejoin="round"/>
      <polyline points="${line("money_out")}" fill="none" stroke="${OUT}" stroke-width="2.5" stroke-linejoin="round"/>
      ${dots("money_in", IN)}${dots("money_out", OUT)}
    </svg>
    <div class="legend"><span><i style="background:${IN}"></i>Money in</span><span><i style="background:${OUT}"></i>Money out</span></div>`;
}

async function transactions(app) {
  const params = new URLSearchParams(location.hash.split("?")[1] || "");
  const months = await api("/api/months");
  const filters = {
    month: params.get("month") ?? state.month ?? months[0] ?? "",
    category: params.get("category") || "",
    account: params.get("account") || "",
    q: params.get("q") || "",
  };
  const rows = await api(`/api/transactions?${new URLSearchParams(filters)}`);
  const total = rows.reduce((sum, t) => sum + t.amount, 0);

  app.innerHTML = `
    <div class="toolbar"><h1>Transactions</h1><span class="muted">${rows.length} shown · ${money(total)}</span></div>
    <div class="card filters">
      <select id="f-month"><option value="">All months</option>${months.map((m) => option(m, monthLabel(m), filters.month)).join("")}</select>
      <select id="f-category"><option value="">All categories</option>${categories.map((c) => option(c, c, filters.category)).join("")}</select>
      <select id="f-account"><option value="">All accounts</option>${Object.entries(ACCOUNT_LABELS).map(([v, l]) => option(v, l, filters.account)).join("")}</select>
      <input id="f-q" type="search" placeholder="Search descriptions… (Enter)" value="${escapeHtml(filters.q)}">
    </div>
    <section class="card">${rows.length ? transactionTable(rows) : `<p class="muted">No transactions match.</p>`}</section>`;

  const apply = () => {
    const next = {
      month: $("#f-month").value,
      category: $("#f-category").value,
      account: $("#f-account").value,
      q: $("#f-q").value.trim(),
    };
    location.hash = `#/transactions?${new URLSearchParams(next)}`;
  };
  ["#f-month", "#f-category", "#f-account"].forEach((id) => ($(id).onchange = apply));
  $("#f-q").onkeydown = (e) => e.key === "Enter" && apply();
  wireChips(app);
}

function transactionTable(rows) {
  return `<table>
    <thead><tr><th>Date</th><th>Description</th><th class="account">Account</th><th>Category</th><th class="amount">Amount</th></tr></thead>
    <tbody>${rows
      .map(
        (t) => `<tr>
          <td class="date">${dateLabel(t.date)}</td>
          <td class="memo">${escapeHtml(t.memo)}</td>
          <td class="account">${ACCOUNT_LABELS[t.account_type] || ""}</td>
          <td>${chip(t)}</td>
          <td class="amount ${t.amount < 0 ? "" : "pos"}">${money(t.amount)}</td>
        </tr>`,
      )
      .join("")}</tbody></table>`;
}

async function review(app) {
  const rows = await api("/api/review");
  app.innerHTML = `
    <div class="toolbar"><h1>Review</h1>
      <p class="muted">${rows.length
        ? `${rows.length} transaction${rows.length === 1 ? "" : "s"} the AI categorized, or nothing could. Confirm or fix them: your choice always wins over rules and the AI, and is reused for similar descriptions.`
        : ""}</p>
    </div>
    ${rows.length
      ? `<section class="card">${rows.map(reviewRow).join("")}</section>`
      : `<div class="card empty"><p><b>All caught up.</b></p><p>Nothing waiting for review.</p></div>`}`;

  $$(".confirm", app).forEach((button) => {
    button.onclick = async () => {
      button.disabled = true;
      try {
        await setCategory(button.dataset.id, button.dataset.category);
        route();
      } catch (e) {
        toast(e.message);
        button.disabled = false;
      }
    };
  });
  $$(".fix", app).forEach((select) => {
    select.onchange = async () => {
      if (!select.value) return;
      select.disabled = true;
      try {
        await setCategory(select.dataset.id, select.value);
        route();
      } catch (e) {
        toast(e.message);
        select.disabled = false;
      }
    };
  });
}

function reviewRow(t) {
  const guess = t.category || "Other";
  const who = t.category_source === "ai" ? `AI guess${t.model ? ` (${escapeHtml(t.model)})` : ""}` : "No rule matched";
  return `<div class="review-row">
    <div>
      <div class="memo">${escapeHtml(t.memo)}</div>
      <div class="meta">${dateLabel(t.date)} · ${ACCOUNT_LABELS[t.account_type] || ""} · <span class="${t.amount < 0 ? "" : "pos"}">${money(t.amount)}</span> · ${who}</div>
    </div>
    <div class="actions">
      <span class="chip" style="--c:${colorOf(guess)};cursor:default">${escapeHtml(guess)}</span>
      <button class="btn confirm" data-id="${escapeHtml(t.id)}" data-category="${escapeHtml(guess)}">✓ Correct</button>
      <select class="fix" data-id="${escapeHtml(t.id)}"><option value="">Change to…</option>${categories.map((c) => option(c, c, "")).join("")}</select>
    </div>
  </div>`;
}

async function importView(app) {
  app.innerHTML = `
    <div class="toolbar"><h1>Import</h1></div>
    <section class="card">
      <p>Save Itaú statements (<b>.pdf</b>, or <b>.ofx</b> “OFX / Money 2000”) into the <code>data/inbox</code> folder, then press Import.
        A file that was already imported is skipped, and so is any transaction already here.</p>
      <p class="muted small">From Windows Explorer the folder is
        <code>\\\\wsl$\\Ubuntu\\home\\…\\projects\\Financial-Tracker\\data\\inbox</code></p>
      <button id="do-import" class="btn primary">Import inbox</button>
      <div id="import-result"></div>
    </section>`;

  $("#do-import").onclick = async (e) => {
    const button = e.target;
    button.disabled = true;
    button.textContent = "Importing… (the AI takes a second or two per new description)";
    try {
      const result = await api("/api/import", { method: "POST" });
      $("#import-result").innerHTML = importResult(result);
      refreshReviewCount();
    } catch (err) {
      $("#import-result").innerHTML = `<p class="neg">${escapeHtml(err.message)}</p>`;
    } finally {
      button.disabled = false;
      button.textContent = "Import inbox";
    }
  };
}

function importResult({ files, categorized: c }) {
  if (!files.length) return `<p class="muted">No .pdf or .ofx files in data/inbox.</p>`;
  const lines = files.map((f) =>
    f.already_imported
      ? `<li><b>${escapeHtml(f.file)}</b> <span class="muted">already imported, skipped</span></li>`
      : `<li><b>${escapeHtml(f.file)}</b> <span class="muted">(${ACCOUNT_LABELS[f.account_type] || ""})</span>:
          ${f.added} new, ${f.in_file - f.added} already here</li>`,
  );
  const summary = c.total
    ? `Categorized ${c.total}: ${c.by_rule} by rules, ${c.by_ai} by the AI, ${c.left} left as Other.
       ${c.by_ai + c.left ? `<a href="#/review">Review them →</a>` : ""}`
    : "Nothing new to categorize.";
  return `<ul class="result-list">${lines.join("")}</ul><p>${summary}</p>`;
}

// --- Routing ------------------------------------------------------------------------

const views = { overview, transactions, review, import: importView };

async function route() {
  const requested = location.hash.replace(/^#\//, "").split("?")[0];
  const name = views[requested] ? requested : "overview";
  $$("nav a").forEach((a) => a.classList.toggle("active", a.dataset.view === name));
  const app = $("#app");
  try {
    await views[name](app);
  } catch (e) {
    app.innerHTML = `<div class="card error">Something went wrong: ${escapeHtml(e.message)}</div>`;
  }
  refreshReviewCount();
}

async function refreshReviewCount() {
  try {
    const rows = await api("/api/review");
    const badge = $("#review-count");
    badge.textContent = rows.length;
    badge.hidden = rows.length === 0;
  } catch {
    // The badge is a nicety; ignore errors here.
  }
}

window.addEventListener("hashchange", route);
api("/api/categories")
  .then((r) => (categories = r.categories))
  .catch(() => {})
  .finally(route);

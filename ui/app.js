"use strict";

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];

// ---------------------------------------------------------------- helpers

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function fmtSize(n) {
  if (n == null) return "";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10240 ? 1 : 0)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

function fmtDate(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000);
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  const pad = (x) => String(x).padStart(2, "0");
  if (sameDay) return `今日 ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const y = new Date(now); y.setDate(now.getDate() - 1);
  if (d.toDateString() === y.toDateString()) return `昨日 ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  if (d.getFullYear() === now.getFullYear()) return `${d.getMonth() + 1}/${d.getDate()}`;
  return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()}`;
}

function fmtDateTime(ts) {
  const d = new Date(ts * 1000);
  const pad = (x) => String(x).padStart(2, "0");
  return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

const KINDS = {
  pdf: ["pdf"], doc: ["doc", "docx", "odt", "rtf"], xls: ["xls", "xlsx", "xlsm", "csv", "ods"],
  ppt: ["ppt", "pptx", "odp"], img: ["jpg", "jpeg", "png", "gif", "bmp", "webp", "heic", "svg", "tif", "tiff", "ico"],
  vid: ["mp4", "mov", "avi", "mkv", "wmv", "webm", "m4v"], aud: ["mp3", "wav", "m4a", "flac", "aac", "ogg", "wma"],
  zip: ["zip", "7z", "rar", "lzh", "tar", "gz"], exe: ["exe", "msi", "bat", "cmd", "lnk"],
  code: ["js", "ts", "py", "rs", "java", "c", "cpp", "cs", "go", "html", "css", "json", "xml", "yml", "yaml", "sql", "ps1", "sh"],
  txt: ["txt", "md", "log"],
};
const EXT_KIND = Object.fromEntries(Object.entries(KINDS).flatMap(([k, v]) => v.map((e) => [e, k])));

function extOf(name) {
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(i + 1).toLowerCase() : "";
}

function badge(hit) {
  if (hit.is_dir) return `<div class="badge folder" aria-label="フォルダ"><svg viewBox="0 0 24 24" width="18" height="18" fill="#fff"><path d="M3 6a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg></div>`;
  const ext = extOf(hit.name);
  const kind = EXT_KIND[ext] || "";
  return `<div class="badge ${kind}">${esc((ext || "FILE").slice(0, 4).toUpperCase())}</div>`;
}

/** Same folding as the backend: NFKC, lowercase, katakana → hiragana. */
function fold(s) {
  return s.normalize("NFKC").toLowerCase().replace(/[ァ-ヶ]/g, (c) => String.fromCharCode(c.charCodeAt(0) - 0x60));
}

/** Highlights query words inside text, matching the way search does (かな/カナ, 全角/半角, case). */
function highlight(text, words) {
  const ws = words.map(fold).filter((w) => w.length > 0);
  if (!ws.length || !text) return esc(text);
  // Fold char by char, remembering which original char each folded char came from.
  const chars = [...text];
  let folded = "";
  const origin = [];
  chars.forEach((c, i) => {
    const f = fold(c);
    folded += f;
    for (let k = 0; k < f.length; k++) origin.push(i);
  });
  const marked = new Array(chars.length).fill(false);
  for (const w of ws) {
    let from = 0, at;
    while ((at = folded.indexOf(w, from)) !== -1) {
      for (let k = at; k < at + w.length; k++) marked[origin[k]] = true;
      from = at + w.length;
    }
  }
  let out = "", open = false;
  chars.forEach((c, i) => {
    if (marked[i] && !open) { out += "<mark>"; open = true; }
    if (!marked[i] && open) { out += "</mark>"; open = false; }
    out += esc(c);
  });
  return open ? out + "</mark>" : out;
}

function snippetHtml(s) {
  return esc(s).replace(/\u0001/g, "<mark>").replace(/\u0002/g, "</mark>");
}

let toastTimer;
function toast(msg, isError = false) {
  const t = $("#toast");
  t.textContent = msg;
  t.className = "toast" + (isError ? " error" : "");
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), isError ? 6000 : 2600);
}

function confirmBox(text, yes = "OK") {
  return new Promise((resolve) => {
    $("#modal-text").textContent = text;
    $("#modal-yes").textContent = yes;
    $("#modal").hidden = false;
    $("#modal-yes").focus();
    const done = (v) => {
      $("#modal").hidden = true;
      $("#modal-yes").onclick = $("#modal-no").onclick = null;
      resolve(v);
    };
    $("#modal-yes").onclick = () => done(true);
    $("#modal-no").onclick = () => done(false);
  });
}

async function pickPath(directory = true) {
  const opts = { directory, multiple: false };
  try {
    if (T.dialog?.open) return await T.dialog.open(opts);
    return await invoke("plugin:dialog|open", { options: opts });
  } catch (e) {
    toast(String(e), true);
    return null;
  }
}

function openUrl(url) {
  invoke("plugin:opener|open_url", { url }).catch((e) => toast(String(e), true));
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast("パスをコピーしました");
  } catch {
    toast("コピーできませんでした", true);
  }
}

/** Matches a letter/digit key by physical code (IME-safe) or by key value. */
function isKey(e, k) {
  return e.code === (/\d/.test(k) ? "Digit" : "Key") + k.toUpperCase() || (e.key || "").toLowerCase() === k;
}

// ---------------------------------------------------------------- tabs

let currentView = "search";
function showView(name) {
  currentView = name;
  $$(".tab").forEach((t) => t.classList.toggle("active", t.dataset.view === name));
  $$(".view").forEach((v) => v.classList.toggle("active", v.id === `view-${name}`));
  if (name === "pins") loadPins();
  if (name === "history") loadHistory();
  if (name === "settings") loadSettings();
  if (name === "organize") renderQuickFolders();
  if (name === "search") $("#q").focus();
}
$$(".tab").forEach((t) => t.addEventListener("click", () => showView(t.dataset.view)));

// ---------------------------------------------------------------- search

const q = $("#q");
const resultsEl = $("#results");
let items = []; // flat list of selectable rows: {type, path, hit?}
let sel = 0;
let searchSeq = 0;
let nameTimer, contentTimer;
let lastWords = [];

const state = { pins: [], names: null, content: null, recent: null };

function render() {
  const parts = [];
  items = [];
  const push = (html, item) => {
    items.push(item);
    parts.push(html.replace("{SEL}", `data-i="${items.length - 1}"`));
  };
  const row = (hit, extra = "") =>
    `<div class="item" {SEL}>${badge(hit)}<div class="main"><div class="name">${highlight(hit.name, lastWords)}</div>` +
    `<div class="path">${esc(hit.dir || hit.path)}</div>${extra}</div>` +
    `<div class="side">${fmtDate(hit.mtime)}<br>${hit.is_dir ? "フォルダ" : fmtSize(hit.size)}${hit.cloud ? ' <span class="cloud" title="OneDrive オンラインのみ">☁</span>' : ""}</div></div>`;

  if (state.pins.length) {
    parts.push(`<div class="section-head"><span>ピン留め</span></div>`);
    for (const p of state.pins) {
      push(
        `<div class="item" {SEL}><div class="badge pin">PIN</div><div class="main"><div class="name">${esc(p.alias)}</div>` +
          `<div class="path">${esc(p.path)}${p.exists ? "" : "（見つかりません）"}</div></div><div class="side"></div></div>`,
        { type: "pin", path: p.path }
      );
    }
  }

  if (state.names) {
    const n = state.names;
    // A file matching by name and by content is listed once, with its content snippet.
    const snippets = new Map((state.content || []).map((c) => [c.path.toLowerCase(), c.snippet]));
    const named = new Set(n.items.map((h) => h.path.toLowerCase()));
    const contentOnly = (state.content || []).filter((c) => c.hit && !named.has(c.path.toLowerCase()));
    if (n.items.length) {
      parts.push(`<div class="section-head"><span>ファイル名</span><span class="meta">${n.total.toLocaleString()}件${n.total > n.items.length ? `（上位${n.items.length}件）` : ""} · ${n.took_ms}ms</span></div>`);
      for (const h of n.items) {
        const s = snippets.get(h.path.toLowerCase());
        push(row(h, s ? `<div class="snippet">${snippetHtml(s)}</div>` : ""), { type: "file", path: h.path, hit: h });
      }
    }
    if (contentOnly.length) {
      parts.push(`<div class="section-head"><span>中身に一致</span><span class="meta">${contentOnly.length}件</span></div>`);
      for (const c of contentOnly) {
        push(row(c.hit, `<div class="snippet">${snippetHtml(c.snippet)}</div>`), { type: "file", path: c.path, hit: c.hit });
      }
    }
    if (!n.items.length && !contentOnly.length && !state.pins.length) {
      parts.push(`<div class="empty-note">見つかりませんでした。<br>言葉を短くする、ひらがなで打つ、別の言葉で探す、などを試してください。</div>`);
    }
  } else if (state.recent) {
    if (state.recent.length) {
      parts.push(`<div class="section-head"><span>最近更新したファイル</span></div>`);
      for (const h of state.recent) push(row(h), { type: "file", path: h.path, hit: h });
    } else {
      parts.push(`<div class="empty-note">インデックスを作成しています。少し待つと検索できるようになります。</div>`);
    }
  }

  resultsEl.innerHTML = parts.join("");
  sel = Math.min(sel, Math.max(items.length - 1, 0));
  updateSel(false);
}

function updateSel(scroll = true) {
  $$(".item.sel", resultsEl).forEach((e) => e.classList.remove("sel"));
  const el = resultsEl.querySelector(`[data-i="${sel}"]`);
  if (el) {
    el.classList.add("sel");
    if (scroll) el.scrollIntoView({ block: "nearest" });
  }
  schedulePreview();
}

async function runSearch() {
  const text = q.value;
  const seq = ++searchSeq;
  clearTimeout(contentTimer);
  if (!text.trim()) {
    state.names = null;
    state.content = null;
    state.pins = [];
    state.recent = await invoke("recent", { limit: 40 }).catch(() => []);
    if (seq === searchSeq) { sel = 0; render(); }
    return;
  }
  try {
    const r = await invoke("search", { query: text, limit: 150 });
    if (seq !== searchSeq) return;
    lastWords = text.split(/[\s　]+/).filter((w) => w && !w.includes(":") && !/^size[<>]/.test(w));
    state.pins = r.pins;
    state.names = r;
    state.content = null;
    sel = 0;
    render();
    if (r.has_terms) {
      contentTimer = setTimeout(async () => {
        const c = await invoke("search_content", { query: text, limit: 40 }).catch(() => []);
        if (seq !== searchSeq) return;
        state.content = c;
        // Keep the selection on the same row while content results arrive below.
        render();
      }, 220);
    }
  } catch (e) {
    toast(String(e), true);
  }
}

q.addEventListener("input", () => {
  clearTimeout(nameTimer);
  nameTimer = setTimeout(runSearch, 60);
});

function currentItem() {
  return items[sel];
}

async function openItem(it, reveal = false) {
  if (!it) return;
  try {
    await invoke(reveal ? "reveal_path" : "open_path", { path: it.path });
    invoke("hide_window");
  } catch (e) {
    toast(String(e), true);
  }
}

q.addEventListener("keydown", (e) => {
  if (e.isComposing || e.keyCode === 229) return; // IME conversion in progress
  if (e.key === "ArrowDown") { e.preventDefault(); if (sel < items.length - 1) { sel++; updateSel(); } }
  else if (e.key === "ArrowUp") { e.preventDefault(); if (sel > 0) { sel--; updateSel(); } }
  else if (e.key === "PageDown") { e.preventDefault(); sel = Math.min(items.length - 1, sel + 10); updateSel(); }
  else if (e.key === "PageUp") { e.preventDefault(); sel = Math.max(0, sel - 10); updateSel(); }
  else if (e.key === "Enter") {
    e.preventDefault();
    openItem(currentItem(), e.ctrlKey);
  }
  else if (e.key === "Escape") {
    e.preventDefault();
    if (q.value) { q.value = ""; runSearch(); }
    else invoke("hide_window");
  }
  else if (isKey(e, "c") && e.ctrlKey && e.shiftKey) {
    e.preventDefault();
    const it = currentItem();
    if (it) copyText(it.path);
  }
});

resultsEl.addEventListener("click", (e) => {
  const el = e.target.closest("[data-i]");
  if (!el) return;
  sel = Number(el.dataset.i);
  updateSel(false);
});
resultsEl.addEventListener("dblclick", (e) => {
  const el = e.target.closest("[data-i]");
  if (el) openItem(items[Number(el.dataset.i)]);
});

// context menu
const menu = $("#menu");
resultsEl.addEventListener("contextmenu", (e) => {
  const el = e.target.closest("[data-i]");
  if (!el) return;
  e.preventDefault();
  sel = Number(el.dataset.i);
  updateSel(false);
  const it = items[sel];
  menu.innerHTML = `
    <button data-act="open">開く</button>
    <button data-act="reveal">保存場所を開く</button>
    <button data-act="copy">パスをコピー</button>
    <button data-act="copyname">名前をコピー</button>
    ${it.type !== "pin" ? `<button data-act="pin">ピン留めに追加…</button>` : ""}
    ${it.hit && !it.hit.is_dir ? `<button data-act="organize">このフォルダを整理…</button>` : it.hit?.is_dir ? `<button data-act="organize-dir">このフォルダを整理…</button>` : ""}`;
  menu.hidden = false;
  const { innerWidth: w, innerHeight: h } = window;
  menu.style.left = Math.min(e.clientX, w - 220) + "px";
  menu.style.top = Math.min(e.clientY, h - menu.offsetHeight - 8) + "px";
  menu.onclick = (ev) => {
    const act = ev.target.dataset.act;
    menu.hidden = true;
    if (act === "open") openItem(it);
    if (act === "reveal") openItem(it, true);
    if (act === "copy") copyText(it.path);
    if (act === "copyname") copyText(it.path.split("\\").pop());
    if (act === "pin") { showView("pins"); $("#pin-path").value = it.path; $("#pin-alias").focus(); }
    if (act === "organize") { $("#org-root").value = it.hit.dir; showView("organize"); inspectFolder(); }
    if (act === "organize-dir") { $("#org-root").value = it.path; showView("organize"); inspectFolder(); }
  };
});
document.addEventListener("click", (e) => { if (!menu.contains(e.target)) menu.hidden = true; });

// preview
let previewTimer, previewSeq = 0;
function schedulePreview() {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(loadPreview, 120);
}

async function loadPreview() {
  const pv = $("#preview");
  const it = currentItem();
  if (!it) { pv.innerHTML = `<div class="empty-note">ファイルを選ぶと中身をここに表示します</div>`; return; }
  const seq = ++previewSeq;
  let p;
  try { p = await invoke("preview", { path: it.path, words: lastWords }); } catch (e) { p = { hit: it.hit, text: null, children: [] }; }
  if (seq !== previewSeq) return;
  const h = p.hit || it.hit || { name: it.path.split("\\").pop(), path: it.path };
  const name = h.name || it.path.split("\\").pop();
  let body = "";
  if (p.children && p.children.length) {
    body = `<div class="pv-children">${p.children.map((c) => `<div>${esc(c)}</div>`).join("")}</div>`;
  } else if (p.text) {
    body = `<div class="pv-text">${highlight(p.text.slice(0, 4000), lastWords)}</div>`;
  } else if (h.cloud) {
    body = `<div class="hint">OneDrive のオンラインのみのファイルです。開くとダウンロードされます。</div>`;
  }
  pv.innerHTML = `
    <div class="pv-name">${esc(name)}</div>
    <div class="pv-path">${esc(it.path)}</div>
    <div class="pv-meta">
      ${h.mtime ? `<span>更新</span><span>${fmtDateTime(h.mtime)}</span>` : ""}
      ${h.is_dir === false ? `<span>サイズ</span><span>${fmtSize(h.size)}</span>` : ""}
    </div>
    <div class="pv-actions">
      <button class="btn small" data-a="open">開く</button>
      <button class="btn small" data-a="reveal">保存場所を開く</button>
      <button class="btn small" data-a="copy">パスをコピー</button>
    </div>
    ${body}`;
  pv.querySelector('[data-a="open"]').onclick = () => openItem(it);
  pv.querySelector('[data-a="reveal"]').onclick = () => openItem(it, true);
  pv.querySelector('[data-a="copy"]').onclick = () => copyText(it.path);
}

// ---------------------------------------------------------------- organize

let currentPlan = null;

async function renderQuickFolders() {
  const box = $("#org-quick");
  if (box.dataset.done) return;
  box.dataset.done = "1";
  try {
    const s = await invoke("get_settings");
    const home = s.settings.roots[0];
    if (!home) return;
    const names = [["ダウンロード", "Downloads"], ["デスクトップ", "Desktop"], ["ドキュメント", "Documents"], ["ピクチャ", "Pictures"]];
    box.innerHTML = names.map(([ja, en]) => `<button class="btn" data-p="${esc(home)}\\${en}">${ja}</button>`).join("");
    box.onclick = (e) => {
      const p = e.target.dataset.p;
      if (p) { $("#org-root").value = p; inspectFolder(); }
    };
  } catch { /* quick buttons are optional */ }
}

async function inspectFolder() {
  const root = $("#org-root").value.trim();
  const out = $("#org-summary");
  hidePlan();
  if (!root) { out.innerHTML = ""; return; }
  out.innerHTML = `<span class="spinner"></span>調べています…`;
  try {
    const s = await invoke("organize_inspect", { root, recursive: $("#org-recursive").checked });
    out.innerHTML = `${s.files.toLocaleString()}件のファイル${s.capped ? "（上限に達したため一部のみ）" : ""}：` +
      s.by_category.map(([k, v]) => `<span class="tag">${esc(k)} ${v}</span>`).join("");
  } catch (e) {
    out.innerHTML = `<span style="color:var(--danger)">${esc(e)}</span>`;
  }
}

$("#org-pick").addEventListener("click", async () => {
  const p = await pickPath(true);
  if (p) { $("#org-root").value = p; inspectFolder(); }
});
$("#org-root").addEventListener("change", inspectFolder);
$("#org-recursive").addEventListener("change", inspectFolder);

function setOrgBusy(busy, label) {
  $$(".preset, #org-apply").forEach((b) => (b.disabled = busy));
  if (busy) {
    $("#org-plan-card").hidden = false;
    $("#org-plan").innerHTML = `<div class="empty-note"><span class="spinner"></span>${esc(label)}</div>`;
  }
}

$$(".preset").forEach((b) =>
  b.addEventListener("click", async () => {
    const root = $("#org-root").value.trim();
    if (!root) { toast("先に整理するフォルダを選んでください", true); return; }
    setOrgBusy(true, "整理案を作っています…");
    try {
      showPlan(await invoke("organize_preset", { root, preset: b.dataset.preset, recursive: $("#org-recursive").checked }));
    } catch (e) {
      hidePlan();
      toast(String(e), true);
    } finally {
      setOrgBusy(false);
    }
  })
);

function hidePlan() {
  currentPlan = null;
  $("#org-plan-card").hidden = true;
}

function showPlan(plan) {
  currentPlan = plan;
  const card = $("#org-plan-card");
  card.hidden = false;
  const groups = new Map();
  plan.moves.forEach((m, i) => {
    const parts = m.to.split("\\");
    parts.pop();
    const folder = parts.join("\\") || "（このフォルダ直下）";
    if (!groups.has(folder)) groups.set(folder, []);
    groups.get(folder).push([m, i]);
  });
  const sorted = [...groups.entries()].sort((a, b) => b[1].length - a[1].length);
  const tree = sorted
    .map(([folder, ms]) =>
      `<details class="plan-folder" ${plan.moves.length <= 60 || sorted.length <= 4 ? "open" : ""}><summary><input type="checkbox" class="grp" checked> 📁 ${esc(folder)} <span class="count">${ms.length}件</span></summary>` +
      ms.map(([m, i]) => {
        const toName = m.to.split("\\").pop();
        const fromName = m.from.split("\\").pop();
        return `<label class="plan-move"><input type="checkbox" class="mv" data-i="${i}" checked><span class="from" title="${esc(m.from)}">${esc(m.from)}</span>` +
          (toName !== fromName ? `<span class="arrow">→</span><span class="to">${esc(toName)}</span>` : "") + `</label>`;
      }).join("") + `</details>`
    )
    .join("");
  $("#org-plan").innerHTML = `
    <div class="plan-summary">${esc(plan.summary)}</div>
    <div class="plan-stats">${plan.moves.length}件を移動 / 対象 ${plan.total_files}件 · 削除は一切しません · 同名ファイルは上書きせず名前を変えます</div>
    ${plan.notes.length ? `<div class="plan-notes">${plan.notes.map(esc).join("<br>")}</div>` : ""}
    ${plan.moves.length ? `<div class="plan-tree">${tree}</div>` : `<div class="empty-note">移動が必要なファイルはありません。</div>`}`;
  $("#org-apply").disabled = !plan.moves.length;
  $$(".grp", card).forEach((g) =>
    g.addEventListener("change", () => $$(".mv", g.closest("details")).forEach((m) => (m.checked = g.checked)))
  );
  card.scrollIntoView({ behavior: "smooth", block: "start" });
}

$("#org-cancel").addEventListener("click", hidePlan);

$("#org-apply").addEventListener("click", async () => {
  if (!currentPlan) return;
  const chosen = $$(".mv:checked").map((c) => currentPlan.moves[Number(c.dataset.i)]);
  if (!chosen.length) { toast("移動するファイルが選ばれていません", true); return; }
  const ok = await confirmBox(`${chosen.length}件のファイルを移動します。\nあとで「履歴」から元に戻せます。`, "整理する");
  if (!ok) return;
  setOrgBusy(true, "移動しています…");
  try {
    const r = await invoke("organize_apply", { plan: { ...currentPlan, moves: chosen } });
    hidePlan();
    inspectFolder();
    if (r.errors.length) {
      toast(`${r.moved}件を移動しました。${r.errors.length}件は移動できませんでした: ${r.errors[0]}`, true);
    } else {
      toast(`${r.moved}件を整理しました。「履歴」から元に戻せます`);
    }
  } catch (e) {
    toast(String(e), true);
    setOrgBusy(false);
  } finally {
    setOrgBusy(false);
  }
});

// ---------------------------------------------------------------- duplicates

$("#dup-run").addEventListener("click", async () => {
  const out = $("#dup-result");
  const btn = $("#dup-run");
  btn.disabled = true;
  out.innerHTML = `<div class="empty-note"><span class="spinner"></span>同じ中身のファイルを比べています…</div>`;
  try {
    const groups = await invoke("find_duplicates", { root: $("#org-root").value.trim() || null, minMb: Number($("#dup-min").value) || 0 });
    if (!groups.length) {
      out.innerHTML = `<div class="empty-note">重複ファイルは見つかりませんでした。</div>`;
      return;
    }
    const wasted = groups.reduce((a, g) => a + g.wasted, 0);
    out.innerHTML = `<div class="plan-stats">${groups.length}組の重複 · 片付けると最大 ${fmtSize(wasted)} 空きます</div>` +
      `<div class="plan-tree">` +
      groups.map((g, gi) =>
        `<details class="plan-folder" ${gi < 5 ? "open" : ""} data-g="${gi}"><summary>${esc(g.files[0].name)} <span class="count">${g.files.length}個 · 各${fmtSize(g.size)}</span></summary>` +
        g.files.map((f) =>
          `<div class="plan-move dup-row"><span class="from grow" title="${esc(f.path)}">${esc(f.path)}</span>` +
          `<button class="btn small" data-reveal="${esc(f.path)}">場所</button>` +
          `<button class="btn small danger" data-trash="${esc(f.path)}">ごみ箱へ</button></div>`
        ).join("") + `</details>`
      ).join("") + `</div>`;
  } catch (e) {
    out.innerHTML = `<div class="empty-note">${esc(e)}</div>`;
  } finally {
    btn.disabled = false;
  }
});

$("#dup-result").addEventListener("click", async (e) => {
  const r = e.target.dataset.reveal, t = e.target.dataset.trash;
  if (r) invoke("reveal_path", { path: r }).catch((er) => toast(String(er), true));
  if (!t) return;
  const group = e.target.closest("details");
  if ($$("[data-trash]", group).length <= 1) { toast("最後の1つはごみ箱に移動できません", true); return; }
  if (!(await confirmBox(`ごみ箱に移動します:\n${t}\n\nごみ箱から元に戻せます。`, "ごみ箱へ"))) return;
  try {
    await invoke("trash_path", { path: t });
    e.target.closest(".dup-row").remove();
    toast("ごみ箱に移動しました");
  } catch (er) {
    toast(String(er), true);
  }
});

// ---------------------------------------------------------------- pins

async function loadPins() {
  const list = $("#pin-list");
  try {
    const pins = await invoke("list_pins");
    list.innerHTML = pins.length
      ? pins.map((p) => `
        <div class="list-item"><div class="head">
          <div class="badge pin">PIN</div>
          <div class="grow"><div class="title">${esc(p.alias)}</div><div class="sub">${esc(p.path)}</div></div>
          <button class="btn small" data-open="${esc(p.path)}">開く</button>
          <button class="btn small danger" data-del="${esc(p.alias)}">外す</button>
        </div></div>`).join("")
      : `<div class="empty-note">まだありません。検索結果を右クリック →「ピン留めに追加」でも登録できます。</div>`;
  } catch (e) {
    toast(String(e), true);
  }
}
$("#pin-list").addEventListener("click", async (e) => {
  const o = e.target.dataset.open, d = e.target.dataset.del;
  if (o) openItem({ path: o });
  if (d) {
    await invoke("remove_pin", { alias: d }).catch((er) => toast(String(er), true));
    loadPins();
  }
});
$("#pin-pick").addEventListener("click", async () => {
  const p = await pickPath(true);
  if (p) $("#pin-path").value = p;
});
$("#pin-add").addEventListener("click", async () => {
  try {
    await invoke("add_pin", { alias: $("#pin-alias").value, path: $("#pin-path").value.trim() });
    $("#pin-alias").value = $("#pin-path").value = "";
    toast("ピン留めしました");
    loadPins();
  } catch (e) {
    toast(String(e), true);
  }
});

// ---------------------------------------------------------------- history

async function loadHistory() {
  const list = $("#history-list");
  try {
    const hs = await invoke("history");
    list.innerHTML = hs.length
      ? hs.map((h) => `
        <div class="list-item ${h.undone ? "undone" : ""}"><div class="head">
          <div class="grow">
            <div class="title">${esc(h.instruction)}</div>
            <div class="sub">${fmtDateTime(h.created)} · ${esc(h.root)} · ${h.moves.length}件${h.undone ? " · 元に戻し済み" : ""}</div>
          </div>
          <button class="btn small" data-undo="${h.id}" ${h.undone ? "disabled" : ""}>元に戻す</button>
        </div>
        <details><summary class="hint">移動したファイルを見る</summary>
          ${h.moves.slice(0, 300).map((m) => `<div>${esc(m.from)} → ${esc(m.to)}</div>`).join("")}
          ${h.moves.length > 300 ? `<div>…ほか${h.moves.length - 300}件</div>` : ""}
        </details></div>`).join("")
      : `<div class="empty-note">まだ整理の履歴はありません。</div>`;
  } catch (e) {
    toast(String(e), true);
  }
}
$("#history-list").addEventListener("click", async (e) => {
  const id = e.target.dataset.undo;
  if (!id) return;
  if (!(await confirmBox("この整理を元に戻します。移動したファイルを元の場所へ戻します。", "元に戻す"))) return;
  e.target.disabled = true;
  try {
    const r = await invoke("undo", { id: Number(id) });
    toast(r.errors.length ? `${r.restored}件を戻しました。${r.errors.length}件は戻せませんでした` : `${r.restored}件を元に戻しました`, r.errors.length > 0);
  } catch (er) {
    toast(String(er), true);
  }
  loadHistory();
});

// ---------------------------------------------------------------- settings

let settingsCache = null;

function renderRoots() {
  $("#roots").innerHTML = settingsCache.roots
    .map((r, i) => `<div class="chip-row"><span>${esc(r)}</span><button class="btn small danger" data-rm="${i}">外す</button></div>`)
    .join("") || `<div class="hint">検索する場所がありません。追加してください。</div>`;
}

async function loadSettings() {
  try {
    const v = await invoke("get_settings");
    settingsCache = structuredClone(v.settings);
    const s = v.settings;
    renderRoots();
    $("#excludes").value = s.excludes.join("\n");
    $("#show-hidden").checked = s.show_hidden;
    $("#content-enabled").checked = s.content_enabled;
    $("#content-max").value = s.content_max_mb;
    $("#content-code").checked = s.content_code;
    $("#hotkey").value = s.hotkey;
    $("#autostart").checked = s.autostart;
    $("#check-updates").checked = s.check_updates;
    $("#settings-version").textContent = `Sagasu v${v.version} · データ形式 v${v.data_version}`;
  } catch (e) {
    toast(String(e), true);
  }
}

$("#roots").addEventListener("click", (e) => {
  const i = e.target.dataset.rm;
  if (i == null) return;
  settingsCache.roots.splice(Number(i), 1);
  renderRoots();
});
$("#root-add").addEventListener("click", async () => {
  const p = await pickPath(true);
  if (p && !settingsCache.roots.includes(p)) {
    settingsCache.roots.push(p);
    renderRoots();
  }
});

// hotkey capture
const hk = $("#hotkey");
hk.addEventListener("keydown", (e) => {
  e.preventDefault();
  if (["Control", "Shift", "Alt", "Meta"].includes(e.key)) return;
  const mods = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Super");
  if (!mods.length) { toast("Ctrl・Alt・Shift のいずれかと組み合わせてください", true); return; }
  let key = e.code;
  if (key.startsWith("Key")) key = key.slice(3);
  else if (key.startsWith("Digit")) key = key.slice(5);
  hk.value = [...mods, key].join("+");
});
$("#hotkey-reset").addEventListener("click", () => (hk.value = "Alt+Space"));

$("#settings-save").addEventListener("click", async () => {
  const s = {
    ...settingsCache,
    excludes: $("#excludes").value.split("\n").map((x) => x.trim()).filter(Boolean),
    show_hidden: $("#show-hidden").checked,
    content_enabled: $("#content-enabled").checked,
    content_code: $("#content-code").checked,
    content_max_mb: Math.max(1, Number($("#content-max").value) || 30),
    hotkey: hk.value.trim(),
    autostart: $("#autostart").checked,
    check_updates: $("#check-updates").checked,
  };
  try {
    await invoke("save_settings", { settings: s });
    toast("設定を保存しました");
    loadSettings();
  } catch (e) {
    toast(String(e), true);
  }
});

$("#rescan").addEventListener("click", async () => {
  await invoke("rescan");
  toast("インデックスを作り直しています");
});
document.addEventListener("click", (e) => {
  const a = e.target.closest("a[data-href]");
  if (a) { e.preventDefault(); openUrl(a.dataset.href); }
});

// ---------------------------------------------------------------- updates

let pendingUpdate = null;

function showUpdate(u) {
  pendingUpdate = u;
  const pill = $("#update-pill");
  pill.textContent = `新しいバージョン v${u.version} があります — 更新する`;
  pill.hidden = false;
}

async function checkUpdate(manual) {
  const st = $("#update-state");
  if (manual) st.innerHTML = `<span class="spinner"></span>確認しています…`;
  try {
    const u = await invoke("check_update");
    if (u) {
      showUpdate(u);
      if (manual) st.textContent = `v${u.version} があります（現在 v${u.current}）。画面下のボタンから更新できます。`;
    } else if (manual) {
      st.textContent = "最新版です。";
    }
  } catch (e) {
    if (manual) st.textContent = String(e);
  }
}

$("#update-pill").addEventListener("click", async () => {
  if (!pendingUpdate) return;
  const notes = pendingUpdate.notes ? `

${pendingUpdate.notes.slice(0, 600)}` : "";
  if (!(await confirmBox(`v${pendingUpdate.version} に更新します。ダウンロード後、Sagasu が自動で再起動します。${notes}`, "更新する"))) return;
  const pill = $("#update-pill");
  pill.disabled = true;
  pill.textContent = "ダウンロードしています…";
  try {
    await invoke("install_update");
  } catch (e) {
    toast(String(e), true);
    pill.disabled = false;
    showUpdate(pendingUpdate);
  }
});
$("#update-check").addEventListener("click", () => checkUpdate(true));
$("#open-logs").addEventListener("click", () => invoke("open_log_folder").catch((e) => toast(String(e), true)));

// Check once a little after startup, without slowing the first search.
setTimeout(async () => {
  const v = await invoke("get_settings").catch(() => null);
  if (v?.settings.check_updates) checkUpdate(false);
}, 8000);

// ---------------------------------------------------------------- status

let lastScanSeen = null;
async function refreshStatus() {
  try {
    const s = await invoke("status");
    let t;
    if (s.scanning) t = `ファイルを調べています… ${s.scanned.toLocaleString()}件`;
    else t = `${s.indexed.toLocaleString()}件のファイル・フォルダ`;
    if (s.content_running && s.content_total) t += ` · 中身を解析中 ${s.content_done.toLocaleString()} / ${s.content_total.toLocaleString()}`;
    else if (s.content_indexed) t += ` · 中身 ${s.content_indexed.toLocaleString()}件`;
    $("#status").textContent = t;
    // A finished scan replaces the index: refresh the idle "recent files" list.
    const scanChanged = s.last_scan !== lastScanSeen;
    lastScanSeen = s.last_scan;
    if (!q.value && currentView === "search" && s.indexed && (scanChanged || !state.recent || !state.recent.length)) runSearch();
  } catch { /* backend not ready */ }
}
setInterval(refreshStatus, 1500);

// ---------------------------------------------------------------- global keys & events

// Behave like a desktop app: no browser context menu or page reload.
document.addEventListener("contextmenu", (e) => {
  if (!e.target.closest("input, textarea, .pv-text")) e.preventDefault();
});

document.addEventListener("keydown", (e) => {
  if (e.key === "F5" || (e.ctrlKey && (isKey(e, "r") || isKey(e, "p")))) {
    e.preventDefault();
    return;
  }
  if (e.key === "Escape" && document.activeElement !== q) {
    if (!$("#modal").hidden) { $("#modal-no").click(); return; }
    if (!menu.hidden) { menu.hidden = true; return; }
    if (currentView !== "search") { showView("search"); return; }
  }
  // e.code, not e.key: with the Japanese IME on, e.key can be "Process".
  const digit = /^(?:Digit|Numpad)([1-5])$/.exec(e.code) || /^([1-5])$/.exec(e.key || "");
  if (e.ctrlKey && digit) {
    e.preventDefault();
    showView(["search", "organize", "pins", "history", "settings"][Number(digit[1]) - 1]);
  }
  if (e.ctrlKey && (isKey(e, "f") || isKey(e, "l"))) { e.preventDefault(); showView("search"); q.select(); }
});

window.addEventListener("focus", () => {
  if (currentView === "search" && $("#modal").hidden) q.focus();
});
T.event.listen("sagasu://focus", () => {
  if (currentView === "search") { q.focus(); q.select(); }
});
let hotkeyWarned = false;
async function checkHotkey() {
  const s = await invoke("status").catch(() => null);
  if (s?.hotkey_error && !hotkeyWarned) {
    hotkeyWarned = true;
    toast(`呼び出しキー「${s.hotkey_error}」を登録できませんでした（他のアプリが使用中）。設定で別のキーを選んでください。`, true);
  }
}
setTimeout(checkHotkey, 800);

refreshStatus();
runSearch();
q.focus();

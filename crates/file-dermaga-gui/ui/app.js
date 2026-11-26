"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = () => window.__TAURI__.window.getCurrentWindow();

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "html") e.innerHTML = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (k === "dataset") Object.assign(e.dataset, v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    e.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return e;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function joinPath(dir, name) {
  return dir.endsWith("/") ? dir + name : `${dir}/${name}`;
}

function parentPath(p) {
  const t = p.replace(/\/+$/, "");
  const i = t.lastIndexOf("/");
  return i <= 0 ? "/" : t.slice(0, i);
}

function normPath(p) {
  p = (p || "/").trim();
  if (!p.startsWith("/")) p = "/" + p;
  p = p.replace(/\/{2,}/g, "/");
  return p.length > 1 ? p.replace(/\/+$/, "") : p;
}

function baseName(p) {
  return p.replace(/\/+$/, "").split("/").pop();
}

function fmtSize(n) {
  if (n == null || n < 0) return "";
  if (n < 1024) return n.toLocaleString();
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toLocaleString(undefined, { minimumFractionDigits: 1, maximumFractionDigits: 1 })} ${units[i]}`;
}

/** Listings with only minute/day precision usually carry server wall-clock
 * time, so show them as-is; precise times (MLSD, S3, local) in local time. */
function fmtTime(ms, acc) {
  if (ms == null) return "";
  const d = new Date(ms);
  const wall = acc === "d" || acc === "m" || acc === "h";
  const g = (f) => (wall ? d[`getUTC${f}`]() : d[`get${f}`]());
  const p = (n) => String(n).padStart(2, "0");
  let s = `${p(g("Date"))}/${p(g("Month") + 1)}/${String(g("FullYear")).slice(-2)}`;
  if (acc !== "d") s += ` ${p(g("Hours"))}:${p(g("Minutes"))}`;
  if (acc === "s") s += `:${p(g("Seconds"))}`;
  return s;
}

function fileType(e) {
  if (e.isDir) return e.isLink ? "Symlink" : "Directory";
  const i = e.name.lastIndexOf(".");
  const ext = i > 0 ? e.name.slice(i + 1) : "";
  // "Apache-2.0" or "v1.10" are versions, not extensions.
  if (!ext || /^\d+$/.test(ext)) return "File";
  return `${ext.toLowerCase()}-file`;
}

/** Scroll `container` (only it, never its ancestors) just enough to show `elm`. */
function ensureVisible(container, elm, margin = 2) {
  if (!container || !elm) return;
  const b = container.getBoundingClientRect();
  const r = elm.getBoundingClientRect();
  const headH = container.querySelector("thead")?.getBoundingClientRect().height || 0;
  const top = b.top + headH;
  if (r.top < top) container.scrollTop -= top - r.top + margin;
  else if (r.bottom > b.bottom) container.scrollTop += r.bottom - b.bottom + margin;
}

function errText(e) {
  return typeof e === "string" ? e : e?.message || JSON.stringify(e);
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

const SVG = {
  folder:
    '<svg viewBox="0 0 16 16"><path d="M1 3.6A1.6 1.6 0 0 1 2.6 2h3.5l1.6 1.7h5.7A1.6 1.6 0 0 1 15 5.3v7.1a1.6 1.6 0 0 1-1.6 1.6H2.6A1.6 1.6 0 0 1 1 12.4z" fill="var(--folder)" stroke="var(--folder-dark)" stroke-width=".8"/></svg>',
  file:
    '<svg viewBox="0 0 16 16"><path d="M3.5 1.5h6l3 3v10h-9z" fill="var(--panel)" stroke="var(--muted)"/><path d="M9.5 1.5v3h3" fill="none" stroke="var(--muted)"/></svg>',
  link:
    '<svg viewBox="0 0 16 16" style="position:absolute;left:0;bottom:0;width:9px;height:9px"><rect width="16" height="16" rx="3" fill="var(--panel)" stroke="var(--muted)"/><path d="M5 11l6-6M6 5h5v5" fill="none" stroke="var(--fg)" stroke-width="2"/></svg>',
};

function icon(kind, { link = false, unknown = false } = {}) {
  const i = el("span", { class: "ico", html: SVG[kind] });
  if (link) i.insertAdjacentHTML("beforeend", SVG.link);
  if (unknown) i.append(el("span", { class: "q" }, "?"));
  return i;
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

const S = {
  session: null,
  info: null,
  lastSite: null,
  showHidden: true,
  activeSide: "local",
  drag: null,
  sites: [],
};

async function loadSites() {
  try {
    S.sites = await invoke("sites_load");
  } catch (e) {
    log("error", `Could not read saved sites: ${errText(e)}`);
  }
}

function siteSubtitle(s) {
  if (s.protocol === "s3") return s.endpoint ? s.endpoint.replace(/^https?:\/\//, "") : "Amazon S3";
  return `${s.user ? s.user + "@" : ""}${s.host}${s.port ? ":" + s.port : ""}`;
}

function sitesDropdown(anchor) {
  const r = anchor.getBoundingClientRect();
  const items = S.sites.length
    ? S.sites.map((s) => ({ label: s.name || "(unnamed)", sub: siteSubtitle(s), action: () => connect(s) }))
    : [{ label: "No saved sites yet", disabled: true }];
  items.push("-", { label: "Site Manager…", key: "Ctrl+S", action: siteManager });
  popup(r.left, r.bottom + 2, items);
}

function store(key, value) {
  try {
    if (value === undefined) return JSON.parse(localStorage.getItem(key));
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    return undefined;
  }
}

// ---------------------------------------------------------------------------
// Message log
// ---------------------------------------------------------------------------

const LOG_LABEL = { status: "Status:", command: "Command:", response: "Response:", error: "Error:" };

function log(kind, text) {
  const box = $("#log");
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 30;
  box.append(el("div", { class: `line ${kind}` }, el("span", { class: "k" }, LOG_LABEL[kind] || kind), el("span", {}, text)));
  while (box.childElementCount > 3000) box.firstElementChild.remove();
  if (atBottom) box.scrollTop = box.scrollHeight;
}

// ---------------------------------------------------------------------------
// Popups (menus, context menus) and dialogs
// ---------------------------------------------------------------------------

function closePopups() {
  $("#popup-root").replaceChildren();
  $$(".menu.open").forEach((m) => m.classList.remove("open"));
}

/** items: {label, key?, action?, disabled?, checked?} | "-" */
function popup(x, y, items) {
  closePopups();
  const box = el("div", { class: "popup" });
  for (const it of items) {
    if (it === "-") {
      box.append(el("div", { class: "sep" }));
      continue;
    }
    const label = it.checked === undefined ? it.label : `${it.checked ? "✓" : " "} ${it.label}`;
    box.append(
      el(
        "div",
        {
          class: `item${it.disabled ? " disabled" : ""}`,
          onclick: (e) => {
            e.stopPropagation();
            if (it.disabled) return;
            closePopups();
            it.action?.();
          },
        },
        el("span", {}, label),
        it.sub ? el("span", { class: "sub" }, it.sub) : null,
        it.key ? el("span", { class: "key" }, it.key) : null,
      ),
    );
  }
  $("#popup-root").append(box);
  const r = box.getBoundingClientRect();
  box.style.left = `${Math.min(x, innerWidth - r.width - 4)}px`;
  box.style.top = `${Math.min(y, innerHeight - r.height - 4)}px`;
  return box;
}

document.addEventListener("mousedown", (e) => {
  if (!e.target.closest(".popup") && !e.target.closest(".menu")) closePopups();
});

/** Generic modal; resolves with the clicked button's value (null on Esc). */
function dialog(title, body, buttons) {
  return new Promise((resolve) => {
    const prevFocus = document.activeElement;
    const overlay = el("div", { class: "overlay" });
    const finish = (v) => {
      overlay.remove();
      document.removeEventListener("keydown", onKey, true);
      prevFocus?.focus?.();
      resolve(v);
    };
    const onKey = (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        finish(null);
      } else if (e.key === "Enter" && !e.target.matches("textarea, select")) {
        const primary = buttons.find((b) => b.primary);
        if (primary) {
          e.preventDefault();
          e.stopPropagation();
          finish(typeof primary.value === "function" ? primary.value() : primary.value);
        }
      }
    };
    document.addEventListener("keydown", onKey, true);
    const actions = el(
      "div",
      { class: "actions" },
      buttons.map((b) =>
        el(
          "button",
          {
            class: b.primary ? "primary" : "",
            dataset: { act: b.id || (b.primary ? "ok" : "cancel") },
            onclick: () => finish(typeof b.value === "function" ? b.value() : b.value),
          },
          b.label,
        ),
      ),
    );
    overlay.append(el("div", { class: "dialog", role: "dialog" }, el("h2", {}, title), el("div", { class: "body" }, body), actions));
    $("#popup-root").after(overlay);
    (overlay.querySelector("input, select") || overlay.querySelector("button.primary"))?.focus();
  });
}

const alertDlg = (title, text) => dialog(title, el("p", {}, text), [{ label: "OK", value: true, primary: true }]);

const confirmDlg = (title, text, ok = "OK") =>
  dialog(title, el("p", {}, text), [
    { label: "Cancel", value: false },
    { label: ok, value: true, primary: true },
  ]).then((v) => v === true);

function promptDlg(title, text, value = "", { password = false } = {}) {
  const input = el("input", { class: "full", type: password ? "password" : "text", value, spellcheck: "false" });
  setTimeout(() => {
    input.focus();
    const dot = value.lastIndexOf(".");
    input.setSelectionRange(0, dot > 0 ? dot : value.length);
  });
  return dialog(title, [el("p", {}, text), input], [
    { label: "Cancel", value: null },
    { label: "OK", value: () => input.value, primary: true },
  ]);
}

// ---------------------------------------------------------------------------
// Directory tree
// ---------------------------------------------------------------------------

class Tree {
  constructor(pane, root) {
    this.pane = pane;
    this.root = root;
    this.reset();
    root.addEventListener("click", (e) => this.onClick(e));
    root.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      const row = e.target.closest(".tnode");
      if (row) this.pane.navigate(row.dataset.path);
    });
    root.addEventListener("dragover", (e) => this.onDragOver(e));
    root.addEventListener("dragleave", (e) => e.target.closest(".tnode")?.classList.remove("drop"));
    root.addEventListener("drop", (e) => this.onDrop(e));
  }

  reset() {
    this.nodes = new Map();
    this.root.replaceChildren();
  }

  node(p) {
    if (!this.nodes.has(p)) this.nodes.set(p, { children: null, partial: false, open: false });
    return this.nodes.get(p);
  }

  setChildren(p, names) {
    const n = this.node(p);
    n.children = [...names].sort(collator.compare);
    n.partial = false;
  }

  /** Make sure `p` and its ancestors are present and expanded. */
  reveal(p) {
    let cur = "/";
    this.node("/").open = true;
    for (const part of p.split("/").filter(Boolean)) {
      const n = this.node(cur);
      if (!n.children) {
        n.children = [part];
        n.partial = true;
      } else if (!n.children.includes(part)) {
        n.children.push(part);
        n.children.sort(collator.compare);
      }
      n.open = true;
      cur = joinPath(cur, part);
    }
  }

  async toggle(p) {
    const n = this.node(p);
    if (n.open && n.children && !n.partial) {
      n.open = false;
    } else {
      n.open = true;
      if (!n.children || n.partial) {
        try {
          const entries = await this.pane.fetch(p);
          this.setChildren(p, entries.filter((e) => e.isDir && this.pane.visible(e)).map((e) => e.name));
        } catch (e) {
          log("error", errText(e));
          n.open = false;
        }
      }
    }
    this.render();
  }

  onClick(e) {
    const row = e.target.closest(".tnode");
    if (!row) return;
    if (e.target.closest(".tw")) this.toggle(row.dataset.path);
    else this.pane.navigate(row.dataset.path, { fromTree: true });
  }

  onDragOver(e) {
    const row = e.target.closest(".tnode");
    if (!S.drag || !row) return;
    e.preventDefault();
    $$(".tnode.drop", this.root).forEach((r) => r !== row && r.classList.remove("drop"));
    row.classList.add("drop");
  }

  onDrop(e) {
    const row = e.target.closest(".tnode");
    if (!S.drag || !row) return;
    e.preventDefault();
    row.classList.remove("drop");
    dropOn(this.pane, row.dataset.path);
  }

  render() {
    if (!this.pane.ready()) {
      this.root.replaceChildren();
      return;
    }
    const rows = [];
    const walk = (p, name, depth) => {
      const n = this.node(p);
      const known = n.children && !n.partial;
      const hasKids = !known || n.children.length > 0;
      const twisty = hasKids ? (n.open ? "▾" : "▸") : "";
      rows.push(
        el(
          "div",
          { class: `tnode${p === this.pane.path ? " current" : ""}`, dataset: { path: p }, style: `padding-left:${4 + depth * 16}px` },
          el("span", { class: "tw" }, twisty),
          icon("folder", { unknown: this.pane.remote && !n.children }),
          el("span", { class: "lbl" }, name),
        ),
      );
      if (n.open && n.children) for (const c of n.children) walk(joinPath(p, c), c, depth + 1);
    };
    walk("/", this.pane.remote && S.info?.s3 ? "/ (buckets)" : "/", 0);
    const scroll = this.root.scrollTop;
    this.root.replaceChildren(...rows);
    this.root.scrollTop = scroll;
    if (this.revealCurrent) {
      this.revealCurrent = false;
      ensureVisible(this.root, $(".tnode.current", this.root));
    }
  }
}

// ---------------------------------------------------------------------------
// File list panes
// ---------------------------------------------------------------------------

const COLUMNS = {
  local: [
    ["name", "Filename", "46%"],
    ["size", "Filesize", "13%", "num"],
    ["type", "Filetype", "16%"],
    ["mtime", "Last modified", ""],
  ],
  remote: [
    ["name", "Filename", "36%"],
    ["size", "Filesize", "10%", "num"],
    ["type", "Filetype", "11%"],
    ["mtime", "Last modified", "17%"],
    ["permissions", "Permissions", "12%"],
    ["owner", "Owner/Group", ""],
  ],
};

class Pane {
  constructor(side) {
    this.side = side;
    this.remote = side === "remote";
    this.root = $(`#${side}`);
    this.listEl = $(".list", this.root);
    this.pathInput = $(".path", this.root);
    this.statusEl = $(".statusline", this.root);
    this.tree = new Tree(this, $(".tree", this.root));
    this.path = null;
    this.entries = [];
    this.sel = new Set();
    this.anchor = null;
    this.sort = store(`fdm.sort.${side}`) || { key: "name", asc: true };
    this.busy = false;
    this.bind();
  }

  ready() {
    return !this.remote || S.session != null;
  }

  visible(e) {
    return S.showHidden || !e.name.startsWith(".");
  }

  fetch(path) {
    return this.remote ? invoke("remote_list", { session: S.session, path }) : invoke("local_list", { path });
  }

  join(name) {
    return joinPath(this.path, name);
  }

  async navigate(path, { keepSelection = false, fromTree = false } = {}) {
    if (!this.ready()) return false;
    path = normPath(path);
    let entries;
    this.busy = true;
    this.statusEl.textContent = this.remote ? "Retrieving directory listing..." : "";
    try {
      entries = await this.fetch(path);
    } catch (e) {
      const msg = errText(e);
      if (!this.remote) log("error", msg);
      this.statusEl.textContent = msg;
      this.pathInput.value = this.path ?? "";
      this.busy = false;
      return false;
    }
    this.busy = false;
    const samePath = path === this.path;
    const keep = keepSelection && samePath ? new Set(this.sel) : new Set();
    this.resetScroll = !samePath;
    this.path = path;
    this.entries = entries;
    this.sel = new Set([...keep].filter((n) => entries.some((e) => e.name === n)));
    this.tree.setChildren(path, entries.filter((e) => e.isDir && this.visible(e)).map((e) => e.name));
    this.tree.reveal(path);
    this.pathInput.value = path;
    if (!this.remote) store("fdm.localPath", path);
    this.render();
    // Clicking in the tree keeps it where it is; other navigation (path box,
    // double-click, "..") scrolls the tree just enough to show the folder.
    this.tree.revealCurrent = !fromTree && !samePath;
    this.tree.render();
    return true;
  }

  refresh() {
    if (this.path) return this.navigate(this.path, { keepSelection: true });
  }

  clear(message) {
    this.path = null;
    this.entries = [];
    this.sel.clear();
    this.tree.reset();
    this.pathInput.value = "";
    this.listEl.replaceChildren(el("div", { class: "placeholder" }, message));
    this.statusEl.textContent = "";
  }

  rows() {
    const { key, asc } = this.sort;
    const dir = asc ? 1 : -1;
    const val = (e) => (key === "type" ? fileType(e) : e[key]);
    return this.entries
      .filter((e) => this.visible(e))
      .sort((a, b) => {
        if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
        let c;
        const va = val(a);
        const vb = val(b);
        if (key === "size" || key === "mtime") c = (va ?? -1) - (vb ?? -1);
        else c = collator.compare(String(va ?? ""), String(vb ?? ""));
        return (c || collator.compare(a.name, b.name)) * dir;
      });
  }

  selected() {
    return this.entries.filter((e) => this.sel.has(e.name));
  }

  render() {
    const cols = COLUMNS[this.side];
    const rows = this.rows();
    this.view = rows;
    const head = el(
      "tr",
      {},
      cols.map(([key, label, width, cls]) =>
        el(
          "th",
          {
            class: `sortable ${cls || ""}`,
            style: width ? `width:${width}` : null,
            onclick: () => {
              this.sort = this.sort.key === key ? { key, asc: !this.sort.asc } : { key, asc: true };
              store(`fdm.sort.${this.side}`, this.sort);
              this.render();
            },
          },
          label,
          this.sort.key === key ? el("span", { class: "arrow" }, this.sort.asc ? "▲" : "▼") : null,
        ),
      ),
    );
    const body = el("tbody");
    if (this.path !== "/") {
      body.append(el("tr", { dataset: { up: "1" } }, el("td", { colspan: cols.length }, icon("folder"), "..")));
    }
    for (const e of rows) {
      const cells = cols.map(([key, , , cls]) => {
        let v;
        switch (key) {
          case "name":
            return el(
              "td",
              { title: e.target ? `${e.name} -> ${e.target}` : e.name },
              icon(e.isDir ? "folder" : "file", { link: e.isLink }),
              e.name,
            );
          case "size":
            v = e.isDir ? "" : fmtSize(e.size);
            break;
          case "type":
            v = fileType(e);
            break;
          case "mtime":
            v = fmtTime(e.mtime, e.mtimeAcc);
            break;
          default:
            v = e[key] || "";
        }
        return el("td", { class: cls || null }, v);
      });
      body.append(
        el("tr", { class: this.sel.has(e.name) ? "sel" : null, draggable: "true", dataset: { name: e.name } }, cells),
      );
    }
    const table = el("table", { class: "grid" }, el("thead", {}, head), body);
    const scroll = this.resetScroll ? 0 : this.listEl.scrollTop;
    this.resetScroll = false;
    this.listEl.replaceChildren(table);
    this.listEl.scrollTop = scroll;
    this.updateStatus();
  }

  /** Scroll the file list (only) so that `name` is visible. */
  reveal(name) {
    ensureVisible(this.listEl, $(`tr[data-name="${CSS.escape(name)}"]`, this.listEl));
  }

  paintSelection() {
    $$("tbody tr[data-name]", this.listEl).forEach((tr) => tr.classList.toggle("sel", this.sel.has(tr.dataset.name)));
    this.updateStatus();
  }

  updateStatus() {
    const pick = this.sel.size ? this.selected() : this.view || [];
    const files = pick.filter((e) => !e.isDir);
    const dirs = pick.length - files.length;
    const total = files.reduce((a, e) => a + Math.max(0, e.size), 0);
    const parts = [];
    if (files.length) parts.push(`${files.length} file${files.length === 1 ? "" : "s"}`);
    if (dirs) parts.push(`${dirs} director${dirs === 1 ? "y" : "ies"}`);
    if (this.sel.size) {
      this.statusEl.textContent = `Selected ${parts.join(" and ")}.${files.length ? ` Total size: ${fmtSize(total)}` : ""}`;
    } else if (!pick.length) {
      this.statusEl.textContent = "Empty directory.";
    } else {
      this.statusEl.textContent = `${parts.join(" and ")}. Total size: ${fmtSize(total) || "0"}`;
    }
  }

  select(name, { toggle = false, range = false } = {}) {
    if (range && this.anchor) {
      const names = this.view.map((e) => e.name);
      const a = names.indexOf(this.anchor);
      const b = names.indexOf(name);
      if (a >= 0 && b >= 0) {
        if (!toggle) this.sel.clear();
        for (let i = Math.min(a, b); i <= Math.max(a, b); i++) this.sel.add(names[i]);
      }
    } else if (toggle) {
      this.sel.has(name) ? this.sel.delete(name) : this.sel.add(name);
      this.anchor = name;
    } else {
      this.sel = new Set([name]);
      this.anchor = name;
    }
    this.paintSelection();
  }

  open(entry) {
    if (!entry) return;
    if (entry.isDir) this.navigate(this.join(entry.name));
    else transferSelected(this, null, [entry]);
  }

  bind() {
    const L = this.listEl;
    this.pathInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        this.navigate(this.pathInput.value);
      }
    });
    L.addEventListener("focusin", () => (S.activeSide = this.side));
    L.addEventListener("mousedown", (e) => {
      const tr = e.target.closest("tbody tr");
      if (e.button === 2 && tr?.dataset.name && this.sel.has(tr.dataset.name)) return;
      if (!tr) {
        if (!e.target.closest("thead")) {
          this.sel.clear();
          this.paintSelection();
        }
        return;
      }
      if (tr.dataset.up) {
        this.sel.clear();
        this.paintSelection();
        return;
      }
      this.select(tr.dataset.name, { toggle: e.ctrlKey || e.metaKey, range: e.shiftKey });
    });
    L.addEventListener("dblclick", (e) => {
      const tr = e.target.closest("tbody tr");
      if (!tr) return;
      if (tr.dataset.up) this.navigate(parentPath(this.path));
      else this.open(this.entries.find((x) => x.name === tr.dataset.name));
    });
    L.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      L.focus();
      this.contextMenu(e.clientX, e.clientY);
    });
    L.addEventListener("keydown", (e) => this.onKey(e));
    L.addEventListener("dragstart", (e) => {
      const tr = e.target.closest("tr[data-name]");
      if (!tr) return;
      if (!this.sel.has(tr.dataset.name)) this.select(tr.dataset.name);
      S.drag = { pane: this, entries: this.selected() };
      e.dataTransfer.effectAllowed = "copyMove";
      e.dataTransfer.setData("text/plain", S.drag.entries.map((x) => x.name).join("\n"));
    });
    L.addEventListener("dragend", () => {
      S.drag = null;
      $$(".drop").forEach((x) => x.classList.remove("drop"));
    });
    L.addEventListener("dragover", (e) => {
      if (!S.drag || !this.ready()) return;
      const tr = e.target.closest("tr[data-name]");
      const target = tr && this.entries.find((x) => x.name === tr.dataset.name);
      const sameSide = S.drag.pane === this;
      // Within one pane a drop only makes sense onto another folder.
      if (sameSide && (!target?.isDir || S.drag.entries.some((x) => x.name === target.name))) return;
      e.preventDefault();
      $$("tr.drop", L).forEach((r) => r !== tr && r.classList.remove("drop"));
      if (target?.isDir) tr.classList.add("drop");
    });
    L.addEventListener("drop", (e) => {
      if (!S.drag) return;
      e.preventDefault();
      const tr = e.target.closest("tr[data-name]");
      const target = tr && this.entries.find((x) => x.name === tr.dataset.name);
      $$("tr.drop", L).forEach((r) => r.classList.remove("drop"));
      dropOn(this, target?.isDir ? this.join(target.name) : this.path);
    });
  }

  onKey(e) {
    const names = (this.view || []).map((x) => x.name);
    const cur = this.anchor && names.includes(this.anchor) ? names.indexOf(this.anchor) : -1;
    const move = (i) => {
      i = Math.max(0, Math.min(names.length - 1, i));
      if (names[i] === undefined) return;
      if (e.shiftKey) {
        const keepAnchor = this.anchor;
        this.select(names[i], { range: true });
        this.anchor = keepAnchor;
        this.focusName = names[i];
      } else this.select(names[i]);
      this.reveal(names[i]);
    };
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        move((this.focusName && e.shiftKey ? names.indexOf(this.focusName) : cur) + 1);
        break;
      case "ArrowUp":
        e.preventDefault();
        move((this.focusName && e.shiftKey ? names.indexOf(this.focusName) : cur) - 1);
        break;
      case "Home":
        e.preventDefault();
        move(0);
        break;
      case "End":
        e.preventDefault();
        move(names.length - 1);
        break;
      case "Enter":
        e.preventDefault();
        if (this.sel.size === 1) this.open(this.selected()[0]);
        else if (this.sel.size > 1) transferSelected(this);
        break;
      case "Backspace":
        e.preventDefault();
        if (this.path && this.path !== "/") this.navigate(parentPath(this.path));
        break;
      case "Delete":
        e.preventDefault();
        deleteSelected(this);
        break;
      case "F2":
        e.preventDefault();
        renameSelected(this);
        break;
      case "a":
        if (e.ctrlKey) {
          e.preventDefault();
          this.sel = new Set(names);
          this.paintSelection();
        }
        break;
    }
    if (!e.shiftKey) this.focusName = null;
  }

  contextMenu(x, y) {
    const n = this.sel.size;
    const other = this.remote ? panes.local : panes.remote;
    const s3 = this.remote && S.info?.s3;
    const one = n === 1 ? this.selected()[0] : null;
    const items = [];
    if (!this.ready()) {
      items.push({ label: "Not connected", disabled: true });
    } else {
      items.push({
        label: this.remote ? "Download" : "Upload",
        disabled: !n || !other.ready() || !S.session,
        action: () => transferSelected(this),
      });
      if (one?.isDir) items.push({ label: "Open", action: () => this.open(one) });
      items.push("-");
      items.push({ label: s3 && this.path === "/" ? "Create bucket" : "Create directory", action: () => mkdirIn(this) });
      items.push({ label: "Refresh", key: "F5", action: () => this.refresh() });
      items.push("-");
      items.push({ label: "Delete", key: "Del", disabled: !n, action: () => deleteSelected(this) });
      items.push({ label: "Rename", key: "F2", disabled: n !== 1 || (s3 && one?.isDir), action: () => renameSelected(this) });
      if (s3) {
        items.push("-");
        items.push({
          label: "Generate pre-signed URL…",
          disabled: !(one && !one.isDir),
          action: () => presignSelected(this),
        });
      }
      items.push("-");
      items.push({ label: "Copy path to clipboard", disabled: !n, action: () => copyPaths(this) });
    }
    popup(x, y, items);
  }
}

const panes = {};

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

async function mkdirIn(pane) {
  const s3Bucket = pane.remote && S.info?.s3 && pane.path === "/";
  const name = await promptDlg(
    s3Bucket ? "Create bucket" : "Create directory",
    s3Bucket ? "Please enter the name of the bucket to create:" : `Please enter the name of the directory to create in "${pane.path}":`,
    s3Bucket ? "" : "New directory",
  );
  if (!name) return;
  if (name.includes("/")) return alertDlg("Invalid name", "The name must not contain '/'.");
  const path = pane.join(name);
  try {
    if (pane.remote) await invoke("remote_mkdir", { session: S.session, path });
    else await invoke("local_mkdir", { path });
    await pane.refresh();
    pane.select(name);
    pane.reveal(name);
  } catch (e) {
    alertDlg("Could not create directory", errText(e));
  }
}

async function deleteSelected(pane) {
  const items = pane.selected();
  if (!items.length) return;
  const dirs = items.filter((e) => e.isDir).length;
  const what = items.length === 1 ? `"${items[0].name}"` : `${items.length} items`;
  const extra = dirs ? "\n\nDirectories are deleted including all their contents." : "";
  const where = pane.remote ? "from the server" : "from your computer";
  if (!(await confirmDlg("Confirmation needed", `Really delete ${what} ${where}?${extra}`, "Delete"))) return;
  const list = items.map((e) => ({ path: pane.join(e.name), isDir: e.isDir && !e.isLink }));
  try {
    if (pane.remote) await invoke("remote_delete", { session: S.session, items: list });
    else await invoke("local_delete", { items: list });
  } catch (e) {
    alertDlg("Delete failed", errText(e));
  }
  pane.sel.clear();
  pane.refresh();
}

async function renameSelected(pane) {
  const [e] = pane.selected();
  if (!e || pane.sel.size !== 1) return;
  const name = await promptDlg("Rename", `Enter the new name for "${e.name}":`, e.name);
  if (!name || name === e.name) return;
  const from = pane.join(e.name);
  const to = name.startsWith("/") ? name : pane.join(name);
  try {
    if (pane.remote) await invoke("remote_rename", { session: S.session, from, to });
    else await invoke("local_rename", { from, to });
    await pane.refresh();
    if (!name.includes("/")) {
      pane.select(name);
      pane.reveal(name);
    }
  } catch (err) {
    alertDlg("Rename failed", errText(err));
  }
}

async function presignSelected(pane) {
  const [e] = pane.selected();
  const hours = await promptDlg("Pre-signed URL", `How many hours should the link for "${e.name}" stay valid? (max 168)`, "24");
  if (!hours) return;
  const secs = Math.round(Math.min(168, Math.max(0.02, parseFloat(hours) || 24)) * 3600);
  try {
    const url = await invoke("remote_presign", { session: S.session, path: pane.join(e.name), expires: secs });
    const input = el("input", { class: "full", value: url, readonly: true });
    setTimeout(() => input.select());
    await dialog("Pre-signed URL", [el("p", {}, "Anyone with this link can download the file until it expires:"), input], [
      { label: "Copy", value: () => (navigator.clipboard?.writeText(url), true) },
      { label: "Close", value: true, primary: true },
    ]);
  } catch (err) {
    alertDlg("Could not create URL", errText(err));
  }
}

function copyPaths(pane) {
  const text = pane.selected().map((e) => pane.join(e.name)).join("\n");
  navigator.clipboard?.writeText(text).catch(() => {});
}

/** Transfer the selection (or `entries`) of `from` into `targetDir` of the other pane. */
async function transferSelected(from, targetDir = null, entries = null) {
  if (!S.session) return alertDlg("Not connected", "Connect to a server first.");
  const to = from.remote ? panes.local : panes.remote;
  const items = (entries || from.selected()).filter(Boolean);
  if (!items.length) return;
  const dest = targetDir || to.path;
  if (!dest) return;
  if (S.info?.s3 && !from.remote && normPath(dest) === "/") {
    return alertDlg("Select a bucket", "Open a bucket on the remote side before uploading.");
  }
  for (const e of items) {
    const req = {
      session: S.session,
      direction: from.remote ? "download" : "upload",
      local: from.remote ? joinPath(dest, e.name) : from.join(e.name),
      remote: from.remote ? from.join(e.name) : joinPath(dest, e.name),
      isDir: e.isDir,
      size: e.isDir || e.size < 0 ? null : e.size,
    };
    queue.start(req);
  }
}

async function dropOn(targetPane, dir) {
  const drag = S.drag;
  S.drag = null;
  if (!drag || !drag.entries.length) return;
  if (drag.pane === targetPane) {
    // Move within the same side.
    for (const e of drag.entries) {
      const from = targetPane.join(e.name);
      const to = joinPath(dir, e.name);
      if (from === to || dir.startsWith(from + "/")) continue;
      try {
        if (targetPane.remote) await invoke("remote_rename", { session: S.session, from, to });
        else await invoke("local_rename", { from, to });
      } catch (err) {
        alertDlg("Move failed", errText(err));
        break;
      }
    }
    targetPane.refresh();
    return;
  }
  transferSelected(drag.pane, dir, drag.entries);
}

// ---------------------------------------------------------------------------
// Transfer queue
// ---------------------------------------------------------------------------

const queue = {
  items: new Map(),
  tab: "queued",
  sel: null,
  failSeq: 0,
  renderPending: false,

  async start(req) {
    let id;
    try {
      id = await invoke("transfer_start", { req });
    } catch (e) {
      log("error", errText(e));
      return;
    }
    const it = this.items.get(id) || { id, state: "queued", done: 0 };
    Object.assign(it, { req, direction: req.direction, local: req.local, remote: req.remote, total: req.size, server: S.info?.label });
    this.items.set(id, it);
    this.render();
  },

  onEvent(ev) {
    let it = this.items.get(ev.id);
    if (!it) {
      it = { id: ev.id, state: "queued", done: 0, local: ev.local, remote: ev.remote, total: ev.total };
      this.items.set(ev.id, it);
    }
    switch (ev.state) {
      case "queued":
        break;
      case "active":
        it.state = "active";
        it.cur = ev.local !== it.local || ev.remote !== it.remote ? { local: ev.local, remote: ev.remote } : null;
        it.fileTotal = ev.total ?? it.total;
        it.done = 0;
        it.fileStarted = performance.now();
        break;
      case "progress":
        it.state = "active";
        it.done = ev.done;
        if (ev.total != null) it.fileTotal = ev.total;
        break;
      case "file-failed": {
        const fid = `f${++this.failSeq}`;
        this.items.set(fid, { id: fid, state: "failed", local: ev.local, remote: "", direction: it.direction, error: ev.error, child: true });
        break;
      }
      case "done":
        it.state = "done";
        it.done = ev.done;
        it.cur = null;
        this.afterFinish(it);
        break;
      case "failed":
        it.state = "failed";
        it.error = ev.error;
        it.cur = null;
        this.afterFinish(it);
        break;
      case "cancelled":
        it.state = "failed";
        it.error = "Cancelled by user";
        it.cur = null;
        break;
    }
    this.scheduleRender();
  },

  afterFinish(it) {
    const pane = it.direction === "upload" ? panes.remote : panes.local;
    const target = it.direction === "upload" ? it.remote : it.local;
    if (pane.path && (parentPath(target) === pane.path || target === pane.path)) pane.refresh();
  },

  scheduleRender() {
    if (this.renderPending) return;
    this.renderPending = true;
    requestAnimationFrame(() => {
      this.renderPending = false;
      this.render();
    });
  },

  inTab(it) {
    if (this.tab === "queued") return it.state === "queued" || it.state === "active";
    if (this.tab === "failed") return it.state === "failed";
    return it.state === "done";
  },

  statusCell(it) {
    if (it.state === "queued") return "Queued";
    if (it.state === "failed") return el("span", { class: "status-failed", title: it.error }, it.error || "Failed");
    if (it.state === "done") return el("span", { class: "status-ok" }, "Transferred");
    const total = it.fileTotal;
    const secs = Math.max(0.001, (performance.now() - (it.fileStarted || performance.now())) / 1000);
    const rate = it.done / secs;
    const name = it.cur ? baseName(it.cur.local || it.cur.remote) + " — " : "";
    if (total) {
      const pct = Math.min(100, (it.done / total) * 100);
      return el(
        "div",
        { class: "bar" },
        el("i", { style: `width:${pct}%` }),
        el("span", {}, `${name}${pct.toFixed(1)}%  ${fmtSize(it.done)} (${fmtSize(rate)}/s)`),
      );
    }
    return el("div", { class: "bar" }, el("span", {}, `${name}${fmtSize(it.done) || "0"} (${fmtSize(rate)}/s)`));
  },

  render() {
    const body = $("#queue tbody");
    const rows = [...this.items.values()].filter((it) => this.inTab(it));
    body.replaceChildren(
      ...rows.map((it) => {
        const up = it.direction === "upload";
        const localCell = it.child ? it.local : it.cur?.local || it.local;
        const remoteCell = it.child ? it.remote : it.cur?.remote || it.remote;
        return el(
          "tr",
          { class: this.sel === it.id ? "sel" : null, dataset: { id: String(it.id) } },
          el("td", { title: localCell }, localCell),
          el("td", {}, up ? "-->" : "<--"),
          el("td", { title: remoteCell }, remoteCell),
          // Folders: total bytes once finished; per-file sizes would mislead.
          el("td", { class: "num" }, it.req?.isDir ? (it.state === "done" ? fmtSize(it.done) : "") : fmtSize(it.total ?? it.fileTotal)),
          el("td", {}, "Normal"),
          el("td", {}, this.statusCell(it)),
        );
      }),
    );
    const counts = { queued: 0, failed: 0, success: 0 };
    for (const it of this.items.values()) {
      if (it.state === "queued" || it.state === "active") counts.queued++;
      else if (it.state === "failed") counts.failed++;
      else counts.success++;
    }
    $$("#queue-tabs .tab").forEach((t) => {
      const label = { queued: "Queued files", failed: "Failed transfers", success: "Successful transfers" }[t.dataset.tab];
      const c = counts[t.dataset.tab];
      t.replaceChildren(label, c ? el("span", { class: "count" }, ` (${c})`) : "");
      t.classList.toggle("active", t.dataset.tab === this.tab);
    });
    $("#queue-state").textContent = counts.queued ? `Queue: ${counts.queued} item${counts.queued === 1 ? "" : "s"}` : "Queue: empty";
  },

  cancel(id) {
    return invoke("transfer_cancel", { id });
  },

  cancelAll() {
    for (const it of this.items.values()) if (it.state === "queued" || it.state === "active") this.cancel(it.id);
  },

  clearFinished(which) {
    for (const [id, it] of this.items) if ((which ? it.state === which : it.state !== "queued" && it.state !== "active")) this.items.delete(id);
    this.render();
  },

  retry(id) {
    const it = this.items.get(id);
    if (!it?.req) return;
    this.items.delete(id);
    if (!S.session) return alertDlg("Not connected", "Connect to the server again to retry.");
    this.start({ ...it.req, session: S.session });
  },

  bind() {
    $("#queue-tabs").addEventListener("click", (e) => {
      const t = e.target.closest(".tab");
      if (!t) return;
      this.tab = t.dataset.tab;
      this.render();
    });
    const box = $("#queue");
    box.addEventListener("mousedown", (e) => {
      const tr = e.target.closest("tbody tr");
      this.sel = tr ? (isNaN(+tr.dataset.id) ? tr.dataset.id : +tr.dataset.id) : null;
      this.render();
    });
    box.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      const it = this.items.get(this.sel);
      popup(e.clientX, e.clientY, [
        { label: "Cancel transfer", disabled: !it || !(it.state === "queued" || it.state === "active"), action: () => this.cancel(it.id) },
        { label: "Retry", disabled: !it || it.state !== "failed" || !it.req, action: () => this.retry(it.id) },
        "-",
        { label: "Cancel all transfers", action: () => this.cancelAll() },
        { label: "Clear failed transfers", action: () => this.clearFinished("failed") },
        { label: "Clear successful transfers", action: () => this.clearFinished("done") },
      ]);
    });
  },
};

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

function setConnState(text, secure = false) {
  const s = $("#conn-state");
  s.textContent = text;
  s.classList.toggle("secure", secure);
}

function setTitle(text) {
  document.title = text;
  appWindow()
    .setTitle(text)
    .catch(() => {});
}

function needsPassword(site) {
  return site.protocol !== "s3" && site.user && site.user !== "anonymous" && !site.password && !site.passwordSaved;
}

/** After a successful connect, keep a password that was typed into the
 * prompt if the site is set to save it. */
async function rememberSecret(site, kind, value) {
  if (!site.id || !site.savePassword || !value) return;
  try {
    await invoke("site_secret_set", { id: site.id, kind, value });
    const s = S.sites.find((x) => x.id === site.id);
    if (s) s.passwordSaved = true;
  } catch (e) {
    log("error", errText(e));
  }
}

const CERT_ERROR = /certificate|UnknownIssuer|CaUsedAsEndEntity|NotValidForName|BadSignature|Expired/i;

async function connect(site) {
  site = { ...site };
  if (S.session != null) await disconnect();
  let typed = null;
  if (needsPassword(site)) {
    const pw = await promptDlg("Enter password", `Please enter the password for ${site.user}@${site.host}:`, "", { password: true });
    if (pw === null) return false;
    site.password = pw;
    typed = ["password", pw];
  }
  if (site.protocol === "s3" && site.accessKey && !site.secretKey && !site.passwordSaved) {
    const sk = await promptDlg("Enter secret key", `Secret access key for ${site.accessKey}:`, "", { password: true });
    if (sk === null) return false;
    site.secretKey = sk;
    typed = ["secret", sk];
  }
  setConnState("Connecting…");
  let info;
  try {
    info = await invoke("connect", { site });
  } catch (e) {
    const msg = errText(e);
    if (CERT_ERROR.test(msg) && !site.insecure) {
      const ok = await confirmDlg(
        "Unknown certificate",
        `The server's certificate could not be verified:\n\n${msg}\n\nOnly continue if you trust this server, for example your own server with a self-signed certificate. Connect anyway?`,
        "Connect anyway",
      );
      if (ok) return connect({ ...site, insecure: true });
    } else {
      alertDlg("Connection failed", msg);
    }
    setConnState("Not connected");
    return false;
  }
  S.session = info.session;
  S.info = info;
  S.lastSite = site;
  if (typed) rememberSecret(site, typed[0], typed[1]);
  setConnState(info.secure ? `Connected to ${info.label}` : `Connected to ${info.label} (not encrypted)`, info.secure);
  setTitle(`${site.name ? site.name + " - " : ""}${info.label} - FileDermaga`);
  panes.remote.tree.reset();
  await panes.remote.navigate(info.home || "/");
  if (site.localDir) await panes.local.navigate(site.localDir);
  return true;
}

async function disconnect() {
  if (S.session == null) return;
  const id = S.session;
  S.session = null;
  S.info = null;
  try {
    await invoke("disconnect", { session: id });
  } catch (e) {
    log("error", errText(e));
  }
  panes.remote.clear("Not connected to any server");
  setConnState("Not connected");
  setTitle("FileDermaga");
}

function reconnect() {
  if (S.lastSite) connect(S.lastSite);
}

// ---------------------------------------------------------------------------
// Quickconnect
// ---------------------------------------------------------------------------

function quickSite() {
  const f = $("#quickconnect");
  let host = f.host.value.trim();
  const user = f.user.value.trim();
  const password = f.password.value;
  const port = parseInt(f.port.value, 10) || null;
  if (/^s3:\/\//i.test(host)) {
    const ep = host.slice(5).replace(/\/+$/, "");
    return {
      protocol: "s3",
      host: "",
      endpoint: ep ? (/^https?:\/\//.test(ep) ? ep : `https://${ep}`) : "",
      accessKey: user,
      secretKey: password,
      pathStyle: !!ep && !/amazonaws\.com/.test(ep),
    };
  }
  let protocol = "ftp";
  const m = host.match(/^([a-z0-9+-]+):\/\/(.*)$/i);
  if (m) {
    protocol = m[1].toLowerCase();
    host = m[2].replace(/\/+$/, "");
  }
  if (port === 990 && protocol === "ftp") protocol = "ftps";
  return { protocol, host, port, user, password };
}

function bindQuickconnect() {
  const f = $("#quickconnect");
  f.addEventListener("submit", async (e) => {
    e.preventDefault();
    const site = quickSite();
    if (!site.host && site.protocol !== "s3") return alertDlg("Quickconnect", "Please enter a host name.");
    const recent = (store("fdm.recent") || []).filter((r) => !(r.host === f.host.value && r.user === f.user.value && r.port === f.port.value));
    recent.unshift({ host: f.host.value.trim(), user: f.user.value.trim(), port: f.port.value.trim() });
    store("fdm.recent", recent.slice(0, 10));
    await connect(site);
  });
  $("#qc-history").addEventListener("click", (e) => {
    const r = e.target.getBoundingClientRect();
    const recent = store("fdm.recent") || [];
    const items = recent.length
      ? recent.map((x) => ({
          label: `${x.user ? x.user + "@" : ""}${x.host}${x.port ? ":" + x.port : ""}`,
          action: () => {
            f.host.value = x.host;
            f.user.value = x.user;
            f.port.value = x.port;
            f.password.value = "";
            f.password.focus();
          },
        }))
      : [{ label: "No recent servers", disabled: true }];
    items.push("-", { label: "Clear history", action: () => store("fdm.recent", []) });
    popup(r.left, r.bottom + 2, items);
  });
}

// ---------------------------------------------------------------------------
// Site Manager
// ---------------------------------------------------------------------------

const PROTOCOLS = [
  ["ftp", "FTP - Use explicit FTP over TLS if available"],
  ["ftpes", "FTP - Require explicit FTP over TLS"],
  ["ftps", "FTP - Require implicit FTP over TLS"],
  ["ftp-plain", "FTP - Only use plain FTP (insecure)"],
  ["s3", "S3 - Amazon S3 / S3-compatible storage"],
];

const S3_PRESETS = [
  ["", "Amazon S3 (AWS)"],
  ["https://s3.us-west-000.backblazeb2.com", "Backblaze B2"],
  ["https://<account-id>.r2.cloudflarestorage.com", "Cloudflare R2"],
  ["https://s3.wasabisys.com", "Wasabi"],
  ["https://<region>.digitaloceanspaces.com", "DigitalOcean Spaces"],
  ["http://localhost:9000", "MinIO / self-hosted"],
];

async function siteManager() {
  let sites = [];
  try {
    sites = await invoke("sites_load");
  } catch (e) {
    alertDlg("Site Manager", `Could not read saved sites: ${errText(e)}`);
  }
  let cur = sites.length ? 0 : -1;

  const root = el("div", { class: "sm" });
  const listbox = el("div", { class: "listbox" });
  const form = el("form", { autocomplete: "off", onsubmit: (e) => e.preventDefault() });

  const field = (label, input, cls) => [el("label", { class: cls }, label), input];
  const inp = (name, attrs = {}) => el("input", { name, spellcheck: "false", ...attrs });

  const proto = el("select", { name: "protocol" }, PROTOCOLS.map(([v, l]) => el("option", { value: v }, l)));
  const preset = el(
    "select",
    { name: "preset" },
    S3_PRESETS.map(([v, l]) => el("option", { value: v }, l)),
  );
  const storage = el(
    "select",
    { name: "storageClass" },
    ["", "STANDARD", "STANDARD_IA", "ONEZONE_IA", "INTELLIGENT_TIERING", "GLACIER_IR", "GLACIER", "DEEP_ARCHIVE"].map((v) =>
      el("option", { value: v }, v || "(bucket default)"),
    ),
  );
  const sse = el(
    "select",
    { name: "sse" },
    [["", "None"], ["AES256", "AES-256 (SSE-S3)"], ["aws:kms", "AWS KMS (SSE-KMS)"]].map(([v, l]) => el("option", { value: v }, l)),
  );

  form.append(
    ...field("Name:", inp("name")),
    ...field("Protocol:", proto),
    el(
      "div",
      { class: "ftp-only" },
      ...field("Host:", el("div", { class: "row" }, inp("host", { placeholder: "ftp.example.com" }), el("span", {}, "Port:"), inp("port", { inputmode: "numeric" }))),
      ...field("User:", inp("user")),
      ...field("Password:", inp("password", { type: "password" })),
      el("label", {}, ""),
      el("label", { class: "check" }, el("input", { type: "checkbox", name: "insecure" }), "Accept invalid / self-signed certificates"),
    ),
    el(
      "div",
      { class: "s3-only" },
      ...field("Provider:", preset),
      ...field("Endpoint:", inp("endpoint", { placeholder: "empty = Amazon S3" })),
      ...field("Region:", inp("region", { placeholder: "e.g. us-east-1" })),
      ...field("Access key ID:", inp("accessKey")),
      ...field("Secret key:", inp("secretKey", { type: "password" })),
      ...field("AWS profile:", inp("profile", { placeholder: "optional, from ~/.aws" })),
      el("div", { class: "hint" }, "Leave keys empty to use the AWS environment variables or ~/.aws."),
      el("label", {}, ""),
      el("label", { class: "check" }, el("input", { type: "checkbox", name: "pathStyle" }), "Use path-style requests (MinIO, most S3-compatible services)"),
      el("h3", {}, "Uploads"),
      ...field("Storage class:", storage),
      ...field("Encryption:", sse),
    ),
    el("label", {}, ""),
    el("label", { class: "check" }, el("input", { type: "checkbox", name: "savePassword" }), "Save password / secret key"),
    el("div", { class: "hint" }, "Stored in the system keyring (GNOME Keyring / Secret Service), never in the sites file."),
    el("h3", {}, "Advanced"),
    ...field("Default remote dir:", inp("remoteDir")),
    ...field("Default local dir:", inp("localDir")),
  );

  const testBtn = el("button", { type: "button" }, "Test connection");
  const testNote = el("span", { class: "muted" });
  const testOut = el("div", { class: "wide" });
  form.append(el("div", { class: "test" }, testBtn, testNote), testOut);

  const runTest = async () => {
    read();
    if (cur < 0) return;
    const site = { ...sites[cur] };
    if (site.protocol !== "s3" && !site.host) {
      testOut.replaceChildren(el("div", { class: "test-result fail" }, el("div", { class: "head" }, el("span", { class: "mark" }, "✘"), "Please enter a host.")));
      return;
    }
    testBtn.disabled = true;
    testNote.textContent = "Testing…";
    testOut.replaceChildren();
    let r;
    try {
      r = await invoke("test_connection", { site });
    } catch (e) {
      r = { ok: false, summary: errText(e), details: [], transcript: [], elapsedMs: 0 };
    }
    testBtn.disabled = false;
    testNote.textContent = "";
    const box = el(
      "div",
      { class: `test-result ${r.ok ? "ok" : "fail"}` },
      el(
        "div",
        { class: "head" },
        el("span", { class: "mark" }, r.ok ? "✔" : "✘"),
        r.ok ? "Connection successful" : "Connection failed",
        r.elapsedMs ? el("span", { class: "time" }, `${(r.elapsedMs / 1000).toFixed(2)} s`) : null,
      ),
      el("p", { class: "msg" }, r.summary),
      r.details.length ? el("ul", {}, r.details.map((d) => el("li", {}, d))) : null,
      r.transcript.length ? el("details", {}, el("summary", {}, "Show protocol log"), el("pre", {}, r.transcript.join("\n"))) : null,
    );
    if (r.certError) {
      box.append(
        el(
          "button",
          {
            type: "button",
            onclick: () => {
              form.insecure.checked = true;
              runTest();
            },
          },
          "Trust this certificate and test again",
        ),
      );
    }
    testOut.replaceChildren(box);
  };
  testBtn.addEventListener("click", runTest);

  const read = () => {
    if (cur < 0) return;
    const s = sites[cur];
    for (const f of form.elements) {
      if (!f.name || f.name === "preset") continue;
      if (f.type === "checkbox") s[f.name] = f.checked;
      else if (f.name === "port") s.port = parseInt(f.value, 10) || null;
      else s[f.name] = f.value;
    }
  };
  const write = () => {
    form.hidden = cur < 0;
    if (cur < 0) return;
    const s = sites[cur];
    for (const f of form.elements) {
      if (!f.name || f.name === "preset") continue;
      if (f.type === "checkbox") f.checked = !!s[f.name];
      else f.value = s[f.name] ?? "";
    }
    if (!proto.value) proto.value = "ftp";
    const saved = s.passwordSaved ? "•••••••• saved — type to replace" : "asked when connecting";
    form.password.placeholder = saved;
    form.secretKey.placeholder = saved;
    testOut.replaceChildren();
    preset.value = S3_PRESETS.some(([v]) => v === s.endpoint) ? s.endpoint : preset.value;
    root.classList.toggle("proto-s3", proto.value === "s3");
  };
  const renderList = () => {
    listbox.replaceChildren(
      ...sites.map((s, i) =>
        el("div", { class: i === cur ? "sel" : null, onclick: () => (read(), (cur = i), renderList(), write()) }, s.name || "(unnamed)"),
      ),
    );
  };
  proto.addEventListener("change", () => root.classList.toggle("proto-s3", proto.value === "s3"));
  preset.addEventListener("change", () => {
    form.endpoint.value = preset.value;
    form.pathStyle.checked = !!preset.value && !preset.value.includes("amazonaws");
  });
  form.name.addEventListener("input", () => {
    if (cur >= 0) {
      sites[cur].name = form.name.value;
      listbox.children[cur].textContent = form.name.value || "(unnamed)";
    }
  });

  const newSite = () => {
    read();
    let n = 1;
    while (sites.some((s) => s.name === `New site${n > 1 ? " " + n : ""}`)) n++;
    sites.push({ name: `New site${n > 1 ? " " + n : ""}`, protocol: "ftp", savePassword: true });
    cur = sites.length - 1;
    renderList();
    write();
    form.name.select();
  };
  const dupSite = () => {
    if (cur < 0) return;
    read();
    sites.push({ ...sites[cur], id: "", passwordSaved: false, name: `${sites[cur].name} (copy)` });
    cur = sites.length - 1;
    renderList();
    write();
  };
  const delSite = async () => {
    if (cur < 0) return;
    sites.splice(cur, 1);
    cur = Math.min(cur, sites.length - 1);
    renderList();
    write();
  };

  root.append(
    el(
      "div",
      { class: "sites" },
      el("div", {}, "Select entry:"),
      listbox,
      el(
        "div",
        { class: "btns" },
        el("button", { type: "button", onclick: newSite }, "New site"),
        el("button", { type: "button", onclick: dupSite }, "Duplicate"),
        el("button", { type: "button", onclick: delSite, style: "grid-column:1/-1" }, "Delete"),
      ),
    ),
    form,
  );
  renderList();
  write();

  const save = async () => {
    read();
    try {
      const typed = sites.map((x) => ({ password: x.password, secretKey: x.secretKey }));
      const saved = await invoke("sites_save", { sites });
      // Keep what was typed for this session's connect; the backend never echoes secrets.
      sites = saved.map((x, i) => ({ ...x, ...(x.savePassword ? {} : typed[i]) }));
      S.sites = saved;
      return true;
    } catch (e) {
      alertDlg("Site Manager", `Could not save sites: ${errText(e)}`);
      return false;
    }
  };

  const result = await dialog("Site Manager", root, [
    { label: "Cancel", value: null },
    { label: "OK", value: "save", id: "save" },
    { label: "Connect", value: "connect", primary: true },
  ]);
  if (!result) return;
  if (!(await save())) return;
  if (result === "connect" && cur >= 0) {
    const site = { ...sites[cur] };
    if (site.protocol !== "s3" && !site.host) return alertDlg("Site Manager", "Please enter a host.");
    connect(site);
  }
}

// ---------------------------------------------------------------------------
// Layout: splitters and view toggles
// ---------------------------------------------------------------------------

function bindSplitters() {
  for (const sp of $$(".splitter")) {
    sp.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      const vertical = sp.classList.contains("v");
      const invert = sp.dataset.invert === "1";
      const host = sp.classList.contains("inner") ? sp.parentElement : vertical ? $("#panes") : document.documentElement;
      const sizeOf = (x) => (vertical ? x.getBoundingClientRect().width : x.getBoundingClientRect().height);
      const ref = invert ? sp.nextElementSibling : sp.previousElementSibling;
      const start = vertical ? e.clientX : e.clientY;
      const base = sizeOf(ref);
      sp.setPointerCapture(e.pointerId);
      sp.classList.add("drag");
      const move = (ev) => {
        const d = (vertical ? ev.clientX : ev.clientY) - start;
        const size = Math.max(40, base + (invert ? -d : d));
        host.style.setProperty(sp.dataset.var, `${size}px`);
      };
      const up = () => {
        sp.classList.remove("drag");
        sp.removeEventListener("pointermove", move);
        sp.removeEventListener("pointerup", up);
        saveLayout();
      };
      sp.addEventListener("pointermove", move);
      sp.addEventListener("pointerup", up);
    });
  }
}

const VIEW = { log: true, localTree: true, remoteTree: true, queue: true };

function applyView() {
  $("#app").classList.toggle("no-log", !VIEW.log);
  $("#app").classList.toggle("no-queue", !VIEW.queue);
  $("#local").classList.toggle("no-tree", !VIEW.localTree);
  $("#remote").classList.toggle("no-tree", !VIEW.remoteTree);
  const map = { "toggle-log": "log", "toggle-local-tree": "localTree", "toggle-remote-tree": "remoteTree", "toggle-queue": "queue" };
  for (const [cmd, key] of Object.entries(map)) $(`[data-cmd="${cmd}"]`).classList.toggle("on", VIEW[key]);
  saveLayout();
}

function saveLayout() {
  const vars = {};
  for (const v of ["--log-h", "--queue-h"]) vars[v] = document.documentElement.style.getPropertyValue(v);
  vars["--left-w"] = $("#panes").style.getPropertyValue("--left-w");
  store("fdm.layout", { vars, view: VIEW, showHidden: S.showHidden, theme: document.documentElement.dataset.theme || "" });
}

function loadLayout() {
  const l = store("fdm.layout");
  if (!l) return;
  for (const [k, v] of Object.entries(l.vars || {})) {
    if (!v) continue;
    (k === "--left-w" ? $("#panes") : document.documentElement).style.setProperty(k, v);
  }
  Object.assign(VIEW, l.view || {});
  S.showHidden = l.showHidden ?? true;
  if (l.theme) document.documentElement.dataset.theme = l.theme;
}

function setTheme(t) {
  if (t) document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
  saveLayout();
}

// ---------------------------------------------------------------------------
// Commands, menus, keyboard
// ---------------------------------------------------------------------------

const activePane = () => panes[S.activeSide] || panes.local;

const COMMANDS = {
  "site-manager": siteManager,
  "sites-dropdown": () => sitesDropdown($('[data-cmd="sites-dropdown"]')),
  "toggle-log": () => ((VIEW.log = !VIEW.log), applyView()),
  "toggle-local-tree": () => ((VIEW.localTree = !VIEW.localTree), applyView()),
  "toggle-remote-tree": () => ((VIEW.remoteTree = !VIEW.remoteTree), applyView()),
  "toggle-queue": () => ((VIEW.queue = !VIEW.queue), applyView()),
  refresh: () => {
    panes.local.refresh();
    if (S.session != null) panes.remote.refresh();
  },
  "cancel-all": () => queue.cancelAll(),
  disconnect,
  reconnect,
  "toggle-hidden": () => {
    S.showHidden = !S.showHidden;
    saveLayout();
    for (const p of Object.values(panes)) if (p.path) p.refresh();
  },
  quit: () => appWindow().close(),
  about: () =>
    alertDlg(
      "About FileDermaga",
      "FileDermaga 0.1.0\n\nFTP, FTPS and S3 client written in Rust.\nLicensed under the MIT License or the Apache License 2.0, at your option.\nThird-party licenses: /usr/share/doc/file-dermaga/THIRD-PARTY-LICENSES.html",
    ),
};

const MENUS = {
  file: () => [
    { label: "Site Manager…", key: "Ctrl+S", action: COMMANDS["site-manager"] },
    { label: "Connect to saved site…", disabled: !S.sites.length, action: COMMANDS["sites-dropdown"] },
    "-",
    { label: "Exit", key: "Ctrl+Q", action: COMMANDS.quit },
  ],
  view: () => [
    { label: "Refresh", key: "F5", action: COMMANDS.refresh },
    "-",
    { label: "Message log", checked: VIEW.log, action: COMMANDS["toggle-log"] },
    { label: "Local directory tree", checked: VIEW.localTree, action: COMMANDS["toggle-local-tree"] },
    { label: "Remote directory tree", checked: VIEW.remoteTree, action: COMMANDS["toggle-remote-tree"] },
    { label: "Transfer queue", checked: VIEW.queue, action: COMMANDS["toggle-queue"] },
    "-",
    { label: "Show hidden files", checked: S.showHidden, key: "Ctrl+H", action: COMMANDS["toggle-hidden"] },
    "-",
    { label: "Theme: follow system", checked: !document.documentElement.dataset.theme, action: () => setTheme("") },
    { label: "Theme: light", checked: document.documentElement.dataset.theme === "light", action: () => setTheme("light") },
    { label: "Theme: dark", checked: document.documentElement.dataset.theme === "dark", action: () => setTheme("dark") },
  ],
  transfer: () => [
    { label: "Upload selected local files", disabled: !S.session || !panes.local.sel.size, action: () => transferSelected(panes.local) },
    { label: "Download selected remote files", disabled: !S.session || !panes.remote.sel.size, action: () => transferSelected(panes.remote) },
    "-",
    { label: "Cancel all transfers", action: COMMANDS["cancel-all"] },
    { label: "Clear finished transfers", action: () => queue.clearFinished() },
  ],
  server: () => [
    { label: "Disconnect", key: "Ctrl+D", disabled: S.session == null, action: COMMANDS.disconnect },
    { label: "Reconnect", key: "Ctrl+R", disabled: !S.lastSite, action: COMMANDS.reconnect },
  ],
  help: () => [{ label: "About FileDermaga", action: COMMANDS.about }],
};

function bindChrome() {
  $("#toolbar").addEventListener("click", (e) => {
    const b = e.target.closest("[data-cmd]");
    if (b) COMMANDS[b.dataset.cmd]?.();
  });
  for (const m of $$(".menu")) {
    const open = () => {
      const r = m.getBoundingClientRect();
      popup(r.left, r.bottom, MENUS[m.dataset.menu]());
      m.classList.add("open");
    };
    m.addEventListener("mousedown", (e) => {
      e.preventDefault();
      m.classList.contains("open") ? closePopups() : open();
    });
    m.addEventListener("mouseenter", () => {
      if ($(".menu.open") && !m.classList.contains("open")) open();
    });
  }
  document.addEventListener("keydown", (e) => {
    if ($(".overlay")) return;
    const k = e.key.toLowerCase();
    if (e.key === "F5") {
      e.preventDefault();
      COMMANDS.refresh();
    } else if (e.ctrlKey && k === "s") {
      e.preventDefault();
      siteManager();
    } else if (e.ctrlKey && k === "q") {
      e.preventDefault();
      COMMANDS.quit();
    } else if (e.ctrlKey && k === "d") {
      e.preventDefault();
      disconnect();
    } else if (e.ctrlKey && k === "r") {
      e.preventDefault();
      reconnect();
    } else if (e.ctrlKey && k === "h") {
      e.preventDefault();
      COMMANDS["toggle-hidden"]();
    } else if (e.key === "Escape") {
      closePopups();
    }
  });
  // Suppress the webview's own context menu outside our handlers.
  document.addEventListener("contextmenu", (e) => {
    if (!e.target.closest("input")) e.preventDefault();
  });
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

async function init() {
  loadLayout();
  panes.local = new Pane("local");
  panes.remote = new Pane("remote");
  bindSplitters();
  bindChrome();
  bindQuickconnect();
  queue.bind();
  applyView();
  queue.render();

  await listen("log", (e) => log(e.payload.kind, e.payload.text));
  loadSites();
  await listen("transfer", (e) => queue.onEvent(e.payload));

  panes.remote.clear("Not connected to any server");
  setConnState("Not connected");
  const home = await invoke("local_home");
  const last = store("fdm.localPath");
  if (!(last && (await panes.local.navigate(last)))) await panes.local.navigate(home);
  panes.local.listEl.focus();
}

// Exposed for development self-tests (FILE_DERMAGA_GUI_SELFTEST, debug builds).
window.fdm = { S, panes, queue, connect, disconnect, transferSelected, invoke };

init().catch((e) => log("error", `Startup failed: ${errText(e)}`));

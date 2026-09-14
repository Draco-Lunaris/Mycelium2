//! Default static assets: written to the assets directory on first boot
//! and served from disk (DESIGN decision). Contains the stylesheet, the
//! graph visualization script (dependency-free force-directed layout), and
//! the shared client script (CSRF header injection).

use std::path::Path;

pub const STYLE_CSS: &str = r#"/* Mycelium2 default styles */
:root {
  --bg: #14171c; --panel: #1d222b; --text: #d8dee9; --muted: #8b93a1;
  --accent: #7aa2f7; --border: #2a3140; --danger: #f7768e; --ok: #9ece6a;
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font-family: system-ui, -apple-system, sans-serif; line-height: 1.5;
}
header {
  display: flex; align-items: center; gap: 1rem; padding: 0.6rem 1.2rem;
  background: var(--panel); border-bottom: 1px solid var(--border);
}
header a { color: var(--text); text-decoration: none; font-weight: 600; }
header nav { display: flex; gap: 1rem; margin-left: auto; }
header nav a { color: var(--muted); font-weight: 400; }
header nav a:hover { color: var(--accent); }
main { max-width: 60rem; margin: 0 auto; padding: 1.5rem 1.2rem; }
h1, h2, h3 { color: var(--accent); }
a { color: var(--accent); }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: 0.5rem 0.7rem; border-bottom: 1px solid var(--border); }
th { color: var(--muted); font-weight: 600; }
form label { display: block; color: var(--muted); margin: 0.6rem 0 0.2rem; }
input, textarea, select {
  width: 100%; padding: 0.5rem 0.7rem; background: var(--bg); color: var(--text);
  border: 1px solid var(--border); border-radius: 4px; font: inherit;
}
textarea { min-height: 18rem; font-family: ui-monospace, monospace; }
button {
  margin-top: 0.8rem; padding: 0.5rem 1.2rem; background: var(--accent);
  color: #10131a; border: none; border-radius: 4px; font-weight: 600; cursor: pointer;
}
button.danger { background: var(--danger); color: #fff; }
.flash { padding: 0.7rem 1rem; border-radius: 4px; margin-bottom: 1rem; }
.flash.ok { background: rgba(158,206,106,.15); border: 1px solid var(--ok); }
.flash.err { background: rgba(247,118,142,.15); border: 1px solid var(--danger); }
.muted { color: var(--muted); }
pre { background: var(--panel); padding: 1rem; border-radius: 6px; overflow-x: auto; }
#graph { width: 100%; height: 34rem; background: var(--panel); border-radius: 6px; }

/* Graph — full-viewport interactive layout (body.graph-page) */
body.graph-page { display: flex; flex-direction: column; height: 100vh; height: 100dvh; }
body.graph-page main {
  display: flex; flex-direction: column; flex: 1;
  max-width: none; width: 100%; margin: 0; padding: 0 1.2rem;
  min-height: 0;
}
body.graph-page h1 { margin: .8rem 0 .4rem; }
#graph-wrap { position: relative; flex: 1; min-height: 0; }
#graph {
  width: 100%; height: 100%; background: var(--panel);
  border-radius: 6px; overflow: hidden; cursor: grab; touch-action: none;
}
#graph.dragging { cursor: grabbing; }
#graph-info {
  position: absolute; top: .6rem; right: .6rem; z-index: 2;
  background: var(--bg); border: 1px solid var(--border); border-radius: 6px;
  padding: .6rem .8rem; max-width: 22rem; font-size: .9rem;
  box-shadow: 0 2px 8px rgba(0,0,0,.3);
}
#graph-info a { display: block; margin-top: .3rem; }
#graph-legend { margin: .4rem 0 .8rem; font-size: .85rem; }

/* Librarian chat — full-viewport layout (body.chat-page) */
body.chat-page { display: flex; flex-direction: column; height: 100vh; height: 100dvh; }
body.chat-page main {
  display: flex; flex-direction: column; flex: 1;
  max-width: none; width: 100%; margin: 0; padding: 0;
  min-height: 0; /* allow children to shrink */
}
.chat-shell {
  display: flex; flex-direction: column; flex: 1;
  min-height: 0; width: 100%; max-width: 60rem; margin: 0 auto;
  padding: 0 1.2rem 1rem;
}
.chat-intro { padding-top: 0.8rem; }
.chat-intro h1 { margin: 0 0 0.2rem; font-size: 1.3rem; }
.chat-intro p { margin: 0 0 0.6rem; font-size: 0.85rem; }
.chat-log {
  display: flex; flex-direction: column; gap: 0.8rem;
  padding: 1rem; margin-bottom: 0.8rem;
  flex: 1 1 auto; min-height: 4rem; overflow-y: auto;
  background: var(--panel); border: 1px solid var(--border);
  border-radius: 6px; overscroll-behavior: contain;
}
.chat-input-bar {
  position: sticky; bottom: 0; flex: 0 0 auto;
  display: flex; gap: 0.6rem; align-items: flex-end;
  padding: 0.6rem 0 0; background: var(--bg);
}
.chat-input-bar textarea {
  flex: 1; min-height: 2.6rem; max-height: 10rem; resize: none;
  font-family: inherit; line-height: 1.4;
}
.chat-input-bar button { margin: 0; flex: 0 0 auto; height: 2.6rem; }
.chat-msg {
  max-width: 80%; padding: 0.6rem 0.9rem; border-radius: 10px;
  white-space: pre-wrap; word-wrap: break-word; line-height: 1.45;
}
.chat-msg.user {
  align-self: flex-end; background: var(--accent); color: #10131a;
  border-bottom-right-radius: 2px;
}
.chat-msg.librarian {
  align-self: flex-start; background: var(--bg);
  border: 1px solid var(--border); border-bottom-left-radius: 2px;
}
.chat-msg.pending { color: var(--muted); font-style: italic; }
.chat-msg.librarian > *:first-child { margin-top: 0; }
.chat-msg.librarian > *:last-child { margin-bottom: 0; }
.chat-msg.librarian p { margin: 0.4rem 0; }
.chat-msg.librarian pre {
  background: var(--panel); padding: 0.6rem; margin: 0.4rem 0;
  font-size: 0.85rem; white-space: pre-wrap;
}
.chat-msg.librarian code {
  background: var(--panel); padding: 0.1rem 0.3rem; border-radius: 3px;
  font-size: 0.9em;
}
.chat-msg.librarian pre code { background: none; padding: 0; }
.chat-msg.librarian ul, .chat-msg.librarian ol { margin: 0.4rem 0; padding-left: 1.4rem; }
.chat-msg.librarian h3, .chat-msg.librarian h4, .chat-msg.librarian h5, .chat-msg.librarian h6 {
  margin: 0.6rem 0 0.2rem; font-size: 1rem;
}
.chat-msg.error {
  align-self: flex-start; background: rgba(247,118,142,.12);
  border: 1px solid var(--danger); color: var(--danger);
}
"#;

pub const APP_JS: &str = r#"// CSRF: attach the session's CSRF token to every fetch/form request.
(function () {
  var meta = document.querySelector('meta[name="csrf-token"]');
  var token = meta ? meta.getAttribute("content") : "";
  var origFetch = window.fetch;
  window.fetch = function (input, init) {
    init = init || {};
    init.headers = new Headers(init.headers || {});
    init.headers.set("x-csrf-token", token);
    return origFetch(input, init);
  };
  document.addEventListener("submit", function (e) {
    var form = e.target;
    if (!form.querySelector('input[name="csrf_token"]')) {
      var hidden = document.createElement("input");
      hidden.type = "hidden"; hidden.name = "csrf_token"; hidden.value = token;
      form.appendChild(hidden);
    }
  });
})();
"#;

pub const GRAPH_JS: &str = r##"// Interactive force-directed graph for /graph: drag nodes (the
// layout re-settles around them, d3-style), wheel-zoom, background-pan,
// click-to-open, hover info panel, per-type palette + legend (v1).
(function () {
  var el = document.getElementById("graph");
  var info = document.getElementById("graph-info");
  if (!el) return;

  fetch("/api/v1/graph")
    .then(function (r) { return r.json(); })
    .then(function (data) { render(data); })
    .catch(function (e) { el.textContent = "graph load failed: " + e; });

  // v1's palette, assigned to concept types in first-seen order.
  var PALETTE = ["#64c8ff", "#a78bfa", "#34d399", "#fbbf24", "#f87171", "#f472b6", "#2dd4bf", "#a3e635"];

  function render(data) {
    var nodes = data.nodes.map(function (n) {
      return { id: n.id, title: n.title, type: n.type || "unknown",
               x: 400 + (Math.random() - 0.5) * 200,
               y: 300 + (Math.random() - 0.5) * 200,
               vx: 0, vy: 0, fixed: false, degree: 0 };
    });
    var byId = {}; nodes.forEach(function (n) { byId[n.id] = n; });
    var links = data.edges.map(function (e) {
      return { source: byId[e.from], target: byId[e.to] };
    }).filter(function (l) { return l.source && l.target; });
    links.forEach(function (l) { l.source.degree++; l.target.degree++; });

    // Type colors: palette order by first-seen type (v1 semantics).
    var typeColors = {};
    nodes.forEach(function (n) {
      if (!(n.type in typeColors)) {
        typeColors[n.type] = PALETTE[Object.keys(typeColors).length % PALETTE.length];
      }
    });

    var svgNS = "http://www.w3.org/2000/svg";
    var root = document.createElementNS(svgNS, "svg");
    root.setAttribute("width", "100%");
    root.setAttribute("height", "100%");
    el.appendChild(root);
    var view = document.createElementNS(svgNS, "g");
    root.appendChild(view);
    var edgeGroup = document.createElementNS(svgNS, "g");
    var nodeGroup = document.createElementNS(svgNS, "g");
    var labelGroup = document.createElementNS(svgNS, "g");
    view.appendChild(edgeGroup); view.appendChild(nodeGroup); view.appendChild(labelGroup);

    var lines = links.map(function (l) {
      var line = document.createElementNS(svgNS, "line");
      line.setAttribute("stroke", "#2a3140");
      edgeGroup.appendChild(line); return line;
    });
    var circles = nodes.map(function (n) {
      var g = document.createElementNS(svgNS, "g");
      var c = document.createElementNS(svgNS, "circle");
      c.setAttribute("r", Math.max(5, 4 + Math.sqrt(n.degree) * 2));
      c.setAttribute("fill", typeColors[n.type]);
      c.setAttribute("stroke", "#1a1b26"); c.setAttribute("stroke-width", "1");
      c.style.cursor = "grab";
      // Orphan highlight (v1): unlinked nodes get a red ring.
      if (n.degree === 0) {
        var ring = document.createElementNS(svgNS, "circle");
        ring.setAttribute("r", Math.max(5, 4 + Math.sqrt(n.degree) * 2) + 3);
        ring.setAttribute("fill", "none");
        ring.setAttribute("stroke", "#ef4444");
        ring.setAttribute("stroke-width", "1.5");
        ring.setAttribute("opacity", "0.8");
        g.appendChild(ring);
      }
      var t = document.createElementNS(svgNS, "title");
      t.textContent = n.title + " (" + n.id + ")";
      g.appendChild(c); g.appendChild(t);
      g.addEventListener("click", function () {
        if (dragMoved) return; // it was a drag, not a click
        window.location.href = "/concept?path=" + encodeURIComponent(n.id);
      });
      g.addEventListener("mouseenter", function () { showInfo(n); });
      g.addEventListener("mouseleave", hideInfo);
      nodeGroup.appendChild(g); return g;
    });
    var labels = nodes.map(function (n) {
      var t = document.createElementNS(svgNS, "text");
      t.setAttribute("fill", "#a9b1d6"); t.setAttribute("font-size", "10");
      t.setAttribute("text-anchor", "middle");
      t.setAttribute("pointer-events", "none");
      t.textContent = n.title.length > 24 ? n.title.slice(0, 23) + "\u2026" : n.title;
      labelGroup.appendChild(t); return t;
    });

    // Legend (v1): type swatches + orphan marker, top-left.
    var legend = document.createElement("div");
    legend.style.cssText =
      "position:absolute;top:.6rem;left:.6rem;z-index:2;background:rgba(26,27,38,.85);" +
      "border:1px solid #2a3140;border-radius:6px;padding:.5rem .7rem;font-size:.8rem;";
    Object.keys(typeColors).forEach(function (t) {
      var row = document.createElement("div");
      row.style.cssText = "display:flex;align-items:center;gap:.4rem;margin:.15rem 0;";
      var sw = document.createElement("span");
      sw.style.cssText = "width:.6rem;height:.6rem;border-radius:50%;background:" + typeColors[t] + ";";
      var name = document.createElement("span");
      name.style.color = "#a9b1d6";
      name.textContent = t;
      row.appendChild(sw); row.appendChild(name);
      legend.appendChild(row);
    });
    if (nodes.some(function (n) { return n.degree === 0; })) {
      var row = document.createElement("div");
      row.style.cssText = "display:flex;align-items:center;gap:.4rem;margin:.15rem 0;border-top:1px solid #2a3140;padding-top:.25rem;";
      var sw = document.createElement("span");
      sw.style.cssText = "width:.6rem;height:.6rem;border-radius:50%;border:1px solid #ef4444;";
      var name = document.createElement("span");
      name.style.color = "#a9b1d6";
      name.textContent = "orphan (unlinked)";
      row.appendChild(sw); row.appendChild(name);
      legend.appendChild(row);
    }
    el.appendChild(legend);

    function showInfo(n) {
      if (!info) return;
      info.hidden = false;
      info.innerHTML = "";
      var b = document.createElement("b"); b.textContent = n.title;
      info.appendChild(b);
      var type = document.createElement("div");
      type.textContent = (n.type ? n.type : "concept") + " \u00b7 " + n.degree + " link" + (n.degree === 1 ? "" : "s");
      info.appendChild(type);
      var path = document.createElement("div");
      path.style.fontFamily = "monospace"; path.textContent = n.id;
      info.appendChild(path);
      var a = document.createElement("a");
      a.href = "/concept?path=" + encodeURIComponent(n.id);
      a.textContent = "open concept \u2192";
      info.appendChild(a);
    }
    function hideInfo() { if (info) info.hidden = true; }

    // --- pan / zoom ---
    var scale = 1, panX = 0, panY = 0;
    function applyView() {
      view.setAttribute("transform", "translate(" + panX + "," + panY + ") scale(" + scale + ")");
    }
    var panning = false, lastPX = 0, lastPY = 0;
    el.addEventListener("mousedown", function (e) {
      // A press on a node (or anything inside the node layer) is a node
      // drag, never a pan.
      if (nodeGroup.contains(e.target)) return;
      panning = true; lastPX = e.clientX; lastPY = e.clientY;
      el.classList.add("dragging");
    });
    window.addEventListener("mousemove", function (e) {
      if (!panning) return;
      panX += e.clientX - lastPX; panY += e.clientY - lastPY;
      lastPX = e.clientX; lastPY = e.clientY;
      applyView();
    });
    window.addEventListener("mouseup", function () {
      panning = false; el.classList.remove("dragging");
    });
    el.addEventListener("wheel", function (e) {
      e.preventDefault();
      var rect = el.getBoundingClientRect();
      var mx = e.clientX - rect.left, my = e.clientY - rect.top;
      var factor = e.deltaY < 0 ? 1.12 : 1 / 1.12;
      var ns = Math.min(4, Math.max(0.2, scale * factor));
      panX = mx - (mx - panX) * (ns / scale);
      panY = my - (my - panY) * (ns / scale);
      scale = ns; applyView();
    }, { passive: false });

    // --- node dragging (d3-style: reheat the simulation) ---
    var dragged = null, dragMoved = false;
    var alpha = 1, tickScheduled = false;
    circles.forEach(function (g, i) {
      g.addEventListener("mousedown", function (e) {
        dragged = nodes[i];
        dragged.fixed = true;
        dragMoved = false;
        alpha = Math.max(alpha, 0.3); // reheat
        scheduleTick();
        e.stopPropagation();
        e.preventDefault();
      });
    });
    window.addEventListener("mousemove", function (e) {
      if (!dragged) return;
      dragMoved = true;
      var rect = el.getBoundingClientRect();
      dragged.x = (e.clientX - rect.left - panX) / scale;
      dragged.y = (e.clientY - rect.top - panY) / scale;
      dragged.vx = 0; dragged.vy = 0;
      alpha = Math.max(alpha, 0.25); // keep the layout live while holding
    });
    window.addEventListener("mouseup", function () {
      if (dragged) {
        dragged.fixed = false;
        dragged = null;
        alpha = Math.max(alpha, 0.3); // reheat: let the graph settle
        scheduleTick();
      }
    });

    // --- simulation (restartable) ---
    function scheduleTick() {
      if (tickScheduled) return;
      tickScheduled = true;
      requestAnimationFrame(function () { tickScheduled = false; tick(); });
    }
    function tick() {
      alpha *= 0.99;
      var W = el.clientWidth || 900, H = el.clientHeight || 500;
      for (var i = 0; i < nodes.length; i++) {
        var a = nodes[i];
        for (var j = i + 1; j < nodes.length; j++) {
          var b = nodes[j];
          var dx = b.x - a.x, dy = b.y - a.y;
          var d2 = dx * dx + dy * dy || 0.01;
          var f = (1200 / d2) * alpha;
          var d = Math.sqrt(d2);
          a.vx -= (dx / d) * f; a.vy -= (dy / d) * f;
          b.vx += (dx / d) * f; b.vy += (dy / d) * f;
        }
      }
      links.forEach(function (l) {
        var dx = l.target.x - l.source.x, dy = l.target.y - l.source.y;
        var d = Math.sqrt(dx * dx + dy * dy) || 0.01;
        var f = ((d - 120) * 0.02) * alpha;
        l.source.vx += (dx / d) * f; l.source.vy += (dy / d) * f;
        l.target.vx -= (dx / d) * f; l.target.vy -= (dy / d) * f;
      });
      nodes.forEach(function (n) {
        if (n.fixed) return; // the dragged node is positioned by the cursor
        n.vx += (W / 2 - n.x) * 0.002 * alpha;
        n.vy += (H / 2 - n.y) * 0.002 * alpha;
        n.x += n.vx * 0.5; n.y += n.vy * 0.5;
        n.vx *= 0.85; n.vy *= 0.85;
        n.x = Math.max(20, Math.min(W - 20, n.x));
        n.y = Math.max(20, Math.min(H - 20, n.y));
      });
      links.forEach(function (l, i) {
        lines[i].setAttribute("x1", l.source.x); lines[i].setAttribute("y1", l.source.y);
        lines[i].setAttribute("x2", l.target.x); lines[i].setAttribute("y2", l.target.y);
      });
      nodes.forEach(function (n, i) {
        circles[i].setAttribute("transform", "translate(" + n.x + "," + n.y + ")");
        labels[i].setAttribute("x", n.x);
        labels[i].setAttribute("y", n.y + 18);
      });
      // Stay alive while the layout is hot OR a node is held.
      if (alpha > 0.02 || dragged) scheduleTick();
    }
    scheduleTick();
  }
})();
"##;

/// The chat page's client logic (external asset — CSP-safe: the site
/// policy is script-src 'self' + nonce, so inline scripts are blocked;
/// /assets/chat.js loads under 'self').
pub const CHAT_JS: &str = r#"// Librarian chat: stream the agent via /api/v1/chat/stream (SSE).
(function () {
  var log = document.getElementById("chat-log");
  var form = document.getElementById("chat-form");
  var input = document.getElementById("chat-input");
  var sendBtn = form ? form.querySelector("button[type=submit]") : null;
  var busy = false;
  function addMsg(text, who) {
    var div = document.createElement("div");
    div.className = "chat-msg " + who;
    if (who.indexOf("librarian") === 0) {
      renderMarkdown(div, text);
    } else {
      div.textContent = text;
    }
    log.appendChild(div);
    log.scrollTop = log.scrollHeight;
    return div;
  }
  // Minimal XSS-safe markdown renderer: builds DOM nodes only (never
  // innerHTML with model text). Supports: fenced code blocks, headings,
  // bullet lists, inline code, bold, and [text](/path) links (relative
  // or same-origin only — no javascript:, no external hrefs).
  function renderMarkdown(container, text) {
    var lines = text.split("\n");
    var i = 0;
    var list = null;
    function flushList() { if (list) { list = null; } }
    function inline(parent, str) {
      // Split on `code`, **bold**, and [text](url) — build nodes.
      var re = /(`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\([^)]+\))/g;
      var last = 0, m;
      while ((m = re.exec(str)) !== null) {
        if (m.index > last) parent.appendChild(document.createTextNode(str.slice(last, m.index)));
        var tok = m[0];
        if (tok.charAt(0) === "`") {
          var code = document.createElement("code");
          code.textContent = tok.slice(1, -1);
          parent.appendChild(code);
        } else if (tok.charAt(0) === "*") {
          var b = document.createElement("strong");
          b.textContent = tok.slice(2, -2);
          parent.appendChild(b);
        } else {
          var lm = /^\[([^\]]+)\]\(([^)]+)\)$/.exec(tok);
          if (lm) {
            var url = lm[2];
            var ok = url.charAt(0) === "/" || url.indexOf("https://") === 0;
            if (ok) {
              var a = document.createElement("a");
              a.textContent = lm[1];
              a.href = url;
              if (url.indexOf("http") === 0) a.rel = "noopener noreferrer";
              parent.appendChild(a);
            } else {
              parent.appendChild(document.createTextNode(lm[1] + " (" + url + ")"));
            }
          } else {
            parent.appendChild(document.createTextNode(tok));
          }
        }
        last = m.index + tok.length;
      }
      if (last < str.length) parent.appendChild(document.createTextNode(str.slice(last)));
    }
    while (i < lines.length) {
      var line = lines[i];
      if (line.indexOf("```") === 0) {
        flushList();
        var pre = document.createElement("pre");
        var codeEl = document.createElement("code");
        i++;
        var codeLines = [];
        while (i < lines.length && lines[i].indexOf("```") !== 0) {
          codeLines.push(lines[i]);
          i++;
        }
        i++; // skip closing fence
        codeEl.textContent = codeLines.join("\n");
        pre.appendChild(codeEl);
        container.appendChild(pre);
        continue;
      }
      var h = /^(#{1,4}) (.*)$/.exec(line);
      if (h) {
        flushList();
        var heading = document.createElement("h" + (h[1].length + 2 > 6 ? 6 : h[1].length + 2));
        inline(heading, h[2]);
        container.appendChild(heading);
      } else if (/^[-*] /.test(line)) {
        if (!list) { list = document.createElement("ul"); container.appendChild(list); }
        var li = document.createElement("li");
        inline(li, line.slice(2));
        list.appendChild(li);
      } else if (/^(\d+)\. /.test(line)) {
        if (!list) { list = document.createElement("ol"); container.appendChild(list); }
        var oli = document.createElement("li");
        inline(oli, line.replace(/^\d+\. /, ""));
        list.appendChild(oli);
      } else if (line.trim() === "") {
        flushList();
      } else {
        flushList();
        var p = document.createElement("p");
        inline(p, line);
        container.appendChild(p);
      }
      i++;
    }
  }
  function setBusy(state) {
    busy = state;
    if (sendBtn) sendBtn.disabled = state;
  }
  // Auto-grow the textarea to its content (capped by CSS max-height).
  function autoGrow() {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 160) + "px";
  }
  input.addEventListener("input", autoGrow);
  // Enter sends; Shift+Enter inserts a newline.
  input.addEventListener("keydown", function (ev) {
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      form.requestSubmit();
    }
  });
  form.addEventListener("submit", function (ev) {
    ev.preventDefault();
    if (busy) return;
    var msg = input.value.trim();
    if (!msg) return;
    addMsg(msg, "user");
    input.value = "";
    setBusy(true);
    var pending = addMsg("The librarian is thinking…", "librarian pending");
    var steps = [];
    function renderPending() {
      var text = "The librarian is thinking…";
      if (steps.length) text += "\n\n" + steps.join("\n");
      pending.textContent = text;
      log.scrollTop = log.scrollHeight;
    }
    var csrfMeta = document.querySelector('meta[name="csrf-token"]');
    var body = new URLSearchParams();
    body.set("message", msg);
    body.set("csrf_token", csrfMeta ? csrfMeta.content : "");
    fetch("/api/v1/chat/stream", {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        "x-csrf-token": csrfMeta ? csrfMeta.content : "",
        "accept": "text/event-stream"
      },
      body: body.toString()
    }).then(function (r) {
      if (!r.ok || !r.body) {
        return r.json().then(function (j) {
          throw new Error(j.error || ("HTTP " + r.status));
        });
      }
      var reader = r.body.getReader();
      var decoder = new TextDecoder();
      var buf = "";
      function pump() {
        return reader.read().then(function (chunk) {
          if (chunk.done) { finish(); return; }
          buf += decoder.decode(chunk.value, { stream: true });
          var parts = buf.split("\n\n");
          buf = parts.pop();
          parts.forEach(function (part) {
            var line = part.replace(/^data: /, "");
            if (!line) return;
            var ev;
            try { ev = JSON.parse(line); } catch (e) { return; }
            if (ev.type === "tool") {
              steps.push("· " + ev.name + (ev.detail ? " (" + ev.detail + ")" : ""));
              renderPending();
            } else if (ev.type === "done") {
              pending.remove();
              addMsg(ev.reply, "librarian");
              setBusy(false);
            } else if (ev.type === "error") {
              pending.remove();
              addMsg(ev.error, "error");
              setBusy(false);
            }
          });
          return pump();
        });
      }
      function finish() {
        // Stream ended without a done/error event (e.g. connection
        // drop): clear the pending state.
        if (busy) {
          pending.remove();
          addMsg("connection closed", "error");
          setBusy(false);
        }
      }
      return pump();
    }).catch(function (err) {
      pending.remove();
      setBusy(false);
      addMsg(err.message || "request failed", "error");
    });
  });
})();
"#;

/// The default assets' content version. Bumped when the built-in
/// defaults change; a mismatching (or missing) marker file triggers a
/// refresh, so upgrades deliver new defaults while admins can still
/// customize (delete the marker to opt out of refreshes, or restore it
/// to re-opt-in on the next boot).
pub const ASSETS_VERSION: &str = "7";

/// Write the default assets to `assets_dir`. First boot writes
/// everything; later boots refresh the defaults when the version
/// marker is stale (upgrade path) — a present, matching marker means
/// leave the files alone (admin customization preserved).
pub fn scaffold_defaults(assets_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(assets_dir)?;
    let marker = assets_dir.join(".defaults-version");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    let refresh = current.trim() != ASSETS_VERSION;
    let files = [
        ("style.css", STYLE_CSS),
        ("app.js", APP_JS),
        ("graph.js", GRAPH_JS),
        ("chat.js", CHAT_JS),
    ];
    for (name, contents) in files {
        let path = assets_dir.join(name);
        if !path.exists() || refresh {
            std::fs::write(path, contents)?;
        }
    }
    if refresh {
        std::fs::write(&marker, ASSETS_VERSION)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffolds_once() {
        let dir = tempfile::tempdir().unwrap();
        scaffold_defaults(dir.path()).unwrap();
        assert!(dir.path().join("style.css").exists());
        assert!(dir.path().join("app.js").exists());
        assert!(dir.path().join("graph.js").exists());
        assert!(dir.path().join("chat.js").exists());
        // Same version: does not overwrite (admin customization safe).
        std::fs::write(dir.path().join("style.css"), "custom").unwrap();
        scaffold_defaults(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("style.css")).unwrap(),
            "custom"
        );
    }

    #[test]
    fn version_bump_refreshes_defaults() {
        let dir = tempfile::tempdir().unwrap();
        scaffold_defaults(dir.path()).unwrap();
        // Simulate an old-version deployment with customized assets.
        std::fs::write(dir.path().join("style.css"), "old custom").unwrap();
        std::fs::write(dir.path().join(".defaults-version"), "1").unwrap();
        scaffold_defaults(dir.path()).unwrap();
        // Refreshed to the current defaults + marker updated.
        assert!(
            std::fs::read_to_string(dir.path().join("style.css"))
                .unwrap()
                .contains("--bg:")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".defaults-version")).unwrap(),
            ASSETS_VERSION
        );
        // And idempotent again.
        std::fs::write(dir.path().join("style.css"), "new custom").unwrap();
        scaffold_defaults(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("style.css")).unwrap(),
            "new custom"
        );
    }
}

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

pub const GRAPH_JS: &str = r##"// Dependency-free force-directed graph renderer for /graph.
(function () {
  var el = document.getElementById("graph");
  if (!el) return;
  fetch("/api/v1/graph")
    .then(function (r) { return r.json(); })
    .then(function (data) { render(data); })
    .catch(function (e) { el.textContent = "graph load failed: " + e; });

  function render(data) {
    var nodes = data.nodes.map(function (n) {
      return { id: n.id, title: n.title, x: 400 + (Math.random() - 0.5) * 200,
               y: 300 + (Math.random() - 0.5) * 200, vx: 0, vy: 0 };
    });
    var byId = {}; nodes.forEach(function (n) { byId[n.id] = n; });
    var links = data.edges.map(function (e) {
      return { source: byId[e.from], target: byId[e.to] };
    }).filter(function (l) { return l.source && l.target; });

    var W = el.clientWidth || 900, H = el.clientHeight || 500;
    var svg = "http://www.w3.org/2000/svg";
    var root = document.createElementNS(svg, "svg");
    root.setAttribute("width", W); root.setAttribute("height", H);
    el.appendChild(root);
    var edgeGroup = document.createElementNS(svg, "g");
    var nodeGroup = document.createElementNS(svg, "g");
    root.appendChild(edgeGroup); root.appendChild(nodeGroup);

    var lines = links.map(function (l) {
      var line = document.createElementNS(svg, "line");
      line.setAttribute("stroke", "#2a3140");
      edgeGroup.appendChild(line); return line;
    });
    var circles = nodes.map(function (n) {
      var g = document.createElementNS(svg, "g");
      var c = document.createElementNS(svg, "circle");
      c.setAttribute("r", 6); c.setAttribute("fill", "#7aa2f7");
      var t = document.createElementNS(svg, "title");
      t.textContent = n.title + " (" + n.id + ")";
      g.appendChild(c); g.appendChild(t);
      g.addEventListener("click", function () {
        window.location.href = "/concept?path=" + encodeURIComponent(n.id);
      });
      nodeGroup.appendChild(g); return g;
    });

    var alpha = 1;
    function tick() {
      alpha *= 0.985;
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
      });
      if (alpha > 0.02) requestAnimationFrame(tick);
    }
    tick();
  }
})();
"##;

/// Write the default assets to `assets_dir` if not present (first boot).
pub fn scaffold_defaults(assets_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(assets_dir)?;
    let files = [
        ("style.css", STYLE_CSS),
        ("app.js", APP_JS),
        ("graph.js", GRAPH_JS),
    ];
    for (name, contents) in files {
        let path = assets_dir.join(name);
        if !path.exists() {
            std::fs::write(path, contents)?;
        }
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
        // Idempotent: does not overwrite.
        std::fs::write(dir.path().join("style.css"), "custom").unwrap();
        scaffold_defaults(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("style.css")).unwrap(),
            "custom"
        );
    }
}

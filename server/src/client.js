/*
 * Live Reload browser client.
 *
 * Injected into every HTML response. Kept dependency free and deliberately
 * small, since it runs inside the user's page and must not disturb it.
 */
(function () {
  "use strict";

  if (window.__liveReloadInstalled) return;
  window.__liveReloadInstalled = true;

  var ENDPOINT = "__live_reload";
  var SCROLL_KEY = "__liveReloadScroll";
  var RETRY_MIN = 250;
  var RETRY_MAX = 5000;

  var socket = null;
  var retry = RETRY_MIN;
  var everConnected = false;
  var badge = null;
  var badgeTimer = null;

  /* ---------------------------------------------------------------- scroll */

  // A full reload otherwise throws away the reader's position, which makes
  // editing anything below the fold miserable.
  function rememberScroll() {
    try {
      sessionStorage.setItem(
        SCROLL_KEY,
        JSON.stringify({ x: window.scrollX, y: window.scrollY, path: location.pathname })
      );
    } catch (e) {
      /* private browsing, or storage disabled */
    }
  }

  function restoreScroll() {
    try {
      var raw = sessionStorage.getItem(SCROLL_KEY);
      if (!raw) return;
      sessionStorage.removeItem(SCROLL_KEY);
      var saved = JSON.parse(raw);
      if (!saved || saved.path !== location.pathname) return;
      // Wait for layout so the target offset actually exists.
      requestAnimationFrame(function () {
        requestAnimationFrame(function () {
          window.scrollTo(saved.x, saved.y);
        });
      });
    } catch (e) {
      /* corrupt entry, nothing to restore */
    }
  }

  function reload() {
    rememberScroll();
    location.reload();
  }

  /* ------------------------------------------------------------------- css */

  function bust(url, token) {
    // Resolve against the document so relative and root-relative hrefs compare
    // equal to the absolute path the server sends.
    var resolved = new URL(url, document.baseURI);
    resolved.searchParams.set("__lr", token);
    return resolved.href;
  }

  function samePath(href, path) {
    try {
      return new URL(href, document.baseURI).pathname === path;
    } catch (e) {
      return false;
    }
  }

  /*
   * Swaps a stylesheet without the flash of unstyled content that removing the
   * old link would cause: the replacement is loaded alongside the original, and
   * the original is only dropped once the new one has painted.
   */
  function swapCss(path) {
    var token = Date.now().toString(36);
    var links = Array.prototype.slice.call(
      document.querySelectorAll('link[rel~="stylesheet"][href]')
    );
    var matched = false;

    links.forEach(function (link) {
      if (path && !samePath(link.getAttribute("href"), path)) return;
      matched = true;

      var next = link.cloneNode();
      next.href = bust(link.getAttribute("href"), token);

      var done = false;
      var drop = function () {
        if (done) return;
        done = true;
        if (link.parentNode) link.parentNode.removeChild(link);
      };

      next.addEventListener("load", drop);
      // If the new sheet 404s we would otherwise leave the page with two copies
      // and no styling, so fall back to dropping the old one anyway.
      next.addEventListener("error", drop);
      setTimeout(drop, 2000);

      link.parentNode.insertBefore(next, link.nextSibling);
    });

    // The changed sheet may be pulled in by an @import, or the page may inline
    // it. A reload is the only reliable answer in that case.
    if (!matched) reload();
  }

  /* ---------------------------------------------------------------- images */

  function swapImages(path) {
    var token = Date.now().toString(36);
    var changed = false;

    Array.prototype.forEach.call(document.images, function (img) {
      var src = img.getAttribute("src");
      if (!src || (path && !samePath(src, path))) return;
      img.src = bust(src, token);
      changed = true;
    });

    Array.prototype.forEach.call(
      document.querySelectorAll("source[srcset], img[srcset]"),
      function (node) {
        var srcset = node.getAttribute("srcset");
        if (!srcset || (path && srcset.indexOf(path) === -1)) return;
        node.setAttribute(
          "srcset",
          srcset.replace(/(\S+)(\s|$)/g, function (match, url, tail) {
            return bust(url, token) + tail;
          })
        );
        changed = true;
      }
    );

    // Backgrounds and other CSS-referenced images are not reachable from the
    // DOM, so fall back to a reload when nothing matched.
    if (!changed) reload();
  }

  /* ----------------------------------------------------------------- badge */

  function showBadge(text, tone) {
    if (!document.body) return;
    if (!badge) {
      badge = document.createElement("div");
      badge.setAttribute("data-live-reload", "");
      badge.style.cssText = [
        "position:fixed",
        "z-index:2147483647",
        "left:12px",
        "bottom:12px",
        "padding:6px 10px",
        "border-radius:6px",
        "font:500 12px/1.4 ui-sans-serif,system-ui,-apple-system,Segoe UI,sans-serif",
        "color:#fff",
        "pointer-events:none",
        "opacity:0",
        "transition:opacity .15s ease",
      ].join(";");
      document.body.appendChild(badge);
    }
    badge.textContent = text;
    badge.style.background = tone === "error" ? "#b3261e" : "#1f6feb";
    badge.style.opacity = "1";

    clearTimeout(badgeTimer);
    if (tone !== "error") {
      badgeTimer = setTimeout(function () {
        if (badge) badge.style.opacity = "0";
      }, 1500);
    }
  }

  function hideBadge() {
    clearTimeout(badgeTimer);
    if (badge) badge.style.opacity = "0";
  }

  /* ---------------------------------------------------------------- socket */

  function connect() {
    var protocol = location.protocol === "https:" ? "wss:" : "ws:";
    var url = protocol + "//" + location.host + "/" + ENDPOINT + "/ws";

    try {
      socket = new WebSocket(url);
    } catch (e) {
      schedule();
      return;
    }

    socket.onopen = function () {
      retry = RETRY_MIN;
      // Reaching the server again after it went away means it restarted, and
      // the page may be stale, so pick up whatever changed while we were gone.
      if (everConnected) {
        reload();
        return;
      }
      everConnected = true;
      hideBadge();
    };

    socket.onmessage = function (event) {
      var message;
      try {
        message = JSON.parse(event.data);
      } catch (e) {
        return;
      }

      switch (message.type) {
        case "reload":
          reload();
          break;
        case "css":
          showBadge("css updated");
          swapCss(message.path);
          break;
        case "image":
          showBadge("image updated");
          swapImages(message.path);
          break;
        case "connected":
          break;
      }
    };

    socket.onclose = function () {
      socket = null;
      showBadge("live reload disconnected", "error");
      schedule();
    };

    socket.onerror = function () {
      if (socket) socket.close();
    };
  }

  function schedule() {
    setTimeout(connect, retry);
    retry = Math.min(retry * 2, RETRY_MAX);
  }

  restoreScroll();
  connect();
})();

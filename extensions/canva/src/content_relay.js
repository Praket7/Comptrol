// Comptrol Canva Companion — content relay
// Forwards postMessage events from the Canva App iframe to the
// background service worker, which relays to the localhost runtime.

(function () {
  const ALLOWED_ORIGINS = ["https://www.canva.com", "https://canva.com"];

  window.addEventListener("message", (event) => {
    if (!ALLOWED_ORIGINS.includes(event.origin)) { return; }
    const payload = event.data;
    if (!payload || typeof payload !== "object") { return; }
    try {
      chrome.runtime.sendMessage(payload, (response) => {
        if (chrome.runtime.lastError) { return; }
        if (!response || !response.ok) {
          // Optionally post error back to iframe
          if (response?.error) {
            event.source?.postMessage({ ok: false, error: response.error }, event.origin);
          }
          return;
        }
        // Forward success receipt back to iframe
        event.source?.postMessage({ ok: true, ...response }, event.origin);
      });
    } catch (_) { /* native messaging may be unavailable */ }
  });
})();
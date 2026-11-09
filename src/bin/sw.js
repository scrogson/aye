// Service worker for the home-screen app: shows approverd's pushes and opens
// the approval page when one is tapped. Pushes carry only a title, a line of
// text and a link on this machine; the command itself is never in them.

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", event => event.waitUntil(self.clients.claim()));

// Only links back into this app, never anywhere else.
function safePath(url) {
  return typeof url === "string" && url.startsWith("/") && !url.startsWith("//") ? url : "/";
}

self.addEventListener("push", event => {
  let data = {};
  try { data = event.data ? event.data.json() : {}; } catch {}
  const title = data.title || "Approval needed";
  event.waitUntil(self.registration.showNotification(title, {
    body: data.body || "",
    tag: data.tag || undefined,
    icon: "/icon-192.png",
    data: { url: safePath(data.url) },
    requireInteraction: true,
  }));
});

self.addEventListener("notificationclick", event => {
  event.notification.close();
  const url = new URL(safePath(event.notification.data && event.notification.data.url), self.location.origin).href;
  event.waitUntil((async () => {
    const open = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const client of open) {
      if (new URL(client.url).origin !== self.location.origin) continue;
      try {
        await client.focus();
        if (client.url !== url) await client.navigate(url);
        return;
      } catch {
        break; // a page this worker doesn't control can't be navigated
      }
    }
    await self.clients.openWindow(url);
  })());
});

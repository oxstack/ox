// Only the static shell is cached. Device proofs, session frames and approvals stay off disk.
const files = [
  "/",
  "/app.js",
  "/operator.js",
  "/composer-tools.js",
  "/activity.js",
  "/model.js",
  "/controls.js",
  "/move-controls.js",
  "/sky.js",
  "/settings.js",
  "/settings.css",
  "/device.js",
  "/pair-camera.js",
  "/vendor/jsQR-1.4.0.js",
  "/vendor/marked-18.3.0.js",
  "/wire.js",
  "/style.css",
  "/marquee.png",
  "/horizon.png",
  "/cartridges.png",
  "/icon.png",
  "/icon-192.png",
  "/manifest.webmanifest",
];
self.addEventListener("install", (event) =>
  event.waitUntil(
    (async () => {
      const c = await caches.open(shell);
      await c.addAll(files);
      await self.skipWaiting();
    })(),
  ),
);
self.addEventListener("activate", (event) =>
  event.waitUntil(
    (async () => {
      for (const k of await caches.keys())
        if (k.startsWith("ox-remote-shell-") && k !== shell)
          await caches.delete(k);
      await self.clients.claim();
    })(),
  ),
);
self.addEventListener("fetch", (event) => {
  const u = new URL(event.request.url);
  if (
    event.request.method !== "GET" ||
    u.origin !== self.location.origin ||
    u.search ||
    !files.includes(u.pathname)
  )
    return;
  event.respondWith(
    (async () => {
      try {
        return await fetch(event.request);
      } catch {
        const cached = await caches.match(u.pathname);
        return cached ?? Response.error();
      }
    })(),
  );
});
const labels = {
  finished: "Turn finished",
  goal_reached: "Goal reached",
  failed: "Turn failed",
  approval: "Needs you · approval",
  question: "Needs you · question",
};
self.addEventListener("push", (event) => {
  let p;
  try {
    p = event.data.json();
  } catch {
    return;
  }
  if (
    !Object.hasOwn(labels, p.kind) ||
    typeof p.thread !== "string" ||
    typeof p.title !== "string"
  )
    return;
  event.waitUntil(
    self.registration.showNotification(p.title || "OX session", {
      body: labels[p.kind],
      icon: "/icon.png",
      tag: p.thread + ":" + p.kind,
      data: { thread: p.thread },
    }),
  );
});
self.addEventListener("notificationclick", (event) => {
  const thread = event.notification.data?.thread;
  event.notification.close();
  if (typeof thread !== "string") return;
  const target = new URL("/", self.location.origin);
  target.hash = new URLSearchParams({ thread }).toString();
  event.waitUntil(
    (async () => {
      const windows = await clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      for (const client of windows)
        if (new URL(client.url).origin === target.origin) {
          await client.focus();
          client.postMessage({ openThread: thread });
          return;
        }
      await clients.openWindow(target.href);
    })(),
  );
});

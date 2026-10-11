const CACHE_PREFIX = 'ebc-battery-tester-';
const BUILD_ID = new URL(self.location.href).searchParams.get('build');
const CACHE_NAME = `${CACHE_PREFIX}${BUILD_ID || 'unversioned'}`;

self.addEventListener('install', (event) => {
  event.waitUntil((async () => {
    const shellUrl = new URL('./', self.registration.scope);
    const response = await fetch(shellUrl, { cache: 'reload' });
    if (!response.ok) {
      throw new Error(`failed to fetch app shell: HTTP ${response.status}`);
    }
    const html = await response.clone().text();
    // Trunk writes content hashes into these references, so discover them from
    // the generated page instead of coupling the worker to build filenames.
    const references = [...html.matchAll(
      /(?:src|href|from|module_or_path:)\s*(?:=\s*)?["']([^"']+\.(?:js|wasm|ico|png|json))["']/g
    )];
    const assetUrls = [...new Set(
      references
        .map((match) => new URL(match[1], shellUrl))
        .filter((url) => url.origin === self.location.origin)
        .map((url) => url.href)
    )];
    const assets = await Promise.all(assetUrls.map(async (url) => {
      const asset = await fetch(url, { cache: 'reload' });
      if (!asset.ok) {
        throw new Error(`failed to fetch ${url}: HTTP ${asset.status}`);
      }
      return [url, asset];
    }));

    // Do not create the new generation until every response is available.
    await caches.delete(CACHE_NAME);
    const cache = await caches.open(CACHE_NAME);
    try {
      await cache.put(shellUrl, response);
      for (const [url, asset] of assets) {
        await cache.put(url, asset);
      }
    } catch (error) {
      await caches.delete(CACHE_NAME);
      throw error;
    }
    await self.skipWaiting();
  })());
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((names) => Promise.all(
        names
          .filter((name) => name.startsWith(CACHE_PREFIX) && name !== CACHE_NAME)
          .map((name) => caches.delete(name))
      ))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== 'GET' || url.origin !== self.location.origin ||
      url.pathname.startsWith(`${new URL('./api/', self.registration.scope).pathname}`)) {
    return;
  }

  event.respondWith((async () => {
    const cache = await caches.open(CACHE_NAME);
    try {
      const response = await fetch(event.request, { cache: 'no-store' });
      if (response.ok) {
        await cache.put(event.request, response.clone());
      }
      return response;
    } catch (error) {
      const cached = await cache.match(event.request);
      if (cached) {
        return cached;
      }
      if (event.request.mode === 'navigate') {
        const shell = await cache.match('./');
        if (shell) {
          return shell;
        }
      }
      throw error;
    }
  })());
});

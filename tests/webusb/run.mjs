import http from 'node:http';
import fs from 'node:fs/promises';
import path from 'node:path';
import puppeteer from 'puppeteer-core';
import probe from './probe.mjs';

const dist = path.resolve(process.argv[2]);
const server = http.createServer(async (req, res) => {
  try {
    const relative = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
    const file = path.resolve(dist, `.${relative === '/' ? '/index.html' : relative}`);
    if (!file.startsWith(`${dist}${path.sep}`)) throw new Error('outside dist');
    const types = { '.html': 'text/html', '.js': 'application/javascript', '.wasm': 'application/wasm', '.json': 'application/json' };
    res.setHeader('Content-Type', types[path.extname(file)] ?? 'application/octet-stream');
    res.end(await fs.readFile(file));
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
let browser;
try {
  // Hosted CI has no physical GPU; allow Chromium's software WebGL renderer.
  browser = await puppeteer.launch({ executablePath: process.env.CHROME_PATH, headless: true, args: ['--no-sandbox', '--enable-unsafe-swiftshader'] });
  for (const section of ['queue', 'modes']) {
    const page = await browser.newPage();
    try {
      const result = await probe({ page, context: { section, url: `http://127.0.0.1:${server.address().port}/` } });
      console.log(JSON.stringify(result.data));
    } finally {
      await page.close();
    }
  }
} finally {
  if (browser) await browser.close();
  await new Promise(resolve => server.close(resolve));
}

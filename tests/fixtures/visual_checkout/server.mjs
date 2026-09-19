import http from 'node:http';
import { readFile } from 'node:fs/promises';
const port = Number(process.env.PORT || 8080);
http.createServer(async (request, response) => {
  const files = { '/': ['index.html', 'text/html'], '/button-finish.png': ['button-finish.png', 'image/png'] };
  const file = files[request.url];
  if (!file) { response.writeHead(404); response.end(); return; }
  try { response.setHeader('Content-Type', file[1]); response.end(await readFile(new URL(file[0], import.meta.url))); }
  catch { response.writeHead(500); response.end('File unavailable'); }
}).listen(port, '0.0.0.0', () => console.log(`Checkout listening on ${port}`));

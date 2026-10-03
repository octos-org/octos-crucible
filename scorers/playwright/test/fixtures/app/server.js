// Tiny fixture app for the scorer's end-to-end check (CI only).
const http = require('node:http');

const page = `<!doctype html>
<html><head><title>Fixture</title></head>
<body>
  <h1>Hello fixture</h1>
  <button id="b" onclick="document.getElementById('out').textContent = 'clicked'">Click me</button>
  <p id="out"></p>
</body></html>`;

http
  .createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    res.end(page);
  })
  .listen(Number(process.env.PORT || 3000), '0.0.0.0');

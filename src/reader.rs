//! Modalità lettore: estrae il contenuto principale della pagina e lo mostra
//! in una vista pulita, senza menu, sidebar o annunci.

/// Script iniettato nella pagina attiva. Restituisce `'ok'` quando trova un
/// articolo, `'none'` quando la pagina non sembra un articolo.
pub const READER_JS: &str = r#"
(() => {
  'use strict';
  if (document.querySelector('.peats-reader')) return 'already';

  const escape = (value) => String(value == null ? '' : value)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

  const nodes = document.body
    ? Array.from(document.body.querySelectorAll('article, main, [role="main"], div, section'))
    : [];

  let best = document.body;
  let bestScore = 0;
  for (const node of nodes) {
    const paragraphs = Array.from(node.querySelectorAll('p'));
    if (paragraphs.length < 3) continue;
    let text = 0;
    let links = 0;
    for (const p of paragraphs) {
      const content = (p.textContent || '').trim();
      if (content.length < 40) continue;
      text += content.length;
      links += p.querySelectorAll('a').length;
    }
    const score = text - links * 60;
    if (score > bestScore) {
      bestScore = score;
      best = node;
    }
  }
  if (!best || bestScore < 400) return 'none';

  const clone = best.cloneNode(true);
  clone.querySelectorAll('script, style, noscript, nav, header, footer, aside, form, iframe, button, input, select, textarea, video, audio, canvas, [aria-hidden="true"]').forEach((node) => node.remove());

  // Contenitori rimasti vuoti (menu, banner, box pubblicitari).
  clone.querySelectorAll('div, section, ul, ol').forEach((node) => {
    if (!(node.textContent || '').trim() && !node.querySelector('img, video, svg')) node.remove();
  });

  const base = document.baseURI;
  clone.querySelectorAll('a[href]').forEach((link) => {
    try {
      link.href = new URL(link.getAttribute('href'), base).href;
      link.target = '_blank';
      link.rel = 'noreferrer';
    } catch (_) {}
  });
  clone.querySelectorAll('img[src]').forEach((image) => {
    try { image.src = new URL(image.getAttribute('src'), base).href; } catch (_) {}
    image.removeAttribute('width');
    image.removeAttribute('height');
  });

  const heading = document.querySelector('h1');
  const title = ((heading && heading.innerText) || document.title || '').trim();
  const source = location.href;

  // Il titolo è già mostrato nell'intestazione: evita di ripeterlo subito sotto.
  const firstHeading = clone.querySelector('h1');
  if (firstHeading && (firstHeading.textContent || '').trim() === title) firstHeading.remove();

  const head = document.head;
  head.innerHTML = '';
  document.title = title || 'Modalità lettore';

  const style = document.createElement('style');
  style.textContent = `
    :root { color-scheme: light dark; }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      background: #fbfaf8;
      color: #1f2023;
      font-family: system-ui, -apple-system, "Segoe UI", Cantarell, sans-serif;
    }
    .peats-reader {
      max-width: 46rem;
      margin: 0 auto;
      padding: 56px 24px 96px;
      font-size: 1.13rem;
      line-height: 1.72;
    }
    .peats-reader h1 {
      font-size: 2.15rem;
      line-height: 1.16;
      letter-spacing: -0.02em;
      margin: 0 0 10px;
    }
    .peats-reader .peats-reader-source {
      margin: 0 0 34px;
      font-size: 0.82rem;
      color: #6d6d72;
      word-break: break-all;
    }
    .peats-reader img, .peats-reader video {
      max-width: 100%;
      height: auto;
      border-radius: 10px;
    }
    .peats-reader figure { margin: 28px 0; }
    .peats-reader figcaption { font-size: 0.85rem; color: #6d6d72; margin-top: 8px; }
    .peats-reader a { color: #b5430b; }
    .peats-reader pre, .peats-reader code {
      font-family: "JetBrains Mono", "Source Code Pro", monospace;
      font-size: 0.92em;
    }
    .peats-reader pre {
      background: rgba(0, 0, 0, 0.05);
      padding: 14px 16px;
      border-radius: 10px;
      overflow-x: auto;
    }
    .peats-reader blockquote {
      margin: 24px 0;
      padding-left: 18px;
      border-left: 3px solid #d6d3ce;
      color: #4c4d52;
    }
    .peats-reader h2, .peats-reader h3 { line-height: 1.25; margin: 34px 0 12px; }
    @media (prefers-color-scheme: dark) {
      body { background: #17181a; color: #ececee; }
      .peats-reader .peats-reader-source { color: #9a9aa2; }
      .peats-reader a { color: #ff8a5c; }
      .peats-reader pre { background: rgba(255, 255, 255, 0.07); }
      .peats-reader blockquote { border-left-color: #3a3b40; color: #b9b9c0; }
    }
  `;
  head.appendChild(style);

  const article = document.createElement('article');
  article.className = 'peats-reader';
  article.innerHTML = '<h1>' + escape(title) + '</h1>'
    + '<p class="peats-reader-source">' + escape(source) + '</p>'
    + clone.innerHTML;

  document.body.replaceChildren(article);
  window.scrollTo(0, 0);
  return 'ok';
})();
"#;
